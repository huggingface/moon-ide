use camino::{Utf8Path, Utf8PathBuf};
use moon_core::{read_host_file, write_host_file};
use moon_protocol::fs::{CollectPathsResult, DirEntry, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{
	BranchDiffStatus, BranchList, BranchSwitchTarget, CommitDiff, CommitEntry, GitBranchInfo, GitChangeSummary,
	GitCommitResult, GitFileBlame, GitFileStatus, GitMergeState, GitPermalink, GitStatusEntry, GitWorktree, PrListScope,
};
use moon_protocol::review::{PublishReviewRequest, PublishReviewResult};
use moon_protocol::MoonError;

use crate::AppCore;

// Every fs command routes through the active folder's host. Paths
// the frontend sends are always absolute (from a tab, from the file
// tree, from a save-as dialog), so the host's job is `LocalHost`-
// flavoured I/O — the routing matters when ContainerHost arrives in
// Phase 2.1 and one folder might be containerised while another
// isn't.

pub async fn fs_read_dir(app: &AppCore, path: String) -> Result<Vec<DirEntry>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
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
pub async fn fs_collect_paths_under(
	app: &AppCore,
	rel: String,
	max_depth: u32,
) -> Result<CollectPathsResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(rel);
	entry.host.collect_paths_under(&path, max_depth).await
}

pub async fn fs_collect_paths(app: &AppCore, max_depth: u32) -> Result<CollectPathsResult, MoonError> {
	// Profiling: paired with the frontend `console.info` from
	// `loadPaths`. The wall time here is the recursive
	// `std::fs::read_dir` storm on the blocking pool; the
	// `require_active_folder` lookup is a single mutex read. See
	// test plan 0076.
	let t0 = std::time::Instant::now();
	let entry = app.workspaces.require_active_folder().await?;
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

pub async fn fs_read_file(app: &AppCore, path: String) -> Result<ReadFileResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.read_file(&path).await
}

pub async fn fs_write_file(app: &AppCore, path: String, text: String) -> Result<WriteFileResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
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
pub async fn fs_read_file_host(path: String) -> Result<ReadFileResult, MoonError> {
	let path = Utf8PathBuf::from(path);
	read_host_file(&path).await
}

/// Largest binary we'll inline as a `data:` URL for preview. The
/// in-workspace path streams from disk via the Tauri asset
/// protocol and has no such limit; this one pays base64 in memory,
/// so cap it rather than wedge the webview on a 200 MB PDF.
const PREVIEW_MAX_BYTES: usize = 25_000_000;

/// Read an image / PDF at an absolute path **outside** every bound
/// folder and return it as a `data:` URL the webview can render.
///
/// In-workspace previews don't come through here — they use
/// `convertFileSrc` (the Tauri asset protocol), which streams from
/// disk. That protocol is host-only, though, so it can't serve a
/// file that exists solely inside the workspace container; this
/// command routes through the active folder's `WorkspaceHost`,
/// which falls back to `docker exec cat` in that case.
pub async fn fs_read_preview_host(app: &AppCore, path: String) -> Result<String, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let abs = Utf8PathBuf::from(&path);
	let mime = preview_mime(&abs).ok_or_else(|| MoonError::invalid(format!("{path}: not a previewable image or PDF")))?;
	let bytes = entry.host.read_preview_bytes(&abs).await?;
	if bytes.len() > PREVIEW_MAX_BYTES {
		return Err(MoonError::invalid(format!(
			"{path}: {:.1} MB is over the {:.0} MB preview limit",
			bytes.len() as f64 / 1_000_000.0,
			PREVIEW_MAX_BYTES as f64 / 1_000_000.0
		)));
	}
	let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
	Ok(format!("data:{mime};base64,{encoded}"))
}

