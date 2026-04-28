//! Hand-rolled Slack Web API client.
//!
//! Endpoints implemented so far:
//! - `auth.test` — identify the human whose token we hold (11.0)
//! - `conversations.list?types=im` — list the user's open DMs (11.0)
//! - `users.info` — pull `is_bot` + display metadata for each DM partner (11.0)
//!   and resolve `<@U…>` mentions in rendered messages (11.1.1)
//! - `conversations.history` — top-level DM messages = sessions (11.1)
//! - `conversations.replies` — every message inside a thread (11.1)
//!
//! See [`specs/slack-chat.md`](../../specs/slack-chat.md) for the
//! "scan-the-user's-own-DMs" approach and why we don't paginate
//! `users.list` over the whole workspace.
//!
//! Per `specs/slack-chat.md`, we deliberately avoid `slack-morphism` /
//! `slack_api`: those crates carry OAuth flows, signing, and a type
//! universe we don't use, all on the user-token critical path.

use moon_protocol::slack::{SlackAction, SlackBotProfile, SlackIdentity, SlackMessage, SlackSession, SlackUserSummary};
use serde::{Deserialize, Serialize};

use crate::error::SlackError;

const SLACK_API_BASE: &str = "https://slack.com/api";

/// Number of DMs we scan when looking for bots. Slack returns DMs in
/// recency order, so this is "the user's 50 most recently active
/// DMs". The cap is also surfaced in the UI (connect modal + picker
/// copy) — keep this value in sync with the strings in
/// `ChatConnectModal.svelte` and `ChatPanel.svelte`. See
/// `specs/slack-chat.md#cost` for why 50 is the right size.
pub const DM_SCAN_LIMIT: usize = 50;

/// Maximum number of top-level messages we pull from a DM channel
/// when populating the session list. One `conversations.history`
/// page; no follow-up cursor walks. 100 covers every real-world
/// "show me my recent moon-bot conversations" without an extra
/// round-trip — the user can scroll through them in the panel and
/// when 100 isn't enough, we add pagination on demand.
pub const SESSION_HISTORY_LIMIT: usize = 100;

/// Hard cap on the number of message previews we ask Slack for in one
/// `conversations.replies` call. Slack's per-thread default is 1000;
/// we pull fewer to keep the first-paint snappy. Threads longer than
/// this get truncated to the most recent N — fine for v1 read-only
/// chat where we mostly care about the latest exchange.
pub const THREAD_REPLY_LIMIT: usize = 200;

/// Transport-safety cap on the session-row preview *body*. Visual
/// truncation belongs to the panel — it's CSS `line-clamp: 2` on the
/// rendered, mrkdwn-flattened output, *after* `<https://…>` /
/// `<@U…|alice>` tokens have become plain text. Truncating the raw
/// mrkdwn here used to cut mid-token at 80 chars and then leak a
/// dangling `<` into the preview (or, post-`trimDanglingAngle`,
/// silently swallow the link). 500 chars is well over any typical
/// thread-starter (a status line, a one-paragraph prompt) but small
/// enough that we're not shipping the bot's full essay just to
/// render a 2-line summary.
pub const PREVIEW_MAX_CHARS: usize = 500;

#[derive(Clone)]
pub struct SlackClient {
	http: reqwest::Client,
	token: String,
}

