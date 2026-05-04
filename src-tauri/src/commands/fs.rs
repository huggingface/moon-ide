use camino::Utf8PathBuf;
use moon_protocol::fs::{DirEntry, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{GitFileBlame, GitStatusEntry};
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

// Every fs command routes through the active folder's host. Paths
// the frontend sends are always absolute (from a tab, from the file
// tree, from a save-as dialog), so the host's job is `LocalHost`-
// flavoured I/O — the routing matters when ContainerHost arrives in
// Phase 2.1 and one folder might be containerised while another
// isn't.

#[tauri::command]
pub async fn fs_read_dir(state: State<'_, AppState>, path: String) -> Result<Vec<DirEntry>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.read_dir(&path).await
}

/// Recursively walk the active folder and return every path in
/// one shot. The tree's refresh used to fire one `fs_read_dir`
/// per directory which, at Tauri's ~15-30ms IPC framing cost per
/// call, dominated refresh latency on anything bigger than a toy
/// repo. This command does the same walk backend-side and returns
/// the full path list in a single roundtrip.
#[tauri::command]
pub async fn fs_collect_paths(state: State<'_, AppState>, max_depth: u32) -> Result<Vec<String>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.collect_paths(max_depth).await
}

#[tauri::command]
pub async fn fs_read_file(state: State<'_, AppState>, path: String) -> Result<ReadFileResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.read_file(&path).await
}

#[tauri::command]
pub async fn fs_write_file(
	state: State<'_, AppState>,
	path: String,
	text: String,
) -> Result<WriteFileResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	// All saves run through the editorconfig-driven pre-save pipeline:
	// line-ending normalization, trim trailing whitespace, ensure final
	// newline. Server-side enforcement keeps the rules consistent
	// whether the writer is the editor, an agent, or (later) an external
	// tool routed through this command. See specs/editorconfig.md.
	let ec = entry.host.editorconfig_for(&path).await?;
	let normalized = moon_core::pre_save::apply_pipeline(&text, &ec);
	entry.host.write_file(&path, &normalized).await
}

#[tauri::command]
pub async fn fs_stat(state: State<'_, AppState>, path: String) -> Result<StatResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.stat(&path).await
}

#[tauri::command]
pub async fn fs_absolute_path(state: State<'_, AppState>, path: String) -> Result<String, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.absolute_path(&path).await
}

#[tauri::command]
pub async fn fs_trash(state: State<'_, AppState>, path: String) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.trash_path(&path).await
}

#[tauri::command]
pub async fn fs_delete(state: State<'_, AppState>, path: String) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.delete_path(&path).await
}

/// Per-path git status for the file tree. Inside a git repo the
/// full add / modify / delete / untracked / ignored vocabulary is
/// reported; outside one, only ignored entries (via the walker
/// fallback against `paths`). Batched so `loadPaths` triggers a
/// single git invocation rather than one per row.
#[tauri::command]
pub async fn fs_git_status_entries(
	state: State<'_, AppState>,
	paths: Vec<String>,
) -> Result<Vec<GitStatusEntry>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_status_entries(&paths).await
}

/// Discard working-tree + index changes for `paths` by restoring
/// them to `HEAD`. Batched so a multi-select discard is one git
/// invocation; the frontend is responsible for routing untracked
/// paths through `fs_trash` instead (HEAD has nothing to restore
/// them to).
#[tauri::command]
pub async fn fs_git_restore_paths(state: State<'_, AppState>, paths: Vec<String>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_restore_paths(&paths).await
}

/// Per-line blame for `path`. Returns `None` (serialised as `null`)
/// for anything that isn't a tracked file inside a git repo; the
/// frontend treats a null response as "no inline annotation".
#[tauri::command]
pub async fn fs_git_blame(state: State<'_, AppState>, path: String) -> Result<Option<GitFileBlame>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_blame(&path).await
}