/// One directory of the workspace machine's filesystem, for the path
/// picker a remote window uses instead of the desktop's native dialog
/// (ADR 0095). Hidden entries are skipped; names are sorted.
#[derive(serde::Serialize)]
pub struct BrowseResult {
	pub path: String,
	pub parent: Option<String>,
	pub dirs: Vec<String>,
	pub files: Vec<String>,
}

/// List `path` (the user's home directory when `None`).
pub async fn fs_browse(path: Option<String>) -> Result<BrowseResult, MoonError> {
	let path = match path {
		Some(path) => Utf8PathBuf::from(path),
		None => std::env::var("HOME")
			.map(Utf8PathBuf::from)
			.map_err(|_| MoonError::internal("HOME is not set on the workspace machine"))?,
	};
	let mut entries = tokio::fs::read_dir(&path).await?;
	let mut dirs = Vec::new();
	let mut files = Vec::new();
	while let Some(entry) = entries.next_entry().await? {
		let Ok(name) = entry.file_name().into_string() else {
			continue;
		};
		if name.starts_with('.') {
			continue;
		}
		// Follows symlinks, so a linked checkout lists as a directory.
		match tokio::fs::metadata(entry.path()).await {
			Ok(meta) if meta.is_dir() => dirs.push(name),
			Ok(_) => files.push(name),
			Err(_) => {}
		}
	}
	dirs.sort();
	files.sort();
	Ok(BrowseResult {
		parent: path.parent().map(|p| p.to_string()),
		path: path.into_string(),
		dirs,
		files,
	})
}

/// One slice of a file's bytes plus its total size and preview MIME,
/// for streaming previews (images, PDFs, video) into a remote window
/// (ADR 0095), where the webview can't read the server's disk.
#[derive(serde::Serialize)]
pub struct FileRange {
	/// Base64.
	pub data: String,
	pub offset: u64,
	pub total: u64,
	pub mime: Option<&'static str>,
}

/// Largest slice one call returns; callers page through bigger files.
const RANGE_MAX_BYTES: u64 = 4_000_000;

pub async fn fs_read_range(path: String, offset: u64, length: u64) -> Result<FileRange, MoonError> {
	use tokio::io::{AsyncReadExt, AsyncSeekExt};
	let abs = Utf8PathBuf::from(&path);
	let mut file = tokio::fs::File::open(&abs).await?;
	let total = file.metadata().await?.len();
	let offset = offset.min(total);
	let length = length.min(RANGE_MAX_BYTES).min(total - offset);
	file.seek(std::io::SeekFrom::Start(offset)).await?;
	let mut bytes = vec![0; length as usize];
	file.read_exact(&mut bytes).await?;
	Ok(FileRange {
		data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
		offset,
		total,
		mime: preview_mime(&abs),
	})
}

/// Extension → MIME for the types the preview views render. Mirrors
/// `src/lib/util/fileKind.ts`'s `IMAGE_EXTS` / `VIDEO_EXTS` (plus
/// `pdf`) — keep the two in step, or the frontend will route a file
/// here that we then refuse.
fn preview_mime(path: &Utf8Path) -> Option<&'static str> {
	let ext = path.extension()?.to_ascii_lowercase();
	Some(match ext.as_str() {
		"png" => "image/png",
		"jpg" | "jpeg" => "image/jpeg",
		"gif" => "image/gif",
		"webp" => "image/webp",
		"svg" => "image/svg+xml",
		"bmp" => "image/bmp",
		"ico" => "image/x-icon",
		"avif" => "image/avif",
		"pdf" => "application/pdf",
		"mp4" | "m4v" => "video/mp4",
		"webm" => "video/webm",
		"mov" => "video/quicktime",
		_ => return None,
	})
}

/// Companion to [`fs_read_file_host`] — saves an external buffer back to
/// the host path it came from. Skips the editorconfig / lint-staged save
/// pipeline because external files don't belong to any workspace root.
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
pub async fn fs_create_file(app: &AppCore, path: String) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.create_file(&path).await
}