impl SlackClient {
	pub fn new(token: String) -> Result<Self, SlackError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(|err| SlackError::Transport(err.to_string()))?;
		Ok(Self { http, token })
	}

	/// `auth.test` — succeeds iff the token is valid; returns the
	/// human's identity. Used for the connect handshake and the
	/// periodic "still connected?" check.
	pub async fn auth_test(&self) -> Result<SlackIdentity, SlackError> {
		let body: AuthTestResponse = self.call("auth.test", &[]).await?;
		Ok(SlackIdentity {
			user_id: body.user_id,
			user_name: body.user,
			team_id: body.team_id,
			team: body.team,
			url: body.url,
		})
	}

	/// Scan the authenticated user's [`DM_SCAN_LIMIT`] most recently
	/// active DMs and return every partner that's a bot.
	///
	/// Implementation: one `conversations.list?types=im&limit=50` call
	/// (Slack returns DMs newest-first), then `users.info` per
	/// partner — `users.info` is tier-4 (100+ /min) so 50 sequential
	/// calls finish in ~10–20 s in the typical case and well within
	/// the rate-limit budget. Filters to `is_bot && !deleted`.
	///
	/// Returns matches in the same order Slack returned the DMs, so
	/// the bot the user has talked to most recently sorts toward the
	/// top of the picker.
	///
	/// Bots living in DMs older than the 50th will not be discovered
	/// — by design. The number is surfaced upfront in the connect
	/// modal so the user knows to bump older bot DMs by sending a
	/// quick "hi" from regular Slack before connecting.
	pub async fn list_dm_bots(&self) -> Result<Vec<SlackBotProfile>, SlackError> {
		let ims = self.list_im_channels().await?;
		let mut bots = Vec::with_capacity(ims.len() / 4);
		for im in ims {
			let user = match self.users_info(&im.user).await {
				Ok(user) => user,
				Err(SlackError::Api { code, .. }) if code == "user_not_found" => {
					tracing::debug!(user_id = %im.user, "users.info returned user_not_found; skipping");
					continue;
				}
				Err(err) => return Err(err),
			};
			if !user.is_bot.unwrap_or(false) || user.deleted.unwrap_or(false) {
				continue;
			}
			bots.push(SlackBotProfile {
				user_id: user.id,
				dm_channel_id: im.id,
				username: user.name.unwrap_or_default(),
				real_name: user.real_name.unwrap_or_default(),
				display_name: user
					.profile
					.as_ref()
					.and_then(|p| p.display_name.clone())
					.filter(|s| !s.is_empty()),
				image_url: user.profile.as_ref().and_then(|p| {
					p.image_72
						.clone()
						.or_else(|| p.image_48.clone())
						.or_else(|| p.image_24.clone())
				}),
			});
		}
		Ok(bots)
	}

	/// Top-level DM messages, mapped to [`SlackSession`]. Anything
	/// inside a thread (`thread_ts != ts`) is filtered out — those are
	/// rendered via [`Self::get_thread`] when the user picks a session.
	///
	/// Slack returns `conversations.history` newest-first, which is
	/// exactly the order the panel wants, so we preserve it. We don't
	/// paginate: 100 sessions is plenty for v1, and the cost of a
	/// cursor walk on every panel mount isn't worth the 1% of users
	/// who'd benefit. Add it back when somebody asks.
	pub async fn list_sessions(&self, channel: &str) -> Result<Vec<SlackSession>, SlackError> {
		let limit = SESSION_HISTORY_LIMIT.to_string();
		let body: ConversationsHistoryResponse = self
			.call(
				"conversations.history",
				&[("channel", channel), ("limit", limit.as_str())],
			)
			.await?;
		let sessions = body.messages.into_iter().filter(is_top_level).map(to_session).collect();
		Ok(sessions)
	}

	/// Every message inside a thread, parent included, in chronological
	/// order. Returned with parent first because that's the natural
	/// reading order for chat — same shape Slack returns from
	/// `conversations.replies`.
	pub async fn get_thread(&self, channel: &str, thread_ts: &str) -> Result<Vec<SlackMessage>, SlackError> {
		let limit = THREAD_REPLY_LIMIT.to_string();
		let body: ConversationsRepliesResponse = self
			.call(
				"conversations.replies",
				&[("channel", channel), ("ts", thread_ts), ("limit", limit.as_str())],
			)
			.await?;
		Ok(body.messages.into_iter().map(to_message).collect())
	}

	/// Look up one user by ID and return the trimmed summary used to
	/// render `<@U…>` mentions. Wraps `users.info` and folds the
	/// nested `profile.display_name` / `profile.real_name` fields so
	/// the frontend doesn't have to know Slack's data model.
	///
	/// Frontend caches the result per user_id, so this method is
	/// expected to fire at most once per distinct mentioned user per
	/// session.
	pub async fn resolve_user(&self, user_id: &str) -> Result<SlackUserSummary, SlackError> {
		let user = self.users_info(user_id).await?;
		let display_name = user
			.profile
			.as_ref()
			.and_then(|p| p.display_name.clone())
			.filter(|s| !s.is_empty());
		let real_name = user
			.real_name
			.clone()
			.or_else(|| user.profile.as_ref().and_then(|p| p.real_name.clone()))
			.unwrap_or_default();
		Ok(SlackUserSummary {
			user_id: user.id,
			name: user.name.unwrap_or_default(),
			real_name,
			display_name,
			is_bot: user.is_bot.unwrap_or(false),
		})
	}

	async fn list_im_channels(&self) -> Result<Vec<ImChannel>, SlackError> {
		let limit = DM_SCAN_LIMIT.to_string();
		let body: ConversationsListResponse = self
			.call("conversations.list", &[("types", "im"), ("limit", limit.as_str())])
			.await?;
		Ok(body.channels)
	}

	async fn users_info(&self, user_id: &str) -> Result<SlackUser, SlackError> {
		let body: UsersInfoResponse = self.call("users.info", &[("user", user_id)]).await?;
		Ok(body.user)
	}

	async fn call<R: for<'de> Deserialize<'de>>(&self, method: &str, params: &[(&str, &str)]) -> Result<R, SlackError> {
		let url = format!("{SLACK_API_BASE}/{method}");
		let response = self
			.http
			.get(&url)
			.bearer_auth(&self.token)
			.query(params)
			.send()
			.await
			.map_err(|err| SlackError::Transport(err.to_string()))?;

		let status = response.status();
		let bytes = response
			.bytes()
			.await
			.map_err(|err| SlackError::Transport(err.to_string()))?;
		if !status.is_success() {
			return Err(SlackError::Http {
				status: status.as_u16(),
				body: String::from_utf8_lossy(&bytes).into_owned(),
			});
		}

		let envelope: SlackEnvelope = serde_json::from_slice(&bytes).map_err(|err| SlackError::Decode(err.to_string()))?;
		if !envelope.ok {
			return Err(SlackError::Api {
				method: method.to_string(),
				code: envelope.error.unwrap_or_else(|| "unknown".to_string()),
				needed: envelope.needed,
			});
		}
		serde_json::from_slice(&bytes).map_err(|err| SlackError::Decode(err.to_string()))
	}
}

