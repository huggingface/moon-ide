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
	let snap = state.workspaces.snapshot().await;
	repoint_fs_watcher(&state, &snap);
	reset_lsp_if_root_changed(&state, &snap).await;
	Ok(snap)
}

/// Drop a folder from the workspace. If it was the active folder,
/// the previous folder in insertion order takes over (or `None` when
/// no folders remain). Returns the post-remove workspace snapshot.
#[tauri::command]
pub async fn workspace_remove_folder(state: State<'_, AppState>, path: String) -> Result<WorkspaceRecord, MoonError> {
	state.workspaces.remove_folder(&path).await?;
	let snap = state.workspaces.snapshot().await;
	repoint_fs_watcher(&state, &snap);
	reset_lsp_if_root_changed(&state, &snap).await;
	Ok(snap)
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
	let snap = state.workspaces.snapshot().await;
	repoint_fs_watcher(&state, &snap);
	reset_lsp_if_root_changed(&state, &snap).await;
	Ok(snap)
}

/// Keep the filesystem watcher aimed at whichever folder is
/// currently active. No-ops when the active folder hasn't changed
/// (the watcher itself dedupes). Called after every command that
/// mutates the workspace shape.
fn repoint_fs_watcher(state: &AppState, snap: &WorkspaceRecord) {
	let active = snap.active_folder.as_ref().map(std::path::PathBuf::from);
	state.fs_watcher.set_root(active);
}

/// Tear down the LSP broker when the active folder moves to a
/// different root. The broker captures its root at spawn time and
/// file URIs are absolute-path-anchored, so a root switch invalidates
/// everything it knows. Next `lsp_*` command lazily rebuilds against
/// the new root; the frontend re-issues `didOpen` for all open
/// buffers when it sees the workspace change (see
/// `state.svelte.ts` → `applyWorkspaceSnapshot`).
///
/// No-op if no broker is alive, or if the active folder is
/// unchanged from the broker's current root. Also no-op if the
/// workspace has no active folder (folder just removed) — the next
/// `lsp_*` call will fail with a clear error instead.
async fn reset_lsp_if_root_changed(state: &AppState, snap: &WorkspaceRecord) {
	let Some(active) = snap.active_folder.as_ref() else {
		// Active folder gone (last folder removed). Tear down
		// the broker so a subsequent re-open gets a clean state.
		let handle = { state.lsp.lock().await.take() };
		if let Some(old) = handle {
			old.broker.shutdown_all().await;
		}
		return;
	};
	let new_root = Utf8PathBuf::from(active);
	let handle = {
		let mut guard = state.lsp.lock().await;
		match guard.as_ref() {
			Some(existing) if existing.root == new_root => None,
			_ => guard.take(),
		}
	};
	if let Some(old) = handle {
		old.broker.shutdown_all().await;
	}
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
