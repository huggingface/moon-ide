use moon_protocol::MoonError;
use serde_json::Value;
use tauri::State;

use crate::state::AppState;

/// In-process entry point into the shared command surface (ADR
/// 0095): the frontend's `call(method, params)` lands here locally,
/// and on `moon-remote` over SSH for a remote workspace.
#[tauri::command]
pub async fn rpc(state: State<'_, AppState>, method: String, params: Value) -> Result<Value, MoonError> {
	state.core.dispatch(&method, params).await
}
