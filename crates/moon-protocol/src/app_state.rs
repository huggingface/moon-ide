//! Persisted, machine-local app state. Owned by moon-core's
//! [`moon_core::app_state`] storage layer, but the *shape* lives here so
//! the frontend and the backend agree on it byte-for-byte over IPC.
//!
//! There is deliberately no `Settings` type. Project-level code style
//! (indentation, EOL, charset) is delegated to `.editorconfig` from
//! Phase 1.5 onward; everything else moon-ide stores about a user is
//! per-machine and lives here. Per AGENTS.md "no premature migrations":
//! we change this struct freely until the roadmap is done — there are no
//! aliases or fallbacks.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::session::WorkspaceSession;
use crate::slack::SlackBotProfile;
use crate::theme::ThemeMode;

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct AppState {
	/// What was on screen at the last successful save: workspace, tabs,
	/// active pane. Restored verbatim on next launch when the workspace
	/// folder still exists.
	pub last_session: Option<WorkspaceSession>,
	/// Active UI theme. Per-machine; survives workspace switches.
	pub theme: ThemeMode,
	/// Per-machine, non-secret Slack panel state. The `xoxp-` token
	/// itself never lives here — it stays in the OS keyring (see
	/// `specs/slack-chat.md`).
	pub slack: SlackAppState,
}

/// Slack-specific slice of [`AppState`]. Only stores derived,
/// non-secret pointers so we can reload the chat panel on launch
/// without re-running the bot picker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct SlackAppState {
	/// Bot the user picked from the DM list. `None` means "show the
	/// picker on next chat-panel render". Cleared by an explicit "Pick
	/// a different bot" gesture or when `auth.test` reports the token
	/// is dead.
	pub active_bot: Option<SlackBotProfile>,
	/// Whether the right-side chat panel was open at last shutdown.
	/// We restore visibility on launch so users who live with the
	/// panel open don't have to re-open it every session. Defaults to
	/// `false` (closed) for first-run users, who shouldn't have a
	/// chat panel hijacking their workspace until they ask for it.
	pub panel_visible: bool,
}
