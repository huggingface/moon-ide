use camino::Utf8PathBuf;
use moon_protocol::workspace::Workspace as WorkspaceRecord;
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

#[tauri::command]
pub async fn workspace_open_local(state: State<'_, AppState>, path: String) -> Result<WorkspaceRecord, MoonError> {
	// Persisting "last opened" used to live here. It now flows through
	// `app_state_save` from the frontend so the workspace path, open
	// tabs, and theme are stored as one consistent blob — see
	// `commands/app_state.rs`.
	let path = Utf8PathBuf::from(path);
	let ws = state.workspaces.open_local(path).await?;
	Ok(ws.record.clone())
}

#[tauri::command]
pub async fn workspace_active(state: State<'_, AppState>) -> Result<Option<WorkspaceRecord>, MoonError> {
	Ok(state.workspaces.active().await.map(|w| w.record.clone()))
}

#[tauri::command]
pub async fn workspace_list(state: State<'_, AppState>) -> Result<Vec<WorkspaceRecord>, MoonError> {
	Ok(state.workspaces.list().await)
}