/// Create a directory at `path`. Errors if the path already
/// exists or any parent directory is missing.
pub async fn fs_create_dir(app: &AppCore, path: String) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.create_dir(&path).await
}

/// Atomically rename `from` to `to`. Both must live inside the
/// active folder; the target must not already exist (we refuse to
/// clobber). Used by the file-tree's inline rename flow.
pub async fn fs_rename(app: &AppCore, from: String, to: String) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let from = Utf8PathBuf::from(from);
	let to = Utf8PathBuf::from(to);
	entry.host.rename_path(&from, &to).await
}

pub async fn fs_stat(app: &AppCore, path: String) -> Result<StatResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.stat(&path).await
}

pub async fn fs_absolute_path(app: &AppCore, path: String) -> Result<String, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.absolute_path(&path).await
}

pub async fn fs_trash(app: &AppCore, path: String) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.trash_path(&path).await
}

pub async fn fs_delete(app: &AppCore, path: String) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.delete_path(&path).await
}

/// Per-path git status for the file tree. Inside a git repo the
/// full add / modify / delete / untracked / ignored vocabulary is
/// reported; outside one, only ignored entries (via the walker
/// fallback against `paths`). Batched so `loadPaths` triggers a
/// single git invocation rather than one per row.
pub async fn fs_git_status_entries(app: &AppCore, paths: Vec<String>) -> Result<Vec<GitStatusEntry>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_status_entries(&paths).await
}

/// Resolve the active folder's `.git/info/exclude` path so the
/// command palette can open it. Returns `None` when the folder isn't
/// a git repo.
pub async fn fs_git_exclude_path(app: &AppCore) -> Result<Option<String>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_exclude_path().await
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
pub async fn fs_git_change_summary(app: &AppCore, folder_path: String) -> Result<GitChangeSummary, MoonError> {
	let entry = app
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
			// `Conflicted` rolls into `modified` for the project-bar
			// summary, mirroring how Pierre's per-row colour maps it
			// at the UI boundary (see test plan 0088).
			GitFileStatus::Modified | GitFileStatus::Conflicted => summary.modified += 1,
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
pub async fn fs_git_restore_paths(app: &AppCore, paths: Vec<String>) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_restore_paths(&paths).await
}

/// `git add -- <paths>` for the active folder. The merge-
/// resolution flow uses this to auto-clear a file's unmerged
/// index entry once the user saves it without conflict markers
/// — without it the row would keep its conflict badge until the
/// commit step's `git add -A` ran.
pub async fn fs_git_add_paths(app: &AppCore, paths: Vec<String>) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_add_paths(&paths).await
}

/// Per-line blame for `path`. Returns `None` (serialised as `null`)
/// for anything that isn't a tracked file inside a git repo; the
/// frontend treats a null response as "no inline annotation".
pub async fn fs_git_blame(app: &AppCore, path: String) -> Result<Option<GitFileBlame>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_blame(&path).await
}

/// Repo permalink (URL + Markdown) for `path` over the inclusive,
/// 1-based line range `start_line..=end_line`. Pins the current
/// `HEAD` commit SHA so the link survives later commits. Returns
/// `None` (serialised as `null`) when the folder's remote isn't a
/// recognised forge (GitHub / Forgejo), has no commits, or `git`
/// isn't available — the editor's "Copy repo link" menu items grey
/// out in that case.
pub async fn fs_git_permalink(
	app: &AppCore,
	path: String,
	start_line: u32,
	end_line: u32,
) -> Result<Option<GitPermalink>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_permalink(&path, start_line, end_line).await
}

/// Blob SHA of the working-tree version of `path` (`git
/// hash-object`). Fingerprints reviewed-file marks (Phase 5.7): the
/// frontend pins a "Viewed" tick to this SHA and clears the tick
/// when a later refresh reports a different one. `None` (serialised
/// as `null`) means the file is gone or `git` is unavailable — the
/// caller leaves the row unticked.
pub async fn fs_git_blob_sha(app: &AppCore, path: String) -> Result<Option<String>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_blob_sha(&path).await
}

