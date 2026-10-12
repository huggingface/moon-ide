//! Folder membership of the workspace this process owns. The
//! machine-wide workspace catalog (create / rename / delete / colour)
//! stays a desktop command: it spawns windows and lives on the
//! user's machine, not in the workspace process.

use camino::Utf8PathBuf;
use moon_protocol::workspace::Workspace as WorkspaceRecord;
use moon_protocol::MoonError;

use crate::AppCore;

/// Add `path` as a folder in this process's workspace and make
/// it active. The frontend's "Open folder" / "+ Add folder"
/// affordances both end up here. Returns the post-add workspace
/// snapshot so the frontend can reconcile its folder bars
/// without a follow-up `workspace_active`.
///
/// Idempotent on duplicate path — re-adding a folder that's already
/// bound just flips it to active.
pub async fn workspace_open_local(app: &AppCore, path: String) -> Result<WorkspaceRecord, MoonError> {
	let path = Utf8PathBuf::from(path);
	app.workspaces.add_folder(path).await?;
	// Disk is the source of truth for worktrees (ADR 0079): a
	// folder re-added after a session.json loss brings its
	// IDE-managed `.worktrees/` checkouts back as rows.
	app.workspaces.adopt_disk_worktrees().await;
	let snap = app.workspaces.snapshot().await;
	repoint_fs_watcher(app, &snap);
	detach_lsp_teardown_if_root_changed(app, &snap).await;
	Ok(snap)
}

/// Drop a folder from the workspace. If it was the active folder,
/// the previous folder in insertion order takes over (or `None` when
/// no folders remain). Returns the post-remove workspace snapshot.
pub async fn workspace_remove_folder(app: &AppCore, path: String) -> Result<WorkspaceRecord, MoonError> {
	app.workspaces.remove_folder(&path).await?;
	let snap = app.workspaces.snapshot().await;
	repoint_fs_watcher(app, &snap);
	detach_lsp_teardown_if_root_changed(app, &snap).await;
	Ok(snap)
}

/// Re-stamp the active folder's worktree branch label from its
/// actual checked-out branch (ADR 0028). The folder bar shows a
/// worktree's branch from the registry, captured at creation; an
/// in-worktree commit-on-new-branch or `git switch` changes the real
/// branch, leaving the label stale. The frontend calls this after
/// those ops; it's a no-op (returns the unchanged snapshot) when the
/// active folder isn't a worktree. Returns the post-sync snapshot.
pub async fn workspace_sync_active_worktree_branch(app: &AppCore) -> Result<WorkspaceRecord, MoonError> {
	if let Some(folder) = app.workspaces.active_folder().await {
		if matches!(
			folder.folder.origin,
			moon_protocol::workspace::FolderOrigin::Worktree { .. }
		) {
			if let Some(branch) = folder.host.git_branch().await?.name {
				app.workspaces.set_worktree_branch(&folder.folder.path, &branch).await;
			}
		}
	}
	Ok(app.workspaces.snapshot().await)
}

/// Set the active folder. Errors if `path` isn't already a member of
/// the workspace — callers should `workspace_open_local` to add new
/// folders.
pub async fn workspace_set_active_folder(app: &AppCore, path: String) -> Result<WorkspaceRecord, MoonError> {
	// Profiling: paired with the frontend `console.info` from
	// `setActiveFolder`. Any phase that creeps past single-digit ms
	// here is the IPC roundtrip's bottleneck. See test plan 0076.
	let t0 = std::time::Instant::now();
	app.workspaces.set_active_folder(&path).await?;
	let t1 = std::time::Instant::now();
	let snap = app.workspaces.snapshot().await;
	let t2 = std::time::Instant::now();
	repoint_fs_watcher(app, &snap);
	let t3 = std::time::Instant::now();
	detach_lsp_teardown_if_root_changed(app, &snap).await;
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
fn repoint_fs_watcher(app: &AppCore, snap: &WorkspaceRecord) {
	let active = snap.active_folder.as_ref().map(std::path::PathBuf::from);
	app.fs_watcher.set_root(active);
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
async fn detach_lsp_teardown_if_root_changed(app: &AppCore, snap: &WorkspaceRecord) {
	let old_handle = match snap.active_folder.as_ref() {
		None => app.lsp.lock().await.take(),
		Some(active) => {
			let new_root = Utf8PathBuf::from(active);
			let mut guard = app.lsp.lock().await;
			match guard.as_ref() {
				Some(existing) if existing.root == new_root => None,
				_ => guard.take(),
			}
		}
	};
	if let Some(old) = old_handle {
		tokio::spawn(async move {
			old.broker.shutdown_all().await;
		});
	}
}

/// Snapshot the workspace this process owns. Returns `None` for
/// an empty workspace (no folders bound) so the frontend can
/// render the welcome screen — same wire shape as before
/// process-per-workspace.
pub async fn workspace_active(app: &AppCore) -> Result<Option<WorkspaceRecord>, MoonError> {
	let snap = app.workspaces.snapshot().await;
	if snap.folders.is_empty() && snap.active_folder.is_none() {
		return Ok(None);
	}
	Ok(Some(snap))
}

/// List of workspaces visible to this process. Single-element
/// vec when the process's own workspace has folders bound;
/// empty otherwise. Cross-workspace catalog lookups go through
/// the desktop's `workspace_catalog` instead.
pub async fn workspace_list(app: &AppCore) -> Result<Vec<WorkspaceRecord>, MoonError> {
	let snap = app.workspaces.snapshot().await;
	if snap.folders.is_empty() {
		return Ok(Vec::new());
	}
	Ok(vec![snap])
}

crate::commands!(
	workspace_open_local([app] path: String),
	workspace_remove_folder([app] path: String),
	workspace_set_active_folder([app] path: String),
	workspace_sync_active_worktree_branch([app]),
	workspace_active([app]),
	workspace_list([app]),
);
