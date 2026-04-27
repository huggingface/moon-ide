//! Hand-rolled Slack Web API client.
//!
//! Endpoints implemented so far:
//! - `auth.test` — identify the human whose token we hold (11.0)
//! - `conversations.list?types=im` — list the user's open DMs (11.0)
//! - `users.info` — pull `is_bot` + display metadata for each DM partner (11.0)
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

use moon_protocol::slack::{SlackBotProfile, SlackIdentity, SlackMessage, SlackSession};
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

/// Length cap for the session-row preview text. Slack does not truncate
/// for us; this keeps long parent messages from blowing out the panel
/// width before the UI's `text-overflow: ellipsis` kicks in.
pub const PREVIEW_MAX_CHARS: usize = 80;

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
/// attachments, blocks…) and we'll grow this struct as later phases
/// need them.
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
}

#[derive(Debug, Deserialize, Serialize)]
struct EditedMeta {
	ts: String,
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
	let preview = preview_from(msg.text.as_deref().unwrap_or(""));
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
	SlackMessage {
		ts: msg.ts,
		user_id: msg.user,
		text: msg.text.unwrap_or_default(),
		edited_ts: msg.edited.map(|e| e.ts),
		is_bot,
	}
}

/// Single-line preview, capped at [`PREVIEW_MAX_CHARS`]. Newlines
/// collapse to spaces (the panel renders one row per session) and
/// runs of whitespace are squashed so "user pressed enter twice"
/// doesn't leave a wide gap. Truncation appends a single `…` so the
/// row reads like "first line of the message…".
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
	fn preview_truncates_and_collapses_whitespace() {
		assert_eq!(preview_from("hello world"), "hello world");
		assert_eq!(preview_from("hello\n\nworld"), "hello world");
		assert_eq!(preview_from("  hello   world  "), "hello world");
		assert_eq!(preview_from(""), "");

		let long = "a".repeat(200);
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