/// Publish a batch of local review-comment drafts to the current
/// branch's PR as one review (Phase 5.7.2). Shells out to `gh` on
/// GitHub remotes, POSTs to the instance's REST API (with the fj
/// CLI's stored token) on Forgejo remotes; returns `NoPr` when the
/// branch has no open PR, else `Published` with the posted count,
/// the ids that couldn't be placed at the PR head, and the review
/// URL.
pub async fn fs_publish_pr_review(
	app: &AppCore,
	request: PublishReviewRequest,
) -> Result<PublishReviewResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.publish_pr_review(request).await
}

/// `HEAD` content for `path`. Feeds the "before" side of the editor's
/// git diff view, and doubles as the displayable text for a
/// working-tree-deleted file whose bytes are no longer on disk.
/// `None` (serialised as `null`) means "the path isn't in `HEAD`" —
/// the frontend interprets that as "no diff context; render current
/// text against an empty before side", so the null case isn't an
/// error.
pub async fn fs_git_head_content(app: &AppCore, path: String) -> Result<Option<String>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.git_head_content(&path).await
}

/// `git show <rev>:<path>` for the SCM panel's `Default` compare
/// baseline. `rev` is either `HEAD` or a 40-char hex SHA (the
/// merge-base the frontend cached); the host validates and
/// rejects anything else. `Ok(None)` on absent path / binary blob
/// / no repo follows the same convention as `fs_git_head_content`.
pub async fn fs_git_ref_content(app: &AppCore, rev: String, path: String) -> Result<Option<String>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
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
pub async fn fs_git_default_branch_diff(app: &AppCore) -> Result<Option<BranchDiffStatus>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_default_branch_diff().await
}

/// Web base URL of the active folder's origin/upstream remote
/// (e.g. `https://github.com/owner/repo`), `None` for unrecognised
/// hosts / non-repos. Powers `#123` issue-reference autolinks in
/// the coder transcript's markdown.
pub async fn fs_git_remote_web_url(app: &AppCore) -> Result<Option<String>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_remote_web_url().await
}

/// Branch + HEAD info for the active folder's SCM panel header.
/// All-`None` is the "no branch label" fallback (non-repo folder,
/// detached HEAD with unreadable commit, etc.).
pub async fn fs_git_branch(app: &AppCore) -> Result<GitBranchInfo, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_branch().await
}

/// Stage every working-tree change and commit with `message`.
/// `amend` flips the call to `git commit --amend`, replacing
/// HEAD's commit instead of stacking a new one (the SCM panel's
/// "Amend" toggle drives this). Errors (empty message, nothing
/// to commit, missing author identity) surface as a flash toast
/// so the user can retry from the same input.
pub async fn fs_git_commit(app: &AppCore, message: String, amend: bool) -> Result<GitCommitResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_commit(&message, amend).await
}

/// Create a fresh branch from `HEAD`, switch to it, and commit
/// the staged + working-tree changes. The SCM panel calls this
/// from its "Commit to new branch…" inline form. Branch name
/// validation runs server-side via `git check-ref-format`; on
/// commit failure the host rolls back the branch creation so
/// `HEAD` is back where it started.
pub async fn fs_git_commit_on_new_branch(
	app: &AppCore,
	branch: String,
	message: String,
) -> Result<GitCommitResult, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_commit_on_new_branch(&branch, &message).await
}

/// Create a linked git worktree at `path` on a fresh branch off the
/// active folder's `HEAD`. Backs worktree-backed coder sessions
/// (ADR 0028): the caller picks the on-disk `path` and the new
/// `branch`; the host validates the branch name and surfaces git's
/// own errors. Returns the freshly-created worktree record.
pub async fn fs_git_worktree_add(app: &AppCore, path: String, branch: String) -> Result<GitWorktree, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry
		.host
		.git_worktree_add(&Utf8PathBuf::from(path), moon_core::host::WorktreeBranch::New(branch))
		.await
}

