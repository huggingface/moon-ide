use camino::Utf8Path;
use moon_core::search;
use moon_protocol::search::{ContentSearchOptions, ContentSearchResult, FileSearchOptions, FileSearchResult};
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

// Search scopes to the active folder for now. Cross-folder search
// lands in Phase 7 (per-folder tantivy indices, parallel query) once
// the multi-folder shape from 2.5 has miles on it.

#[tauri::command]
pub async fn search_files(
	state: State<'_, AppState>,
	options: FileSearchOptions,
) -> Result<Vec<FileSearchResult>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let root = entry.folder.path.clone();
	tokio::task::spawn_blocking(move || search::search_files(Utf8Path::new(&root), &options))
		.await
		.map_err(|e| MoonError::Internal(format!("search task crashed: {e}")))?
}

#[tauri::command]
pub async fn search_content(
	state: State<'_, AppState>,
	options: ContentSearchOptions,
) -> Result<ContentSearchResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let root = entry.folder.path.clone();
	tokio::task::spawn_blocking(move || search::search_content(Utf8Path::new(&root), &options))
		.await
		.map_err(|e| MoonError::Internal(format!("search task crashed: {e}")))?
}
