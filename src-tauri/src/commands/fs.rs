use camino::Utf8PathBuf;
use moon_protocol::fs::{DirEntry, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

#[tauri::command]
pub async fn fs_read_dir(state: State<'_, AppState>, path: String) -> Result<Vec<DirEntry>, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.read_dir(&path).await
}

#[tauri::command]
pub async fn fs_read_file(state: State<'_, AppState>, path: String) -> Result<ReadFileResult, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.read_file(&path).await
}

#[tauri::command]
pub async fn fs_write_file(
	state: State<'_, AppState>,
	path: String,
	text: String,
) -> Result<WriteFileResult, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.write_file(&path, &text).await
}

#[tauri::command]
pub async fn fs_stat(state: State<'_, AppState>, path: String) -> Result<StatResult, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.stat(&path).await
}

#[tauri::command]
pub async fn fs_absolute_path(state: State<'_, AppState>, path: String) -> Result<String, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.absolute_path(&path).await
}