/// List the active folder repository's working trees (main +
/// linked). Used to reconcile session→worktree bindings.
pub async fn fs_git_worktree_list(app: &AppCore) -> Result<Vec<GitWorktree>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_worktree_list().await
}

/// Prune the linked worktree at `path`. `force` (`--force`) is
/// needed when the worktree is dirty or locked; the UI gates that
/// behind a confirm. The branch is never deleted — it's the
/// deliverable left for a later PR.
pub async fn fs_git_worktree_remove(app: &AppCore, path: String, force: bool) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_worktree_remove(&Utf8PathBuf::from(path), force).await
}

/// Push the active folder's current branch to its configured
/// upstream. Returns `Ok(())` on success; failures (no upstream,
/// non-fast-forward, auth) propagate git's stderr.
pub async fn fs_git_push(app: &AppCore) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_push().await
}

/// `git push -u origin HEAD` — first push for a branch that
/// doesn't yet have an upstream configured. The SCM panel calls
/// this instead of `fs_git_push` when `GitBranchInfo.has_upstream`
/// is `false`. Errors propagate git's stderr (no `origin` remote,
/// auth, network).
pub async fn fs_git_publish_branch(app: &AppCore) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_publish_branch().await
}

/// `git pull --rebase` from the active folder's configured
/// upstream. On a rebase conflict the backend aborts the rebase
/// so the working tree is restored; the user resolves in their
/// terminal and retries. Failures (conflicts, dirty tree, no
/// upstream) propagate git's stderr.
pub async fn fs_git_pull(app: &AppCore) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_pull().await
}

/// `git merge --no-edit <remote_ref>` for the active folder. The
/// SCM panel calls this when the user clicks "Update from main";
/// `remote_ref` is whatever the latest `GitBranchInfo` exposed in
/// `default_branch_remote_ref` (typically `"origin/main"`). Errors
/// (conflicts, dirty tree, missing ref) propagate git's stderr
/// verbatim so the flash matches what the user would see in a
/// terminal.
pub async fn fs_git_merge_default_branch(app: &AppCore, remote_ref: String) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_merge_default_branch(&remote_ref).await
}

/// Snapshot of the active folder's in-flight merge for the SCM
/// panel: whether `.git/MERGE_HEAD` exists, the ref being merged,
/// `.git/MERGE_MSG`, and the unmerged-path list. Cheap to poll —
/// the panel calls it on mount, after every git op that could
/// change merge state, and whenever the fs-watcher reports a
/// write inside `.git/`.
pub async fn fs_git_merge_state(app: &AppCore) -> Result<GitMergeState, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_merge_state().await
}

/// `git merge --abort` for the active folder. The SCM panel
/// exposes this as "Abort merge" when a merge is in progress;
/// failures (no merge in progress, dirty pre-merge state) propagate
/// git's stderr verbatim.
pub async fn fs_git_merge_abort(app: &AppCore) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_merge_abort().await
}

/// Recent local branches + open PRs for the branch-switcher
/// palette. Local branches always populate (single-digit ms `git
/// for-each-ref`); the PR section depends on the active folder's
/// remote being a recognised forge and its data path being usable
/// (`gh` installed + signed in for GitHub, the fj CLI's token for
/// Forgejo) — each "no" surfaces as a
/// [`PrListStatus`](moon_protocol::git::PrListStatus) the frontend
/// renders as the section's empty-state row.
pub async fn fs_branch_list(app: &AppCore, pr_scope: PrListScope) -> Result<BranchList, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.branch_list(pr_scope).await
}

/// URL of the open PR for the active folder's current branch, or
/// `null` when there's no matching PR / the forge's data path
/// isn't available. The SCM panel uses this to retarget the "Open
/// PR" button at the existing PR when one exists, instead of the
/// create-PR URL `GitBranchInfo.prUrl` always carries.
pub async fn fs_git_existing_pr_url(app: &AppCore) -> Result<Option<String>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_existing_pr_url().await
}

