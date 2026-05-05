//! Tauri commands wrapping [`moon_core::app_state`].
//!
//! AppState is split across multiple writers:
//! - The frontend's session-persist path owns `last_session`, `theme`,
//!   and `bottom_panel` and hits this `app_state_save` command on
//!   every navigation.
//! - The Slack tauri commands own `slack.*` and write via their own
//!   load-mutate-save path (see `commands::slack`).
//! - The right-panel pick (`right_panel`) is owned by the frontend
//!   but written through the dedicated `ui_set_right_panel` command
//!   so the slack poller can react synchronously to chat being
//!   opened/closed without waiting for the next persist tick.
//!
//! To stop the frontend's writes from clobbering the Slack slice (or
//! vice versa), `app_state_save` merges: it takes everything from
//! the payload **except** `slack` and `right_panel`, both of which
//! are preserved from disk verbatim. Anything the frontend sends in
//! those fields is ignored on this path.

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
	let existing = core_app_state::load(&state.config_dir).await?;
	let merged = AppStatePayload {
		last_session: app_state.last_session,
		theme: app_state.theme,
		slack: existing.slack,
		bottom_panel: app_state.bottom_panel,
		right_panel: existing.right_panel,
	};
	core_app_state::save(&state.config_dir, &merged).await
}
