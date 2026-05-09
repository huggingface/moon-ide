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

use crate::next_edit::NextEditAppState;
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
	/// Bottom-panel visibility + height. Hosts service-log streams and
	/// (Phase 5) terminals, so it's worth restoring across launches —
	/// users tend to live with it open or closed and resent the panel
	/// re-jumping to a default height every restart.
	pub bottom_panel: BottomPanelAppState,
	/// Which surface — chat or coder — is mounted in the single
	/// right-side panel slot. `None` means the slot is closed. Chat
	/// and coder are mutually exclusive: opening one swaps the other
	/// out rather than stacking. Persisted so the user lands back in
	/// whichever surface they had open at last shutdown. Defaults to
	/// `None` (closed) for first-run users.
	pub right_panel: Option<RightPanelKind>,
	/// Per-machine coder state — picks up where the user left off
	/// without forcing them to navigate the sessions list again.
	pub coder: CoderAppState,
	/// Local llama.cpp autocomplete: managed `llama-server` spawn fields + optional external HTTP base.
	#[serde(default)]
	pub next_edit: NextEditAppState,
}

/// Surface mounted in the right-side panel. Chat and coder are
/// mutually exclusive; this enum encodes the pick. The slot can also
/// be closed entirely (`None` on `AppState::right_panel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RightPanelKind {
	Chat,
	Coder,
}

/// Slack-specific slice of [`AppState`]. Only stores derived,
/// non-secret pointers so we can reload the chat panel on launch
/// without re-running the bot picker. Panel visibility lives at the
/// top level on [`AppState::right_panel`] — chat and coder share
/// one slot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct SlackAppState {
	/// Bot the user picked from the DM list. `None` means "show the
	/// picker on next chat-panel render". Cleared by an explicit "Pick
	/// a different bot" gesture or when `auth.test` reports the token
	/// is dead.
	pub active_bot: Option<SlackBotProfile>,
	/// `thread_ts` of the session the user last had open in the chat
	/// panel. Restored on launch so reopening the panel jumps back
	/// into the same conversation. Cleared on bot switch and on
	/// disconnect — bot pick and active thread are coupled (the
	/// thread lives inside the bot's DM channel, ID encoded in
	/// `active_bot.dm_channel_id`).
	pub active_thread_ts: Option<String>,
}

/// Coder-specific slice of [`AppState`].
///
/// Only frontend-side affordance pointers — the actual session
/// content lives under
/// `<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/<id>.jsonl`,
/// not here. See [`crate::session`] / `crates/moon-coder/src/sessions.rs`
/// for the on-disk format.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct CoderAppState {
	/// Last-opened session id **per workspace folder**. Restored on
	/// launch when the user revisits a folder: the active folder's
	/// entry decides which session the panel mounts. Per the
	/// multi-session design, every project gets its own slot so a
	/// re-open of folder X resumes X's last session even if the
	/// user has worked in folder Y in between. Cleared per-folder
	/// when the matching session gets deleted; an `open_session`
	/// call updates that folder's entry.
	#[serde(default)]
	pub last_session_by_folder: std::collections::HashMap<String, String>,
}

/// Bottom-panel slice of [`AppState`].
///
/// Tab contents (open log streams, terminal sessions) are intentionally
/// not persisted: they're tied to running `docker compose logs -f`
/// processes that don't survive a launch, and re-spawning them blindly
/// on startup would surprise the user. Visibility + height are pure
/// chrome and safe to restore.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct BottomPanelAppState {
	/// Whether the bottom panel was open at last shutdown. Defaults to
	/// `false` — first-run users shouldn't have an empty panel
	/// occupying screen real estate before they ask for it.
	pub visible: bool,
	/// Panel height in CSS pixels. Clamped to a sane range on the
	/// frontend so a saved 0 / huge value can't render the editor
	/// invisible.
	pub height: u32,
}

impl Default for BottomPanelAppState {
	fn default() -> Self {
		Self {
			visible: false,
			// Matches `DEFAULT_BOTTOM_PANEL_HEIGHT` in
			// `src/lib/bottomPanel.svelte.ts`. Tall enough to show ~12
			// lines of log output at the default editor font size on
			// a typical 1080p screen, without crowding the editor.
			height: 240,
		}
	}
}