/// `git switch <name>` (Local) or `gh pr checkout <number>` /
/// `fj pr checkout <number>` (Pr, picked from the remote's forge).
/// Errors propagate the CLI's stderr verbatim — dirty-tree refusal,
/// missing branch, auth required, network failure — so the
/// frontend's flash carries the actionable hint without us
/// re-wrapping it.
pub async fn fs_branch_switch(app: &AppCore, target: BranchSwitchTarget) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.branch_switch(&target).await
}

/// `git fetch --quiet --no-tags` against the configured upstream
/// remote. Drives the periodic auto-fetch loop in the frontend so
/// the "Sync Changes" button surfaces when commits land upstream
/// without the user clicking anything. Best-effort: failures
/// (offline, no upstream, auth refused, 30s timeout) propagate
/// git's stderr but the frontend logs them at debug level rather
/// than flashing a toast — auto-fetch must not be noisy.
pub async fn fs_git_fetch(app: &AppCore) -> Result<(), MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_fetch().await
}

/// `git log -1 --pretty=%B` — subject + body of the current `HEAD`
/// commit. Used by the SCM panel to prefill the commit message
/// when the user toggles "amend" on with an empty draft. Empty
/// string when there's nothing to read (no commits yet, not a
/// repo, git unavailable); the panel renders that as "amend with
/// no prefill" rather than a flash toast.
pub async fn fs_git_head_commit_message(app: &AppCore) -> Result<String, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_head_commit_message().await
}

/// Recent commits on the current branch for the SCM panel's
/// recent-commits list. `limit` caps the number of entries (default
/// 50, clamped server-side to 1–200). Returns short SHA, full SHA,
/// subject, author, and a relative date per commit. Empty vec when
/// the repo has no commits, isn't a repo, or git is unavailable.
pub async fn fs_git_log(app: &AppCore, limit: Option<u32>) -> Result<Vec<CommitEntry>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_log(limit.unwrap_or(50)).await
}

/// File-level diff for a single commit vs its first parent. Returns
/// the parent SHA, commit SHA, subject, and the changed-file list so
/// the `commit://<sha>` pseudo-tab can render per-file diff sections.
/// `Ok(None)` when the commit doesn't exist, isn't a repo, or git is
/// unavailable.
pub async fn fs_git_commit_diff(app: &AppCore, sha: String) -> Result<Option<CommitDiff>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	entry.host.git_commit_diff(&sha).await
}

