//! Tauri commands wrapping [`moon_core::app_state`].
//!
//! AppState is one small struct (last UI session + theme); the frontend
//! is the only writer. We expose load + save and let the frontend
//! manage read-modify-write itself — there is no per-field setter.

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
	core_app_state::save(&state.config_dir, &app_state).await
}
