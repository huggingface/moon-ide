use camino::Utf8PathBuf;
use moon_core::{read_host_file, write_host_file};
use moon_protocol::fs::{DirEntry, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{GitBranchInfo, GitCommitResult, GitFileBlame, GitStatusEntry};
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
	// `save_file` is the single seam every editor / agent / future
	// writer funnels through: editorconfig pre-save (line endings, trim
	// ws, final newline) plus the lint-staged formatter. See
	// specs/editorconfig.md and specs/decisions/0012-format-on-save.md.
	entry.host.save_file(&path, &text).await
}

/// Read an arbitrary host path, bypassing every `WorkspaceHost`. Used by
/// the "Open File…" affordance for files that live outside any bound folder.
/// In the Phase 2 container world, this still reads from the host
/// filesystem — the in-container host can't see paths outside the bind
/// mount, so routing those through `WorkspaceHost::read_file` would be
/// wrong by construction. No active-folder check; absolute paths only.
#[tauri::command]
pub async fn fs_read_file_host(path: String) -> Result<ReadFileResult, MoonError> {
	let path = Utf8PathBuf::from(path);
	read_host_file(&path).await
}

/// Companion to [`fs_read_file_host`] — saves an external buffer back to
/// the host path it came from. Skips the editorconfig / lint-staged save
/// pipeline because external files don't belong to any workspace root.
#[tauri::command]
pub async fn fs_write_file_host(path: String, text: String) -> Result<WriteFileResult, MoonError> {
	let path = Utf8PathBuf::from(path);
	write_host_file(&path, &text).await
}

/// Create an empty file at `path`. Errors if the path already
/// exists or any parent directory is missing — the file-tree's
/// "New file" flow uses this and surfaces those errors to the
/// user. Distinct from `fs_write_file` because we want strict
/// "fail-on-exists" semantics that `save_file`'s create-or-
/// truncate doesn't give.
#[tauri::command]
pub async fn fs_create_file(state: State<'_, AppState>, path: String) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.create_file(&path).await
}

/// Create a directory at `path`. Errors if the path already
/// exists or any parent directory is missing.
#[tauri::command]
pub async fn fs_create_dir(state: State<'_, AppState>, path: String) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.create_dir(&path).await
}

/// Atomically rename `from` to `to`. Both must live inside the
/// active folder; the target must not already exist (we refuse to
/// clobber). Used by the file-tree's inline rename flow.
#[tauri::command]
pub async fn fs_rename(state: State<'_, AppState>, from: String, to: String) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let from = Utf8PathBuf::from(from);
	let to = Utf8PathBuf::from(to);
	entry.host.rename_path(&from, &to).await
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

/// `HEAD` content for `path`. Feeds the "before" side of the editor's
/// git diff view, and doubles as the displayable text for a
/// working-tree-deleted file whose bytes are no longer on disk.
/// `None` (serialised as `null`) means "the path isn't in `HEAD`" —
/// the frontend interprets that as "no diff context; render current
/// text against an empty before side", so the null case isn't an
/// error.
#[tauri::command]
pub async fn fs_git_head_content(state: State<'_, AppState>, path: String) -> Result<Option<String>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_head_content(&path).await
}

/// Branch + HEAD info for the active folder's SCM panel header.
/// All-`None` is the "no branch label" fallback (non-repo folder,
/// detached HEAD with unreadable commit, etc.).
#[tauri::command]
pub async fn fs_git_branch(state: State<'_, AppState>) -> Result<GitBranchInfo, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_branch().await
}

/// Stage every working-tree change and commit with `message`.
/// `amend` flips the call to `git commit --amend`, replacing
/// HEAD's commit instead of stacking a new one (the SCM panel's
/// "Amend" toggle drives this). Errors (empty message, nothing
/// to commit, missing author identity) surface as a flash toast
/// so the user can retry from the same input.
#[tauri::command]
pub async fn fs_git_commit(
	state: State<'_, AppState>,
	message: String,
	amend: bool,
) -> Result<GitCommitResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_commit(&message, amend).await
}

/// Push the active folder's current branch to its configured
/// upstream. Returns `Ok(())` on success; failures (no upstream,
/// non-fast-forward, auth) propagate git's stderr.
#[tauri::command]
pub async fn fs_git_push(state: State<'_, AppState>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_push().await
}

/// `git push -u origin HEAD` — first push for a branch that
/// doesn't yet have an upstream configured. The SCM panel calls
/// this instead of `fs_git_push` when `GitBranchInfo.has_upstream`
/// is `false`. Errors propagate git's stderr (no `origin` remote,
/// auth, network).
#[tauri::command]
pub async fn fs_git_publish_branch(state: State<'_, AppState>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_publish_branch().await
}

/// Pull from the active folder's configured upstream using the
/// user's `pull.rebase` preference. Failures (conflicts, dirty
/// tree, no upstream) propagate git's stderr.
#[tauri::command]
pub async fn fs_git_pull(state: State<'_, AppState>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_pull().await
}