// --- Wire types -----------------------------------------------------------
//
// Kept private; only the protocol-level types in `moon-protocol::slack`
// cross the IPC boundary. Slack response shapes are *much* wider than
// what we need, so we deserialise only the fields we touch.

#[derive(Debug, Deserialize)]
struct SlackEnvelope {
	ok: bool,
	#[serde(default)]
	error: Option<String>,
	/// Slack's `missing_scope` envelope tells us which scope it
	/// wanted (e.g. `needed: "im:history"`). Captured here so the
	/// error surface can show "Slack API error: missing_scope (need
	/// im:history)" instead of the bare code.
	#[serde(default)]
	needed: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthTestResponse {
	user_id: String,
	user: String,
	team_id: String,
	team: String,
	url: String,
}

#[derive(Debug, Deserialize)]
struct ConversationsListResponse {
	channels: Vec<ImChannel>,
}

/// One entry of `conversations.list?types=im`. The `user` field is
/// the *other* party in the DM (never the authenticated user).
#[derive(Debug, Deserialize)]
struct ImChannel {
	id: String,
	user: String,
}

#[derive(Debug, Deserialize)]
struct UsersInfoResponse {
	user: SlackUser,
}

/// Slack `objects.User`. Marked `Serialize` only for test fixtures —
/// it never leaves this crate.
#[derive(Debug, Deserialize, Serialize)]
struct SlackUser {
	id: String,
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	real_name: Option<String>,
	#[serde(default)]
	is_bot: Option<bool>,
	#[serde(default)]
	deleted: Option<bool>,
	#[serde(default)]
	profile: Option<SlackUserProfile>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SlackUserProfile {
	#[serde(default)]
	display_name: Option<String>,
	#[serde(default)]
	real_name: Option<String>,
	#[serde(default)]
	image_24: Option<String>,
	#[serde(default)]
	image_48: Option<String>,
	#[serde(default)]
	image_72: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ConversationsHistoryResponse {
	messages: Vec<RawMessage>,
}

#[derive(Debug, Deserialize)]
struct ConversationsRepliesResponse {
	messages: Vec<RawMessage>,
}

/// Subset of Slack's `objects.Message`. Only the fields we touch are
/// deserialised; the wire format is much wider (subtypes, reactions,
/// attachments…) and we'll grow this struct as later phases need them.
#[derive(Debug, Deserialize, Serialize)]
struct RawMessage {
	ts: String,
	#[serde(default)]
	thread_ts: Option<String>,
	#[serde(default)]
	user: Option<String>,
	#[serde(default)]
	bot_id: Option<String>,
	#[serde(default)]
	text: Option<String>,
	#[serde(default)]
	subtype: Option<String>,
	#[serde(default)]
	reply_count: Option<u32>,
	#[serde(default)]
	latest_reply: Option<String>,
	#[serde(default)]
	edited: Option<EditedMeta>,
	/// Block Kit content. Bots that use rich layouts (moon-bot,
	/// Cursor, GitHub) put the *real* message body here; the `text`
	/// field above is just a notification fallback that often
	/// strips newlines and structure. We extract a Slack-mrkdwn
	/// representation from the supported block types and use that
	/// in preference to `text` when present.
	#[serde(default)]
	blocks: Option<Vec<RawBlock>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct EditedMeta {
	ts: String,
}

/// Subset of Slack's [Block Kit][1] block types. We only deserialise
/// the shapes we render; everything else (image, header, context,
/// rich_text, table, …) falls into `Unknown` and contributes nothing.
///
/// `markdown` blocks are forwarded as-is even though they carry
/// CommonMark, not Slack mrkdwn. The frontend tokenizer will render
/// most of it correctly (text, links, code) but not `**bold**` /
/// `__italic__` / fenced language tags. Acceptable while no real
/// message hits the long-message path; revisit when someone reports
/// a poorly rendered long bot reply.
///
/// [1]: https://api.slack.com/reference/block-kit/blocks
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawBlock {
	Section {
		#[serde(default)]
		text: Option<RawBlockText>,
	},
	Markdown {
		text: String,
	},
	Divider,
	/// Footer button row (moon-bot's "Response" / "Download" /
	/// "Session" links). We only extract URL-bearing buttons —
	/// interactive ones need a `block_actions` callback that this
	/// read-only panel can't dispatch.
	Actions {
		#[serde(default)]
		elements: Vec<RawAction>,
	},
	#[serde(other)]
	Unknown,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawBlockText {
	#[serde(rename = "type")]
	kind: String,
	text: String,
}

/// Subset of Slack's [block element][1] types that can appear inside
/// an `actions` block. Only `button` is recognised — other element
/// types (date pickers, selects, overflow menus) need server-side
/// callbacks we don't have. Captured as `Unknown` and dropped.
///
/// [1]: https://api.slack.com/reference/block-kit/block-elements
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawAction {
	Button {
		text: RawBlockText,
		/// Set on link buttons. Absent on interactive buttons (which
		/// carry `value` instead) — those we drop.
		#[serde(default)]
		url: Option<String>,
		#[serde(default)]
		style: Option<String>,
	},
	#[serde(other)]
	Unknown,
}

/// Top-level test: parent of a thread (or a thread-less message).
/// Slack sets `thread_ts` to the parent's own `ts` on every message
/// in a thread, including the parent itself, so the parent passes
/// (`ts == thread_ts`) and the replies don't.
fn is_top_level(msg: &RawMessage) -> bool {
	match &msg.thread_ts {
		None => true,
		Some(thread_ts) => thread_ts == &msg.ts,
	}
}

fn to_session(msg: RawMessage) -> SlackSession {
	// Same blocks-vs-text precedence as `to_message`: bots put the
	// real content in blocks, the `text` fallback often loses
	// newlines and structure.
	let body = text_from_blocks(msg.blocks.as_deref()).unwrap_or_else(|| msg.text.unwrap_or_default());
	let preview = preview_from(&body);
	let reply_count = msg.reply_count.unwrap_or(0);
	let latest_ts = msg.latest_reply.unwrap_or_else(|| msg.ts.clone());
	SlackSession {
		thread_ts: msg.ts,
		latest_ts,
		preview,
		reply_count,
		user_id: msg.user,
	}
}

fn to_message(msg: RawMessage) -> SlackMessage {
	let is_bot = msg.bot_id.is_some();
	let blocks = msg.blocks.as_deref();
	let text = text_from_blocks(blocks).unwrap_or_else(|| msg.text.unwrap_or_default());
	let actions = actions_from_blocks(blocks);
	SlackMessage {
		ts: msg.ts,
		user_id: msg.user,
		text,
		edited_ts: msg.edited.map(|e| e.ts),
		is_bot,
		actions,
	}
}

/// Build a single Slack-mrkdwn string from the Block Kit blocks we
/// know how to render. Returns `None` if no block contributed any
/// text, in which case the caller falls back to the raw `text` field.
///
/// Each block becomes one paragraph separated by a blank line — the
/// same vertical rhythm Slack uses in its own renderer. Newlines
/// inside a block (e.g. inside a `section` mrkdwn body) are preserved
/// as-is.
fn text_from_blocks(blocks: Option<&[RawBlock]>) -> Option<String> {
	let blocks = blocks?;
	let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
	for block in blocks {
		match block {
			RawBlock::Section { text: Some(text) } if text.kind == "mrkdwn" => {
				parts.push(text.text.clone());
			}
			RawBlock::Markdown { text } => {
				// CommonMark, not Slack mrkdwn. Forwarded as-is for
				// now — the frontend tokenizer will render it
				// best-effort. Long messages with `**bold**` will
				// show literal asterisks; deferred until needed.
				parts.push(text.clone());
			}
			RawBlock::Divider => {
				parts.push("———".to_string());
			}
			_ => {}
		}
	}
	if parts.is_empty() {
		return None;
	}
	Some(parts.join("\n\n"))
}

/// Pull link buttons out of every `actions` block in the message.
/// Order is preserved across blocks and within each block (Slack
/// renders them left-to-right), so the frontend can render them as
/// one flat row underneath the body.
///
/// Buttons without a `url` are dropped — see [`SlackAction`] for the
/// rationale.
fn actions_from_blocks(blocks: Option<&[RawBlock]>) -> Vec<SlackAction> {
	let Some(blocks) = blocks else {
		return Vec::new();
	};
	let mut out = Vec::new();
	for block in blocks {
		let RawBlock::Actions { elements } = block else {
			continue;
		};
		for element in elements {
			let RawAction::Button {
				text,
				url: Some(url),
				style,
			} = element
			else {
				continue;
			};
			out.push(SlackAction {
				label: text.text.clone(),
				url: url.clone(),
				style: style.clone(),
			});
		}
	}
	out
}

/// Single-line preview body, capped at [`PREVIEW_MAX_CHARS`]. Newlines
/// collapse to spaces (the panel renders one row per session) and
/// runs of whitespace are squashed so "user pressed enter twice"
/// doesn't leave a wide gap.
///
/// We deliberately *over-shoot* the visible width: visual truncation is
/// the frontend's job — it parses the mrkdwn, flattens
/// `<https://…|label>` to `label`, resolves `<@U…>` mentions, then
/// CSS `line-clamp: 2` cuts to the actual panel width. Truncating
/// here would cut mid-token (`<https://…cu` → JS dangling-trim → no
/// link visible at all). The 500-char cap is a transport safety net
/// for runaway bot replies, not a UI knob.
fn preview_from(raw: &str) -> String {
	let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
	if collapsed.chars().count() <= PREVIEW_MAX_CHARS {
		return collapsed;
	}
	let mut out: String = collapsed.chars().take(PREVIEW_MAX_CHARS).collect();
	out.push('…');
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn deserializes_im_list() {
		let json = r#"{
			"ok": true,
			"channels": [
				{ "id": "D01", "user": "U01" },
				{ "id": "D02", "user": "U02" }
			]
		}"#;
		let r: ConversationsListResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.channels.len(), 2);
		assert_eq!(r.channels[0].id, "D01");
		assert_eq!(r.channels[1].user, "U02");
	}

	#[test]
	fn deserializes_users_info_bot() {
		let json = r#"{
			"ok": true,
			"user": {
				"id": "U0AGNMHHQ1H",
				"name": "moon_bot",
				"real_name": "Moon Bot",
				"is_bot": true,
				"deleted": false,
				"profile": {
					"display_name": "",
					"real_name": "Moon Bot",
					"image_72": "https://example.com/avatar.png"
				}
			}
		}"#;
		let r: UsersInfoResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.user.id, "U0AGNMHHQ1H");
		assert_eq!(r.user.is_bot, Some(true));
		assert_eq!(r.user.real_name.as_deref(), Some("Moon Bot"));
	}

