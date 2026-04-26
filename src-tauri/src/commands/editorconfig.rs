use camino::Utf8PathBuf;
use moon_protocol::editorconfig::EditorConfig;
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

#[tauri::command]
pub async fn editorconfig_for_path(state: State<'_, AppState>, path: String) -> Result<EditorConfig, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.editorconfig_for(&path).await
}
