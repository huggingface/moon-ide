use camino::Utf8PathBuf;
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::app_state as core_app_state;
use moon_protocol::workspace::{
	slugify_workspace_name, validate_workspace_id, Workspace as WorkspaceRecord, WorkspaceMeta,
};
use moon_protocol::MoonError;
use tauri::State;

use crate::focus_socket;
use crate::state::AppState;

/// Add `path` as a folder in this process's workspace and make
/// it active. The frontend's "Open folder" / "+ Add folder"
/// affordances both end up here. Returns the post-add workspace
/// snapshot so the frontend can reconcile its folder bars
/// without a follow-up `workspace_active`.
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
async fn reset_lsp_if_root_changed(state: &AppState, snap: &WorkspaceRecord) {
	let Some(active) = snap.active_folder.as_ref() else {
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

/// Snapshot the workspace this process owns. Returns `None` for
/// an empty workspace (no folders bound) so the frontend can
/// render the welcome screen — same wire shape as before
/// process-per-workspace.
#[tauri::command]
pub async fn workspace_active(state: State<'_, AppState>) -> Result<Option<WorkspaceRecord>, MoonError> {
	let snap = state.workspaces.snapshot().await;
	if snap.folders.is_empty() && snap.active_folder.is_none() {
		return Ok(None);
	}
	Ok(Some(snap))
}

/// List of workspaces visible to this process. Single-element
/// vec when the process's own workspace has folders bound;
/// empty otherwise. Cross-workspace catalog lookups go through
/// [`workspace_catalog`] instead.
#[tauri::command]
pub async fn workspace_list(state: State<'_, AppState>) -> Result<Vec<WorkspaceRecord>, MoonError> {
	let snap = state.workspaces.snapshot().await;
	if snap.folders.is_empty() {
		Ok(Vec::new())
	} else {
		Ok(vec![snap])
	}
}

/// Catalog of every workspace the user has on this machine,
/// sorted most-recently-active first. Drives the picker palette
/// (Phase 7.8) and the launcher's restore pick (Phase 7.9).
#[tauri::command]
pub async fn workspace_catalog(state: State<'_, AppState>) -> Result<Vec<WorkspaceMeta>, MoonError> {
	let mut metas = core_app_state::load(&state.config_dir).await?.workspaces;
	metas.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
	Ok(metas)
}

/// Create a workspace, **or return the existing one** if the
/// derived slug is already in the catalog. Pure catalog write
/// — no live registry is touched here, because each workspace
/// lives in its own process. The frontend follows up with
/// `window_open(slug)` to spawn / focus the matching child
/// process; the create-or-switch behaviour means
/// `Ctrl+Shift+N "Hugging Face"` does the right thing whether
/// the user has used that name before or not.
///
/// Pass an empty `slug` to derive it from `name`. The existing
/// entry's `name` is **not** overwritten on a hit — renames go
/// through `workspace_rename`. `last_active_at` is bumped on
/// every call so the picker / launcher recency sort reflects
/// that the user just touched this workspace.
#[tauri::command]
pub async fn workspace_create(
	state: State<'_, AppState>,
	slug: String,
	name: String,
) -> Result<WorkspaceMeta, MoonError> {
	let trimmed_name = name.trim();
	if trimmed_name.is_empty() {
		return Err(MoonError::invalid("workspace name must not be empty"));
	}
	let slug = if slug.is_empty() {
		let derived = slugify_workspace_name(trimmed_name);
		if derived.is_empty() {
			return Err(MoonError::invalid(
				"workspace name has no alphanumeric characters; pick a different name",
			));
		}
		derived
	} else {
		slug
	};
	validate_workspace_id(&slug)?;

	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);

	let name_owned = trimmed_name.to_string();
	let slug_owned = slug.clone();
	let meta = core_app_state::mutate(&state.config_dir, move |s| {
		if let Some(existing) = s.workspaces.iter_mut().find(|m| m.id == slug_owned) {
			existing.last_active_at = now;
			return existing.clone();
		}
		let meta = WorkspaceMeta {
			id: slug_owned,
			name: name_owned,
			last_active_at: now,
		};
		s.workspaces.push(meta.clone());
		meta
	})
	.await?;
	Ok(meta)
}

/// Drop a workspace: refuses if a sibling process currently
/// owns its instance lock (deleting underneath a live window
/// would leave that window pointing at nothing). Otherwise runs
/// `docker compose down` for its compose project, deletes
/// `<workspaces_dir>/<slug>/`, and removes the catalog entry.
#[tauri::command]
pub async fn workspace_delete(state: State<'_, AppState>, slug: String) -> Result<(), MoonError> {
	validate_workspace_id(&slug)?;

	if Some(slug.as_str()) == state.workspace_id() {
		return Err(MoonError::invalid(format!(
			"cannot delete `{slug}`: this window is showing it"
		)));
	}
	if focus_socket::workspace_is_live(&state.workspaces_dir, &slug).await {
		return Err(MoonError::invalid(format!(
			"workspace `{slug}` is open in another window; close it first"
		)));
	}

	let state_dir = state.workspace_state_dir(&slug);

	// Best-effort `compose down`. The compose project might
	// still be running from a previous launch even if the
	// workspace was never re-opened this session. A failure
	// here (e.g. docker daemon unreachable) is logged but not
	// fatal: the catalog removal below still happens so the
	// user can clean up the stray container manually.
	let container = ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: slug.clone(),
		state_dir: state_dir.clone(),
		bound_folders: Vec::new(),
	})?;
	if let Err(err) = container.teardown().await {
		tracing::warn!(error = %err, slug = %slug, "compose down failed during workspace_delete");
	}

	if state_dir.exists() {
		if let Err(err) = tokio::fs::remove_dir_all(state_dir.as_std_path()).await {
			tracing::warn!(error = %err, path = %state_dir, "failed to remove workspace state dir");
		}
	}

	let slug_owned = slug.clone();
	core_app_state::mutate(&state.config_dir, move |s| {
		s.workspaces.retain(|m| m.id != slug_owned);
	})
	.await?;
	Ok(())
}

/// Update the human-readable name of a workspace. The id (and
/// therefore the on-disk state dir, the compose project name,
/// and any open process owning it) is immutable.
#[tauri::command]
pub async fn workspace_rename(
	state: State<'_, AppState>,
	slug: String,
	name: String,
) -> Result<WorkspaceMeta, MoonError> {
	let trimmed = name.trim();
	if trimmed.is_empty() {
		return Err(MoonError::invalid("workspace name must not be empty"));
	}
	let new_name = trimmed.to_string();
	let slug_owned = slug.clone();
	core_app_state::mutate(&state.config_dir, move |s| {
		let meta = s
			.workspaces
			.iter_mut()
			.find(|m| m.id == slug_owned)
			.ok_or_else(|| MoonError::NotFound(format!("workspace `{slug_owned}`")))?;
		meta.name = new_name;
		Ok::<WorkspaceMeta, MoonError>(meta.clone())
	})
	.await?
}
