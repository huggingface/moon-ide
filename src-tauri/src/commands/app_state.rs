//! Tauri commands wrapping [`moon_core::app_state`].
//!
//! AppState is split across two writers:
//! - The frontend's session-persist path owns `last_session` + `theme`
//!   and hits this `app_state_save` command on every navigation.
//! - The Slack tauri commands own `slack.*` and write via their own
//!   load-mutate-save path (see `commands::slack`).
//! - `bottom_panel` is pure UI chrome owned by the frontend
//!   (visibility + height). The frontend writes it through this
//!   path; the Slack writers don't touch it.
//!
//! To stop the frontend's writes from clobbering the Slack slice (or
//! vice versa), `app_state_save` merges: it takes everything from
//! the payload **except** `slack`, which is preserved from disk
//! verbatim. Anything the frontend sends in `payload.slack` is
//! ignored on this path.

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
	};
	core_app_state::save(&state.config_dir, &merged).await
}
