use camino::Utf8Path;
use moon_core::search;
use moon_protocol::search::{ContentSearchOptions, ContentSearchResult, FileSearchOptions, FileSearchResult};
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

#[tauri::command]
pub async fn search_files(
	state: State<'_, AppState>,
	options: FileSearchOptions,
) -> Result<Vec<FileSearchResult>, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let root = ws.record.root.clone();
	tokio::task::spawn_blocking(move || search::search_files(Utf8Path::new(&root), &options))
		.await
		.map_err(|e| MoonError::Internal(format!("search task crashed: {e}")))?
}

#[tauri::command]
pub async fn search_content(
	state: State<'_, AppState>,
	options: ContentSearchOptions,
) -> Result<ContentSearchResult, MoonError> {
	let ws = state.workspaces.require_active().await?;
	let root = ws.record.root.clone();
	tokio::task::spawn_blocking(move || search::search_content(Utf8Path::new(&root), &options))
		.await
		.map_err(|e| MoonError::Internal(format!("search task crashed: {e}")))?
}
