use camino::Utf8PathBuf;
use moon_protocol::workspace::Workspace as WorkspaceRecord;
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

/// Add `path` as a folder in the workspace and make it active. The
/// frontend's "Open folder" / "+ Add folder" affordances both end up
/// here. Returns the post-add workspace snapshot so the frontend can
/// reconcile its folder bars without a follow-up `workspace_active`.
///
/// Idempotent on duplicate path — re-adding a folder that's already
/// bound just flips it to active.
#[tauri::command]
pub async fn workspace_open_local(state: State<'_, AppState>, path: String) -> Result<WorkspaceRecord, MoonError> {
	let path = Utf8PathBuf::from(path);
	state.workspaces.add_folder(path).await?;
	Ok(state.workspaces.snapshot().await)
}

/// Drop a folder from the workspace. If it was the active folder,
/// the previous folder in insertion order takes over (or `None` when
/// no folders remain). Returns the post-remove workspace snapshot.
#[tauri::command]
pub async fn workspace_remove_folder(state: State<'_, AppState>, path: String) -> Result<WorkspaceRecord, MoonError> {
	state.workspaces.remove_folder(&path).await?;
	Ok(state.workspaces.snapshot().await)
}

/// Set the active folder. Errors if `path` isn't already a member of
/// the workspace — callers should `workspace_open_local` to add new
/// folders.
#[tauri::command]
pub async fn workspace_set_active_folder(
	state: State<'_, AppState>,
	path: String,
) -> Result<WorkspaceRecord, MoonError> {
	state.workspaces.set_active_folder(&path).await?;
	Ok(state.workspaces.snapshot().await)
}

/// Snapshot the current workspace (singleton until Phase 7).
#[tauri::command]
pub async fn workspace_active(state: State<'_, AppState>) -> Result<Option<WorkspaceRecord>, MoonError> {
	let snap = state.workspaces.snapshot().await;
	if snap.folders.is_empty() && snap.active_folder.is_none() {
		// Empty workspace — frontend treats this the same as "no
		// workspace open" and shows the welcome screen. Returning
		// `None` keeps the existing IPC contract.
		return Ok(None);
	}
	Ok(Some(snap))
}

/// List of workspaces. Returns the singleton when it has folders,
/// empty otherwise. Phase 7 grows this to multiple workspaces.
#[tauri::command]
pub async fn workspace_list(state: State<'_, AppState>) -> Result<Vec<WorkspaceRecord>, MoonError> {
	let snap = state.workspaces.snapshot().await;
	if snap.folders.is_empty() {
		Ok(Vec::new())
	} else {
		Ok(vec![snap])
	}
}
