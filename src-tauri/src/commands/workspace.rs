use camino::Utf8PathBuf;
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::app_state as core_app_state;
use moon_protocol::workspace::{
	slugify_workspace_name, validate_workspace_id, Workspace as WorkspaceRecord, WorkspaceMeta,
};
use moon_protocol::MoonError;
use tauri::{Manager, State};

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
	detach_lsp_teardown_if_root_changed(&state, &snap).await;
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
	detach_lsp_teardown_if_root_changed(&state, &snap).await;
	Ok(snap)
}

/// Re-stamp the active folder's worktree branch label from its
/// actual checked-out branch (ADR 0028). The folder bar shows a
/// worktree's branch from the registry, captured at creation; an
/// in-worktree commit-on-new-branch or `git switch` changes the real
/// branch, leaving the label stale. The frontend calls this after
/// those ops; it's a no-op (returns the unchanged snapshot) when the
/// active folder isn't a worktree. Returns the post-sync snapshot.
#[tauri::command]
pub async fn workspace_sync_active_worktree_branch(state: State<'_, AppState>) -> Result<WorkspaceRecord, MoonError> {
	if let Some(folder) = state.workspaces.active_folder().await {
		if matches!(
			folder.folder.origin,
			moon_protocol::workspace::FolderOrigin::Worktree { .. }
		) {
			if let Some(branch) = folder.host.git_branch().await?.name {
				state.workspaces.set_worktree_branch(&folder.folder.path, &branch).await;
			}
		}
	}
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
	// Profiling: paired with the frontend `console.info` from
	// `setActiveFolder`. Any phase that creeps past single-digit ms
	// here is the IPC roundtrip's bottleneck. See test plan 0076.
	let t0 = std::time::Instant::now();
	state.workspaces.set_active_folder(&path).await?;
	let t1 = std::time::Instant::now();
	let snap = state.workspaces.snapshot().await;
	let t2 = std::time::Instant::now();
	repoint_fs_watcher(&state, &snap);
	let t3 = std::time::Instant::now();
	detach_lsp_teardown_if_root_changed(&state, &snap).await;
	let t4 = std::time::Instant::now();
	tracing::info!(
		target: "moon_profile",
		"workspace_set_active_folder path={} set={}ms snapshot={}ms watcher={}ms lsp_detach={}ms total={}ms",
		path,
		(t1 - t0).as_millis(),
		(t2 - t1).as_millis(),
		(t3 - t2).as_millis(),
		(t4 - t3).as_millis(),
		(t4 - t0).as_millis(),
	);
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
/// The actual `shutdown_all` runs on a detached task — every LSP
/// server gets up to 4 s of timeouts (2 s shutdown request plus 2 s
/// child wait), iterated **sequentially**, so on a folder running
/// TS, rust-analyzer, and tailwind together the synchronous
/// version stalled the IPC roundtrip for 6–12 s. The user can't
/// switch folders while LSPs are dying, even though nothing on the
/// frontend's critical path actually depends on the old brokers
/// finishing teardown: the next `lsp_*` command lazily builds a
/// fresh broker against the new root regardless. Snipping the
/// handle out of the mutex synchronously and letting the spawned
/// task own the rest of the teardown gives the IPC ~sub-millisecond
/// latency here.
async fn detach_lsp_teardown_if_root_changed(state: &AppState, snap: &WorkspaceRecord) {
	let old_handle = match snap.active_folder.as_ref() {
		None => state.lsp.lock().await.take(),
		Some(active) => {
			let new_root = Utf8PathBuf::from(active);
			let mut guard = state.lsp.lock().await;
			match guard.as_ref() {
				Some(existing) if existing.root == new_root => None,
				_ => guard.take(),
			}
		}
	};
	if let Some(old) = old_handle {
		tauri::async_runtime::spawn(async move {
			old.broker.shutdown_all().await;
		});
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
	metas.sort_by_key(|m| std::cmp::Reverse(m.last_active_at));
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
			color: None,
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

/// Update the badge colour for a workspace. `color` is either a
/// `#rrggbb` (or `#rgb`) hex string or an empty string to clear
/// back to the deterministic hash-derived colour. When the
/// edited workspace is the one this process owns, the running
/// window's icon repaints immediately — sibling processes
/// repaint on next launch.
///
/// Validation is intentionally light: an unparseable colour is
/// stored as-is and the icon code falls back to the hash hue.
/// That way a partial typo round-trip from the picker doesn't
/// fail loudly; the user just sees the default colour and
/// re-edits.
#[tauri::command]
pub async fn workspace_set_color(
	app: tauri::AppHandle,
	state: State<'_, AppState>,
	slug: String,
	color: String,
) -> Result<WorkspaceMeta, MoonError> {
	validate_workspace_id(&slug)?;
	let trimmed = color.trim();
	let next_color: Option<String> = if trimmed.is_empty() {
		None
	} else {
		Some(trimmed.to_string())
	};
	let slug_owned = slug.clone();
	let color_for_mutate = next_color.clone();
	let meta = core_app_state::mutate(&state.config_dir, move |s| {
		let meta = s
			.workspaces
			.iter_mut()
			.find(|m| m.id == slug_owned)
			.ok_or_else(|| MoonError::NotFound(format!("workspace `{slug_owned}`")))?;
		meta.color = color_for_mutate;
		Ok::<WorkspaceMeta, MoonError>(meta.clone())
	})
	.await??;

	if Some(slug.as_str()) == state.workspace_id() {
		if let Some(window) = app.get_webview_window("main") {
			crate::window_icon::apply_workspace_icon(&window, &slug, next_color.as_deref());
		}
	}
	Ok(meta)
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
