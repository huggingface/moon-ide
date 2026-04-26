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
	// All saves run through the editorconfig-driven pre-save pipeline:
	// line-ending normalization, trim trailing whitespace, ensure final
	// newline. Server-side enforcement keeps the rules consistent
	// whether the writer is the editor, an agent, or (later) an external
	// tool routed through this command. See specs/editorconfig.md.
	let ec = ws.host.editorconfig_for(&path).await?;
	let normalized = moon_core::pre_save::apply_pipeline(&text, &ec);
	ws.host.write_file(&path, &normalized).await
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

#[tauri::command]
pub async fn fs_trash(state: State<'_, AppState>, path: String) -> Result<(), MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.trash_path(&path).await
}

#[tauri::command]
pub async fn fs_delete(state: State<'_, AppState>, path: String) -> Result<(), MoonError> {
	let ws = state.workspaces.require_active().await?;
	let path = Utf8PathBuf::from(path);
	ws.host.delete_path(&path).await
}
