use moon_protocol::MoonError;
use serde_json::Value;
use tauri::State;

use crate::state::{AppState, Backend};

/// In-process entry point into the shared command surface (ADR
/// 0095): the frontend's `call(method, params)` lands here locally,
/// and on `moon-remote` over SSH for a remote workspace.
#[tauri::command]
pub async fn rpc(state: State<'_, AppState>, method: String, params: Value) -> Result<Value, MoonError> {
	match &state.backend {
		Backend::Local(core) => core.dispatch(&method, params).await,
		Backend::Remote(remote) => remote.client.call(&method, params).await,
	}
}
