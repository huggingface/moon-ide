use camino::Utf8PathBuf;
use moon_core::{read_host_file, write_host_file};
use moon_protocol::fs::{CollectPathsResult, DirEntry, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{
	BranchDiffStatus, BranchList, BranchSwitchTarget, GitBranchInfo, GitChangeSummary, GitCommitResult, GitFileBlame,
	GitFileStatus, GitMergeState, GitStatusEntry, PrListScope,
};
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
/// Lazy-load a single subtree on demand. The file tree calls this
/// when the user expands a directory that `fs_collect_paths` left
/// collapsed because git ignored it (`node_modules/`, `target/`,
/// …). One level at a time keeps the IPC small and the path-store
/// batch cheap — drilling deeper into the subtree re-issues this
/// command with the deeper rel.
#[tauri::command]
pub async fn fs_collect_paths_under(
	state: State<'_, AppState>,
	rel: String,
	max_depth: u32,
) -> Result<CollectPathsResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(rel);
	entry.host.collect_paths_under(&path, max_depth).await
}

#[tauri::command]
pub async fn fs_collect_paths(state: State<'_, AppState>, max_depth: u32) -> Result<CollectPathsResult, MoonError> {
	// Profiling: paired with the frontend `console.info` from
	// `loadPaths`. The wall time here is the recursive
	// `std::fs::read_dir` storm on the blocking pool; the
	// `require_active_folder` lookup is a single mutex read. See
	// test plan 0076.
	let t0 = std::time::Instant::now();
	let entry = state.workspaces.require_active_folder().await?;
	let t1 = std::time::Instant::now();
	let result = entry.host.collect_paths(max_depth).await?;
	let t2 = std::time::Instant::now();
	tracing::info!(
		target: "moon_profile",
		"fs_collect_paths folder={} require={}ms walk={}ms total={}ms paths={} depth_capped={}",
		entry.folder.path,
		(t1 - t0).as_millis(),
		(t2 - t1).as_millis(),
		(t2 - t0).as_millis(),
		result.paths.len(),
		result.depth_capped.len(),
	);
	Ok(result)
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

/// Aggregate added / modified / deleted counts for a single bound
/// folder — feeds the per-folder badges on the project bar so the
/// user can see at a glance which folders have working-tree
/// changes. Distinct from `fs_git_status_entries` because (a) the
/// project bar only needs counts, not the full path list, and (b)
/// it must work for **any** bound folder, not just the active one.
/// Folder lookup goes through the `WorkspaceRegistry`; an empty
/// `paths` slice keeps the walker-fallback path silent for non-repo
/// folders (zero counts), which is the right "this folder has
/// nothing to flag" answer.
#[tauri::command]
pub async fn fs_git_change_summary(
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<GitChangeSummary, MoonError> {
	let entry = state
		.workspaces
		.folder_for_path(&folder_path)
		.await
		.ok_or_else(|| MoonError::invalid(format!("unknown folder: {folder_path}")))?;
	let entries = entry.host.git_status_entries(&[]).await?;
	let mut summary = GitChangeSummary::default();
	for e in entries {
		match e.status {
			// `Untracked` files fold into `added`: from the project
			// bar's POV "there's a new file in this folder" is one
			// signal — the SCM panel inside the active folder still
			// keeps the two distinct.
			GitFileStatus::Added | GitFileStatus::Untracked => summary.added += 1,
			GitFileStatus::Modified => summary.modified += 1,
			GitFileStatus::Deleted => summary.deleted += 1,
			GitFileStatus::Ignored => {}
		}
	}
	Ok(summary)
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

/// `git add -- <paths>` for the active folder. The merge-
/// resolution flow uses this to auto-clear a file's unmerged
/// index entry once the user saves it without conflict markers
/// — without it the row would keep its conflict badge until the
/// commit step's `git add -A` ran.
#[tauri::command]
pub async fn fs_git_add_paths(state: State<'_, AppState>, paths: Vec<String>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_add_paths(&paths).await
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

/// `git show <rev>:<path>` for the SCM panel's `Default` compare
/// baseline. `rev` is either `HEAD` or a 40-char hex SHA (the
/// merge-base the frontend cached); the host validates and
/// rejects anything else. `Ok(None)` on absent path / binary blob
/// / no repo follows the same convention as `fs_git_head_content`.
#[tauri::command]
pub async fn fs_git_ref_content(
	state: State<'_, AppState>,
	rev: String,
	path: String,
) -> Result<Option<String>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_ref_content(&rev, &path).await
}

/// Resolve the merge-base with the repo's default branch and
/// return the file-level diff (committed + uncommitted) against
/// it. Powers the `Default` compare baseline: file tree
/// decoration, change gutter, SCM filter view, and the diff
/// view's "before" side all read from the returned
/// `BranchDiffStatus`. `Ok(None)` for non-repo / on-default-branch
/// / detached-HEAD / no-merge-base — the SCM panel silently
/// keeps the toggle inert in those states.
#[tauri::command]
pub async fn fs_git_default_branch_diff(state: State<'_, AppState>) -> Result<Option<BranchDiffStatus>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_default_branch_diff().await
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

/// Create a fresh branch from `HEAD`, switch to it, and commit
/// the staged + working-tree changes. The SCM panel calls this
/// from its "Commit to new branch…" inline form. Branch name
/// validation runs server-side via `git check-ref-format`; on
/// commit failure the host rolls back the branch creation so
/// `HEAD` is back where it started.
#[tauri::command]
pub async fn fs_git_commit_on_new_branch(
	state: State<'_, AppState>,
	branch: String,
	message: String,
) -> Result<GitCommitResult, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_commit_on_new_branch(&branch, &message).await
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

/// `git pull --rebase` from the active folder's configured
/// upstream. On a rebase conflict the backend aborts the rebase
/// so the working tree is restored; the user resolves in their
/// terminal and retries. Failures (conflicts, dirty tree, no
/// upstream) propagate git's stderr.
#[tauri::command]
pub async fn fs_git_pull(state: State<'_, AppState>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_pull().await
}

/// `git merge --no-edit <remote_ref>` for the active folder. The
/// SCM panel calls this when the user clicks "Update from main";
/// `remote_ref` is whatever the latest `GitBranchInfo` exposed in
/// `default_branch_remote_ref` (typically `"origin/main"`). Errors
/// (conflicts, dirty tree, missing ref) propagate git's stderr
/// verbatim so the flash matches what the user would see in a
/// terminal.
#[tauri::command]
pub async fn fs_git_merge_default_branch(state: State<'_, AppState>, remote_ref: String) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_merge_default_branch(&remote_ref).await
}

/// Snapshot of the active folder's in-flight merge for the SCM
/// panel: whether `.git/MERGE_HEAD` exists, the ref being merged,
/// `.git/MERGE_MSG`, and the unmerged-path list. Cheap to poll —
/// the panel calls it on mount, after every git op that could
/// change merge state, and whenever the fs-watcher reports a
/// write inside `.git/`.
#[tauri::command]
pub async fn fs_git_merge_state(state: State<'_, AppState>) -> Result<GitMergeState, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_merge_state().await
}

/// `git merge --abort` for the active folder. The SCM panel
/// exposes this as "Abort merge" when a merge is in progress;
/// failures (no merge in progress, dirty pre-merge state) propagate
/// git's stderr verbatim.
#[tauri::command]
pub async fn fs_git_merge_abort(state: State<'_, AppState>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_merge_abort().await
}

/// Recent local branches + open GitHub PRs for the branch-switcher
/// palette. Local branches always populate (single-digit ms `git
/// for-each-ref`); the PR section depends on `gh` being installed,
/// signed in, and the active folder's remote being on GitHub —
/// each "no" surfaces as a [`PrListStatus`](moon_protocol::git::PrListStatus)
/// the frontend renders as the section's empty-state row.
#[tauri::command]
pub async fn fs_branch_list(state: State<'_, AppState>, pr_scope: PrListScope) -> Result<BranchList, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.branch_list(pr_scope).await
}

/// URL of the open GitHub PR for the active folder's current
/// branch, or `null` when there's no matching PR / `gh` isn't
/// available. The SCM panel uses this to retarget the "Open PR"
/// button at the existing PR when one exists, instead of the
/// create-PR URL `GitBranchInfo.prUrl` always carries.
#[tauri::command]
pub async fn fs_git_existing_pr_url(state: State<'_, AppState>) -> Result<Option<String>, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_existing_pr_url().await
}

/// `git switch <name>` (Local) or `gh pr checkout <number>` (Pr).
/// Errors propagate git / gh stderr verbatim — dirty-tree refusal,
/// missing branch, gh auth required, network failure — so the
/// frontend's flash carries the actionable hint without us
/// re-wrapping it.
#[tauri::command]
pub async fn fs_branch_switch(state: State<'_, AppState>, target: BranchSwitchTarget) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.branch_switch(&target).await
}

/// `git fetch --quiet --no-tags` against the configured upstream
/// remote. Drives the periodic auto-fetch loop in the frontend so
/// the "Sync Changes" button surfaces when commits land upstream
/// without the user clicking anything. Best-effort: failures
/// (offline, no upstream, auth refused, 30s timeout) propagate
/// git's stderr but the frontend logs them at debug level rather
/// than flashing a toast — auto-fetch must not be noisy.
#[tauri::command]
pub async fn fs_git_fetch(state: State<'_, AppState>) -> Result<(), MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_fetch().await
}

/// `git log -1 --pretty=%B` — subject + body of the current `HEAD`
/// commit. Used by the SCM panel to prefill the commit message
/// when the user toggles "amend" on with an empty draft. Empty
/// string when there's nothing to read (no commits yet, not a
/// repo, git unavailable); the panel renders that as "amend with
/// no prefill" rather than a flash toast.
#[tauri::command]
pub async fn fs_git_head_commit_message(state: State<'_, AppState>) -> Result<String, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	entry.host.git_head_commit_message().await
}
