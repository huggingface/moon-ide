//! Slack chat panel shapes shared between backend and frontend.
//!
//! See `specs/slack-chat.md` for the architecture. Phase 11.0 only
//! exposes the connect/disconnect surface — sessions, threads, and
//! messages join in 11.1+.
//!
//! Per AGENTS.md "no premature migrations": these structs change
//! freely until the roadmap is done — there are no aliases or
//! version-tolerant readers.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Result of `auth.test`. Identifies the human whose token we hold.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackIdentity {
	/// Slack user ID of the human (e.g. `U01ABCDE`).
	pub user_id: String,
	/// Slack username (`real_name` field — what shows next to the avatar).
	pub user_name: String,
	/// Workspace ID (e.g. `T01ABCDE`).
	pub team_id: String,
	/// Workspace display name (e.g. `Hugging Face`).
	pub team: String,
	/// Workspace base URL (e.g. `https://huggingface.slack.com/`).
	pub url: String,
}

/// A bot we can DM, discovered by scanning the authenticated user's
/// own DM list. See `specs/slack-chat.md#bot-resolution` for why we
/// don't search the workspace directory.
///
/// Persisted (when the user picks one) in `AppState.slack.active_bot`
/// — non-secret, machine-local — so the picker doesn't reappear on
/// every launch.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackBotProfile {
	/// Slack user ID (e.g. `U0AGNMHHQ1H`).
	pub user_id: String,
	/// DM channel between the human and this bot. Comes straight from
	/// `conversations.list?types=im` — Slack returns a stable channel
	/// per `(human, bot)` pair so this never changes.
	pub dm_channel_id: String,
	/// Slack `name` (e.g. `moon_bot`). Stable username slug.
	pub username: String,
	/// `real_name` (e.g. `Moon Bot`). Empty string if Slack returns nothing.
	pub real_name: String,
	/// `profile.display_name` when set; otherwise `None`.
	pub display_name: Option<String>,
	/// Avatar URL (`profile.image_72` preferred, falling back to 48 / 24).
	pub image_url: Option<String>,
}

/// Connection status for the chat panel. The frontend polls this on
/// startup and after every `slack_set_token` / `slack_clear_token`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackStatus {
	pub connected: bool,
	pub identity: Option<SlackIdentity>,
}

/// One row in the chat panel's session list — a top-level DM message
/// that has (or could have) a thread underneath. Sessions correspond
/// 1:1 to threads in Slack: posting a top-level message starts a new
/// session, and bot replies live inside that thread (`thread_ts`).
///
/// Returned newest-first. The preview text is truncated server-side so
/// the UI doesn't have to. `latest_ts` is what we use to render
/// "2 min ago" and to drive future polling cadence.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackSession {
	/// `thread_ts` of the top-level message. Doubles as the session's
	/// stable ID — also equal to the parent message's own `ts` in
	/// Slack's data model.
	pub thread_ts: String,
	/// Timestamp of the most recent activity on the thread (the
	/// parent's own `ts` if there are no replies yet, otherwise the
	/// last reply's `ts`). Drives "2 min ago" and the
	/// future cadence ladder.
	pub latest_ts: String,
	/// First ~80 chars of the parent message, single-line. Empty if
	/// the parent is image-only / file-only (Slack returns no text in
	/// that case).
	pub preview: String,
	/// Number of replies (excluding the parent). 0 means "fresh
	/// session, bot hasn't replied yet".
	pub reply_count: u32,
	/// Slack user ID who posted the parent. Usually us, but can be
	/// the bot (e.g. an automated daily summary). Used by the panel
	/// to render a tiny "you" / bot label on each session row.
	pub user_id: Option<String>,
}

/// Minimal user record for resolving `<@U12345>` mentions in
/// rendered Slack mrkdwn. Cached per-user on the frontend; the
/// backend just wraps `users.info`.
///
/// `display_name` is what Slack shows next to the avatar; falls back
/// to `real_name` and finally `name` (the username slug). Callers
/// should prefer that order — see `bestLabel` on the frontend.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackUserSummary {
	pub user_id: String,
	pub name: String,
	pub real_name: String,
	pub display_name: Option<String>,
	pub is_bot: bool,
}

/// One message inside a thread (or the parent itself). Phase 11.1
/// renders these read-only as bubbles; 11.2 will diff successive
/// snapshots to detect edits via `edited_ts`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackMessage {
	/// Stable Slack message ID. Sortable lexicographically.
	pub ts: String,
	/// Author's Slack user ID. `None` only for system / unknown
	/// messages, which we still render but flag as "unknown sender".
	pub user_id: Option<String>,
	/// Plain text body. Slack's mrkdwn dialect is *not* parsed here —
	/// 11.4 swaps in proper rendering. For now it's literally what
	/// Slack returned, with `\n` preserved.
	pub text: String,
	/// `edited.ts` when the message has been edited. Lets the UI
	/// surface "(edited)" and lets 11.2's polling diff identify
	/// changes without comparing the full body.
	pub edited_ts: Option<String>,
	/// True when the author is a bot (per Slack's `bot_id` field on
	/// the message). Used for bubble alignment + colour. Doesn't
	/// distinguish *which* bot.
	pub is_bot: bool,
}
