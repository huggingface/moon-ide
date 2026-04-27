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
