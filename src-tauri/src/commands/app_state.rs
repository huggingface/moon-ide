//! Tauri commands wrapping [`moon_core::app_state`].
//!
//! AppState is split across multiple writers:
//! - The frontend's persist path owns `theme`, `bottom_panel`, and
//!   `next_edit` and hits this `app_state_save` command on every
//!   navigation.
//! - The Slack tauri commands own `slack.*` and write via their own
//!   load-mutate-save path (see `commands::slack`).
//! - The right-panel pick (`right_panel`) is owned by the frontend
//!   but written through the dedicated `ui_set_right_panel` command
//!   so the slack poller can react synchronously to chat being
//!   opened/closed without waiting for the next persist tick.
//! - The coder slice (`coder.last_session_by_folder`) is owned by
//!   the coder Tauri commands — set when `coder_open_session`
//!   lands so the relaunch path can re-open the right session.
//! - Per-workspace session blobs (folders, tabs, splits) live in
//!   their own per-workspace `session.json` (see
//!   `commands::session`), not in `AppState`.
//!
//! To stop the frontend's writes from clobbering the Slack slice
//! (or vice versa), `app_state_save` merges: it takes everything
//! from the payload **except** `workspaces`, `slack`,
//! `right_panel`, and `coder`, all of which are preserved from
//! disk verbatim. Anything the frontend sends in those fields
//! is ignored on this path. The workspace catalog
//! (`workspaces`) is bootstrap-and-eventually-IPC-owned (Phase
//! 7.6); the frontend has no business mutating it through this
//! generic save path.

use moon_core::app_state as core_app_state;
use moon_protocol::app_state::AppState as AppStatePayload;
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

#[tauri::command]
pub async fn app_state_load(state: State<'_, AppState>) -> Result<AppStatePayload, MoonError> {
	core_app_state::load(&state.config_dir).await
}

#[tauri::command]
pub async fn app_state_save(state: State<'_, AppState>, app_state: AppStatePayload) -> Result<(), MoonError> {
	core_app_state::mutate(&state.config_dir, move |s| {
		s.theme = app_state.theme;
		s.bottom_panel = app_state.bottom_panel;
		s.next_edit = app_state.next_edit;
		// `workspaces`, `slack`, `right_panel`, `coder` are
		// preserved from disk verbatim — owned by other writers,
		// see the module docs above.
	})
	.await
}