	#[test]
	fn deserializes_users_info_human() {
		let json = r#"{
			"ok": true,
			"user": {
				"id": "U02SCR6D6U8",
				"name": "eliott",
				"real_name": "Eli Ott",
				"is_bot": false,
				"deleted": false
			}
		}"#;
		let r: UsersInfoResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.user.is_bot, Some(false));
		assert!(r.user.profile.is_none());
	}

	#[test]
	fn deserializes_history_and_filters_top_level() {
		// Real Slack response shape (trimmed). Two top-level messages
		// (one with a reply, one without) and one in-thread reply
		// that should be filtered out by `list_sessions`.
		let json = r#"{
			"ok": true,
			"messages": [
				{ "ts": "1700000003.000300", "user": "U_BOT", "thread_ts": "1700000001.000100", "text": "in thread", "bot_id": "B01" },
				{ "ts": "1700000002.000200", "user": "U_HUMAN", "text": "fresh session" },
				{ "ts": "1700000001.000100", "user": "U_HUMAN", "thread_ts": "1700000001.000100", "text": "list new files", "reply_count": 1, "latest_reply": "1700000003.000300" }
			]
		}"#;
		let r: ConversationsHistoryResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.messages.len(), 3);
		let sessions: Vec<_> = r.messages.into_iter().filter(is_top_level).map(to_session).collect();
		assert_eq!(sessions.len(), 2);
		// Newest-first preserved (Slack already orders this way).
		assert_eq!(sessions[0].thread_ts, "1700000002.000200");
		assert_eq!(sessions[0].latest_ts, "1700000002.000200");
		assert_eq!(sessions[0].reply_count, 0);
		assert_eq!(sessions[1].thread_ts, "1700000001.000100");
		// `latest_reply` overrides parent ts when present.
		assert_eq!(sessions[1].latest_ts, "1700000003.000300");
		assert_eq!(sessions[1].reply_count, 1);
		assert_eq!(sessions[1].preview, "list new files");
	}

	#[test]
	fn deserializes_replies_with_edits_and_bots() {
		let json = r#"{
			"ok": true,
			"messages": [
				{ "ts": "1700000001.000100", "user": "U_HUMAN", "thread_ts": "1700000001.000100", "text": "hi" },
				{ "ts": "1700000002.000200", "user": "U_BOT", "thread_ts": "1700000001.000100", "text": "hello world", "bot_id": "B01", "edited": { "user": "U_BOT", "ts": "1700000005.000500" } }
			]
		}"#;
		let r: ConversationsRepliesResponse = serde_json::from_str(json).unwrap();
		let messages: Vec<_> = r.messages.into_iter().map(to_message).collect();
		assert_eq!(messages.len(), 2);
		assert_eq!(messages[0].user_id.as_deref(), Some("U_HUMAN"));
		assert!(!messages[0].is_bot);
		assert_eq!(messages[0].edited_ts, None);
		assert!(messages[1].is_bot);
		assert_eq!(messages[1].edited_ts.as_deref(), Some("1700000005.000500"));
	}

	#[test]
	fn block_text_overrides_text_field_when_blocks_present() {
		// Real moon-bot shape: `text` is a flattened notification
		// fallback (newlines lost), `blocks` carries the actual rich
		// content with newlines preserved. We must use the blocks.
		let json = r#"{
			"ts": "1700000002.000200",
			"user": "U_BOT",
			"thread_ts": "1700000001.000100",
			"text": "Root cause: foo. Fix: bar.",
			"bot_id": "B01",
			"blocks": [
				{ "type": "section", "text": { "type": "mrkdwn", "text": "*Root cause:* foo.\nstill same paragraph." } },
				{ "type": "section", "text": { "type": "mrkdwn", "text": "*Fix:* bar." } }
			]
		}"#;
		let raw: RawMessage = serde_json::from_str(json).unwrap();
		let msg = to_message(raw);
		assert_eq!(msg.text, "*Root cause:* foo.\nstill same paragraph.\n\n*Fix:* bar.");
	}

	#[test]
	fn falls_back_to_text_when_blocks_absent() {
		let json = r#"{
			"ts": "1700000003.000300",
			"user": "U_HUMAN",
			"thread_ts": "1700000001.000100",
			"text": "plain typed message"
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "plain typed message");
	}

	#[test]
	fn falls_back_to_text_when_no_block_contributed() {
		// Slack auto-generates rich_text blocks for human typers; we
		// don't render those (yet), so the typed `text` field has to
		// win.
		let json = r#"{
			"ts": "1700000004.000400",
			"user": "U_HUMAN",
			"thread_ts": "1700000001.000100",
			"text": "typed by a human",
			"blocks": [
				{ "type": "rich_text", "elements": [] }
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "typed by a human");
	}

	#[test]
	fn divider_renders_as_em_dashes() {
		let json = r#"{
			"ts": "1700000005.000500",
			"bot_id": "B01",
			"blocks": [
				{ "type": "section", "text": { "type": "mrkdwn", "text": "before" } },
				{ "type": "divider" },
				{ "type": "section", "text": { "type": "mrkdwn", "text": "after" } }
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "before\n\n———\n\nafter");
	}

	#[test]
	fn skips_unsupported_blocks_silently() {
		// `image` / `header` / `context` contribute nothing to the
		// rendered text right now — either separately rendered later
		// (Phase 11.4+) or genuinely cosmetic.
		let json = r#"{
			"ts": "1700000006.000600",
			"bot_id": "B01",
			"blocks": [
				{ "type": "section", "text": { "type": "mrkdwn", "text": "hello" } },
				{ "type": "image", "image_url": "https://example.com/x.png", "alt_text": "x" },
				{ "type": "header", "text": { "type": "plain_text", "text": "title" } }
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "hello");
	}

	#[test]
	fn ignores_section_with_plain_text_payload() {
		// Section blocks can carry `plain_text` (no formatting); we
		// only render mrkdwn — plain_text is rare and would conflict
		// with the renderer's mrkdwn assumptions.
		let json = r#"{
			"ts": "1700000007.000700",
			"bot_id": "B01",
			"blocks": [
				{ "type": "section", "text": { "type": "plain_text", "text": "ignored" } },
				{ "type": "section", "text": { "type": "mrkdwn", "text": "kept" } }
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "kept");
	}

	#[test]
	fn session_preview_reads_blocks_not_text_fallback() {
		// Same precedence as `to_message`: a bot session row should
		// summarise the rich blocks, not the flattened text field.
		let json = r#"{
			"ts": "1700000010.001000",
			"thread_ts": "1700000010.001000",
			"user": "U_BOT",
			"bot_id": "B01",
			"text": "ignored fallback",
			"blocks": [
				{ "type": "section", "text": { "type": "mrkdwn", "text": "real preview body" } }
			]
		}"#;
		let raw: RawMessage = serde_json::from_str(json).unwrap();
		let session = to_session(raw);
		assert_eq!(session.preview, "real preview body");
	}

	#[test]
	fn extracts_link_buttons_from_actions_block() {
		// moon-bot's footer: three URL buttons. We surface them as
		// `SlackAction` rows under the message body. Order and style
		// are preserved.
		let json = r#"{
			"ts": "1700000020.002000",
			"bot_id": "B01",
			"blocks": [
				{ "type": "section", "text": { "type": "mrkdwn", "text": "done." } },
				{
					"type": "actions",
					"elements": [
						{ "type": "button", "text": { "type": "plain_text", "text": "Response" }, "url": "https://hf.co/r" },
						{ "type": "button", "text": { "type": "plain_text", "text": "Download" }, "url": "https://hf.co/d" },
						{ "type": "button", "style": "primary", "text": { "type": "plain_text", "text": "Session" }, "url": "https://hf.co/s" }
					]
				}
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "done.");
		assert_eq!(msg.actions.len(), 3);
		assert_eq!(msg.actions[0].label, "Response");
		assert_eq!(msg.actions[0].url, "https://hf.co/r");
		assert_eq!(msg.actions[0].style, None);
		assert_eq!(msg.actions[2].label, "Session");
		assert_eq!(msg.actions[2].style.as_deref(), Some("primary"));
	}

	#[test]
	fn drops_interactive_buttons_without_url() {
		// `value`-only buttons (interactive — would post
		// block_actions) can't be dispatched from a read-only panel.
		// Drop them, keep the link buttons in the same row.
		let json = r#"{
			"ts": "1700000021.002100",
			"bot_id": "B01",
			"blocks": [
				{
					"type": "actions",
					"elements": [
						{ "type": "button", "text": { "type": "plain_text", "text": "Approve" }, "value": "approve" },
						{ "type": "button", "text": { "type": "plain_text", "text": "Open" }, "url": "https://hf.co/o" }
					]
				}
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.actions.len(), 1);
		assert_eq!(msg.actions[0].label, "Open");
	}

	#[test]
	fn drops_unknown_action_element_types() {
		// Date pickers, selects, overflow menus etc. need server-side
		// callbacks; silently drop without breaking the rest of the
		// row.
		let json = r#"{
			"ts": "1700000022.002200",
			"bot_id": "B01",
			"blocks": [
				{
					"type": "actions",
					"elements": [
						{ "type": "datepicker", "action_id": "d", "initial_date": "2026-01-01" },
						{ "type": "button", "text": { "type": "plain_text", "text": "Open" }, "url": "https://hf.co/o" }
					]
				}
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.actions.len(), 1);
	}

	#[test]
	fn no_actions_yields_empty_vec() {
		let json = r#"{
			"ts": "1700000023.002300",
			"bot_id": "B01",
			"blocks": [
				{ "type": "section", "text": { "type": "mrkdwn", "text": "no buttons here" } }
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert!(msg.actions.is_empty());
	}

	#[test]
	fn forwards_markdown_block_text_unchanged() {
		// `markdown` blocks carry CommonMark, not Slack mrkdwn. We
		// hand them to the frontend as-is for now (deferred — see
		// `text_from_blocks` doc comment). `**bold**` will leak as
		// literal asterisks until conversion lands.
		let json = r#"{
			"ts": "1700000008.000800",
			"bot_id": "B01",
			"blocks": [
				{ "type": "markdown", "text": "**bold** and a [link](https://x.io)" }
			]
		}"#;
		let msg = to_message(serde_json::from_str(json).unwrap());
		assert_eq!(msg.text, "**bold** and a [link](https://x.io)");
	}

	#[test]
	fn preview_collapses_whitespace_without_visual_truncation() {
		// Whitespace collapse is the main job — keeps the panel from
		// spending two lines on a blank-line-separated paragraph.
		assert_eq!(preview_from("hello world"), "hello world");
		assert_eq!(preview_from("hello\n\nworld"), "hello world");
		assert_eq!(preview_from("  hello   world  "), "hello world");
		assert_eq!(preview_from(""), "");

		// A typical thread-starter (a one-paragraph status line plus a
		// long URL) sails under the transport cap and ships intact.
		// CSS `line-clamp: 2` is what the user actually sees; we
		// must not pre-cut and lose the `<…>` link envelope.
		let starter = "This space is in runtime error: <https://huggingface.co/spaces/CohereLabs/review-global-mmlu-lite>";
		assert_eq!(preview_from(starter), starter);
	}

	#[test]
	fn preview_caps_runaway_bodies_with_ellipsis() {
		// Transport safety net only — bot replies that dump their full
		// essay as the thread starter would otherwise ship 10 kB+.
		let long = "a".repeat(PREVIEW_MAX_CHARS + 200);
		let cut = preview_from(&long);
		assert_eq!(cut.chars().count(), PREVIEW_MAX_CHARS + 1);
		assert!(cut.ends_with('…'));
	}

	#[test]
	fn auth_failure_classification() {
		let api = SlackError::Api {
			method: "auth.test".into(),
			code: "invalid_auth".into(),
			needed: None,
		};
		assert!(api.is_auth_failure());

		let other = SlackError::Api {
			method: "auth.test".into(),
			code: "ratelimited".into(),
			needed: None,
		};
		assert!(!other.is_auth_failure());

		let transport = SlackError::Transport("dns failure".into());
		assert!(!transport.is_auth_failure());
	}

	#[test]
	fn missing_scope_envelope_carries_needed() {
		let json = r#"{
			"ok": false,
			"error": "missing_scope",
			"needed": "im:history",
			"provided": "users:read,im:read"
		}"#;
		let envelope: SlackEnvelope = serde_json::from_str(json).unwrap();
		assert!(!envelope.ok);
		assert_eq!(envelope.error.as_deref(), Some("missing_scope"));
		assert_eq!(envelope.needed.as_deref(), Some("im:history"));
	}

	#[test]
	fn api_error_display_mentions_needed_scope() {
		let err = SlackError::Api {
			method: "conversations.history".into(),
			code: "missing_scope".into(),
			needed: Some("im:history".into()),
		};
		assert_eq!(
			err.to_string(),
			"Slack API error (conversations.history): missing_scope (need im:history)"
		);

		let err = SlackError::Api {
			method: "auth.test".into(),
			code: "ratelimited".into(),
			needed: None,
		};
		assert_eq!(err.to_string(), "Slack API error (auth.test): ratelimited");
	}
}