crate::commands! {
	fs_browse(path: Option<String>),
	fs_read_range(path: String, offset: u64, length: u64),
	fs_read_dir([app] path: String),
	fs_collect_paths_under([app] rel: String, max_depth: u32),
	fs_collect_paths([app] max_depth: u32),
	fs_read_file([app] path: String),
	fs_write_file([app] path: String, text: String),
	fs_read_file_host(path: String),
	fs_read_preview_host([app] path: String),
	fs_write_file_host(path: String, text: String),
	fs_create_file([app] path: String),
	fs_create_dir([app] path: String),
	fs_rename([app] from: String, to: String),
	fs_stat([app] path: String),
	fs_absolute_path([app] path: String),
	fs_trash([app] path: String),
	fs_delete([app] path: String),
	fs_git_status_entries([app] paths: Vec<String>),
	fs_git_exclude_path([app]),
	fs_git_change_summary([app] folder_path: String),
	fs_git_restore_paths([app] paths: Vec<String>),
	fs_git_add_paths([app] paths: Vec<String>),
	fs_git_blame([app] path: String),
	fs_git_permalink([app] path: String, start_line: u32, end_line: u32),
	fs_git_blob_sha([app] path: String),
	fs_publish_pr_review([app] request: PublishReviewRequest),
	fs_git_head_content([app] path: String),
	fs_git_ref_content([app] rev: String, path: String),
	fs_git_default_branch_diff([app]),
	fs_git_remote_web_url([app]),
	fs_git_branch([app]),
	fs_git_commit([app] message: String, amend: bool),
	fs_git_commit_on_new_branch([app] branch: String, message: String),
	fs_git_worktree_add([app] path: String, branch: String),
	fs_git_worktree_list([app]),
	fs_git_worktree_remove([app] path: String, force: bool),
	fs_git_push([app]),
	fs_git_publish_branch([app]),
	fs_git_pull([app]),
	fs_git_merge_default_branch([app] remote_ref: String),
	fs_git_merge_state([app]),
	fs_git_merge_abort([app]),
	fs_branch_list([app] pr_scope: PrListScope),
	fs_git_existing_pr_url([app]),
	fs_branch_switch([app] target: BranchSwitchTarget),
	fs_git_fetch([app]),
	fs_git_head_commit_message([app]),
	fs_git_log([app] limit: Option<u32>),
	fs_git_commit_diff([app] sha: String),
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The mapping must stay in step with `fileKindFor` in
	/// `src/lib/util/fileKind.ts` — the frontend only routes a path
	/// here when *it* thinks the file is an image or a PDF, so a
	/// gap here surfaces as "not a previewable image or PDF" on a
	/// file the tab bar happily opened.
	#[test]
	fn preview_mime_covers_every_frontend_preview_extension() {
		for (name, expected) in [
			("a.png", "image/png"),
			("a.jpg", "image/jpeg"),
			("a.jpeg", "image/jpeg"),
			("a.gif", "image/gif"),
			("a.webp", "image/webp"),
			("a.svg", "image/svg+xml"),
			("a.bmp", "image/bmp"),
			("a.ico", "image/x-icon"),
			("a.avif", "image/avif"),
			("a.pdf", "application/pdf"),
			("a.mp4", "video/mp4"),
			("a.m4v", "video/mp4"),
			("a.webm", "video/webm"),
			("a.mov", "video/quicktime"),
		] {
			assert_eq!(
				preview_mime(Utf8Path::new(name)),
				Some(expected),
				"missing mapping for {name}"
			);
		}
	}

	#[test]
	fn preview_mime_is_case_insensitive_and_rejects_other_types() {
		assert_eq!(preview_mime(Utf8Path::new("/tmp/Shot.PNG")), Some("image/png"));
		assert_eq!(preview_mime(Utf8Path::new("a.rs")), None);
		assert_eq!(preview_mime(Utf8Path::new("a.mkv")), None);
		assert_eq!(preview_mime(Utf8Path::new("noext")), None);
	}

	#[tokio::test]
	async fn read_range_pages_through_a_file() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("clip.mp4");
		std::fs::write(&path, b"0123456789").unwrap();
		let path = path.to_str().unwrap().to_owned();
		let range = fs_read_range(path.clone(), 3, 4).await.unwrap();
		assert_eq!((range.offset, range.total, range.mime), (3, 10, Some("video/mp4")));
		let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, range.data).unwrap();
		assert_eq!(bytes, b"3456");
		let tail = fs_read_range(path, 8, 100).await.unwrap();
		assert_eq!(
			tail.data,
			base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"89")
		);
	}

	#[tokio::test]
	async fn browse_lists_visible_dirs_and_files_sorted() {
		let dir = tempfile::tempdir().unwrap();
		for name in ["b", "a", ".hidden"] {
			std::fs::create_dir(dir.path().join(name)).unwrap();
		}
		std::fs::write(dir.path().join("z.txt"), "").unwrap();
		let root = dir.path().to_str().unwrap().to_owned();
		let listing = fs_browse(Some(root.clone())).await.unwrap();
		assert_eq!(listing.dirs, ["a", "b"]);
		assert_eq!(listing.files, ["z.txt"]);
		assert_eq!(listing.path, root);
		assert!(listing.parent.is_some());
	}
}
