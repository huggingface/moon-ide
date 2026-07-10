//! `WorkspaceHost` is the I/O boundary. See [ADR 0002](../../../specs/decisions/0002-workspace-host.md).
//!
//! Phase 0 ships only `LocalHost`. The trait exists pre-implementation
//! so call sites in higher layers don't have to be rewritten when
//! `RemoteHost` lands in Phase 2.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::editorconfig::EditorConfig;
use moon_protocol::fs::{CollectPathsResult, DirEntry, EntryKind, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{
	BranchDiffStatus, BranchList, BranchListEntry, BranchSwitchTarget, CommitDiff, CommitEntry, GitBranchInfo,
	GitCommitResult, GitFileBlame, GitFileStatus, GitLineBlame, GitMergeState, GitPermalink, GitStatusEntry, GitWorktree,
	PrListScope, PrListStatus,
};
use moon_protocol::review::{PublishReviewRequest, PublishReviewResult, ReviewSide};
use moon_protocol::{MoonError, MoonResult};
use std::sync::Arc;
use std::time::SystemTime;

use crate::editorconfig::EditorConfigService;
use crate::format;
use crate::lint_staged::{LintStagedRules, LintStagedService};
use crate::pre_save;
use crate::shell::{ShellResolverHandle, ShellTarget};

/// What branch a freshly-created worktree should be on (ADR 0028).
#[derive(Debug, Clone)]
pub enum WorktreeBranch {
	/// Create a fresh branch off the current `HEAD`
	/// (`git worktree add -b <name> <path>`). The default
	/// isolated-session behaviour.
	New(String),
	/// Check out an **existing** branch (`git worktree add <path>
	/// <name>`), DWIM-creating a local tracking branch from a remote
	/// of the same name when no local branch exists — the same
	/// resolution `git switch <name>` does. Lets an isolated session
	/// pick up a local or remote branch *without* checking it out in
	/// the parent (which would disturb other agents). A branch can be
	/// checked out in only one worktree, so this fails if `name` is
	/// already checked out (in the parent or another worktree).
	Existing(String),
}

impl WorktreeBranch {
	/// The branch name either variant resolves to — used for the
	/// worktree's display label, on-disk dir slug, and session header.
	pub fn name(&self) -> &str {
		match self {
			WorktreeBranch::New(name) | WorktreeBranch::Existing(name) => name,
		}
	}
}

#[async_trait]
pub trait WorkspaceHost: Send + Sync {
	async fn read_dir(&self, path: &Utf8Path) -> MoonResult<Vec<DirEntry>>;
	async fn read_file(&self, path: &Utf8Path) -> MoonResult<ReadFileResult>;
	async fn write_file(&self, path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult>;

	/// Create an empty file at `path`. Errors if any path component
	/// inside the workspace is missing (we don't auto-mkdir parent
	/// directories — the caller is responsible for creating those
	/// via `create_dir` first; a typo like `srrc/foo.ts` should
	/// surface as an error rather than silently land a stray file
	/// under a new directory). Errors if the target already exists,
	/// so the file-tree's "new file" flow can't accidentally clobber
	/// a sibling. The caller picks up the post-create state via the
	/// usual fs-watcher / `loadPaths` refresh.
	async fn create_file(&self, path: &Utf8Path) -> MoonResult<()>;

	/// Create a directory at `path`. Errors if it already exists.
	/// Like `create_file` we don't auto-mkdir parents; the caller
	/// can issue multiple `create_dir` calls when nesting is
	/// intentional, and a typo should surface as an error rather
	/// than create a chain of unintended dirs.
	async fn create_dir(&self, path: &Utf8Path) -> MoonResult<()>;

	/// Atomic rename of `from` to `to`. Both must live inside the
	/// workspace root. Errors if `from` doesn't exist or `to`
	/// already exists. Used by the file-tree's inline rename and
	/// by any future "Move to…" command. RemoteHost (Phase 2)
	/// serves this over JSON-RPC so the move happens entirely
	/// inside the container.
	async fn rename_path(&self, from: &Utf8Path, to: &Utf8Path) -> MoonResult<()>;

	/// Write `text` after running it through the save pipeline:
	/// `.editorconfig` line-ending / trim-ws / final-newline normalization,
	/// then every command in the file's lint-staged chain (run in order,
	/// against the file already on disk — same shape `bun run
	/// lint-staged` uses on commit). Every editor save funnels through
	/// this, so the on-disk shape matches what lint-staged would
	/// produce regardless of who issued the write. Failures inside the
	/// formatter step never abort the save — callers are guaranteed to
	/// land at least the editorconfig-normalised bytes. See
	/// [specs/decisions/0012-format-on-save.md](../../../specs/decisions/0012-format-on-save.md).
	///
	/// The coder's `write_file` / `edit_file` tools deliberately don't
	/// route here — they write raw bytes via [`write_file`](Self::write_file)
	/// and defer the save pipeline to [`format_file`](Self::format_file)
	/// at turn end, so a multi-edit turn doesn't re-spawn the formatter
	/// chain on every tool call and an `eslint --fix` between edits
	/// can't strip an "unused" import the model intends to use two
	/// edits later.
	async fn save_file(&self, path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult>;

	/// Run the full save pipeline against the bytes already on disk
	/// at `path`: read, apply the editorconfig pre-save transforms,
	/// write back, then spawn the lint-staged chain. Same on-disk
	/// shape as [`save_file`](Self::save_file) but starting from the
	/// existing file rather than a fresh string. The coder calls
	/// this at turn end against the union of files its write/edit
	/// tools touched. Failures collapse to a `tracing::warn!` and
	/// return the file's current stat; the caller never aborts on
	/// format failure (best-effort, same posture as `save_file`).
	async fn format_file(&self, path: &Utf8Path) -> MoonResult<WriteFileResult>;

	async fn stat(&self, path: &Utf8Path) -> MoonResult<StatResult>;

	/// Move a file or directory to the OS trash / recycle bin (XDG
	/// trash on Linux, Finder Trash on macOS, Recycle Bin on Windows).
	/// This is the default destructive action — `Delete` in the file
	/// tree maps here. Reversible via the OS UI; callers still confirm
	/// to make sure the user picked the right row.
	async fn trash_path(&self, path: &Utf8Path) -> MoonResult<()>;

	/// Permanently delete a file or directory, bypassing the trash.
	/// Directories are removed recursively. Reachable via `Shift+Delete`
	/// in the file tree; the team's recovery story for tracked files
	/// is git, so the only safety net is the confirmation prompt the
	/// caller is expected to show.
	async fn delete_path(&self, path: &Utf8Path) -> MoonResult<()>;

	/// Returns an absolute, canonical, host-side path for `path`. The string
	/// is shaped for the **host** that owns the file (the OS path on local;
	/// the in-container path on remote). Used by the UI to feed Tauri's asset
	/// protocol for image previews and any other "give me a URL the webview
	/// can load directly" case. Remote hosts will return a path the webview
	/// cannot dereference directly; that's their problem to handle (e.g. by
	/// streaming bytes back over JSON-RPC instead).
	async fn absolute_path(&self, path: &Utf8Path) -> MoonResult<String>;

	/// Effective `.editorconfig` for `path`. The cascade is fully
	/// resolved host-side; the caller gets the resulting struct, never
	/// the chain. RemoteHost (Phase 2) serves this over JSON-RPC, so
	/// agents and devcontainer-hosted tools see the same answer the UI
	/// does — this is the single point where the rules are decided.
	async fn editorconfig_for(&self, path: &Utf8Path) -> MoonResult<EditorConfig>;

	/// Effective lint-staged rules for `path`. The walk (closest
	/// `.lintstagedrc.json`, then `package.json#lint-staged`, up to the
	/// workspace root) happens host-side; the caller gets a compiled
	/// matcher and runs `match_command` against the absolute path. An
	/// empty `LintStagedRules` means "no formatter configured for any
	/// file under this directory", not an error. RemoteHost (Phase 2)
	/// serves this over JSON-RPC so the same rules reach agents and
	/// devcontainer-hosted writers.
	async fn lint_staged_for(&self, path: &Utf8Path) -> MoonResult<LintStagedRules>;

	/// Recursively enumerate every path inside the workspace root,
	/// returning the same string format the file tree consumes
	/// (directories carry a trailing `/`, files don't, `.git/` is
	/// skipped). `max_depth` bounds how deep we recurse so very
	/// nested trees can't stall the UI on first load; entries at
	/// the cap are included but their children aren't — those
	/// directories surface in [`CollectPathsResult::depth_capped`]
	/// so the frontend can lazy-load them on expansion.
	///
	/// Exists separately from `read_dir` because the tree's walker
	/// would otherwise fire one IPC roundtrip per directory —
	/// dominating the refresh latency on anything bigger than a
	/// handful of folders. One call collapses hundreds of
	/// roundtrips into a single backend walk, which is the actual
	/// work; everything else was IPC framing.
	async fn collect_paths(&self, max_depth: u32) -> MoonResult<CollectPathsResult>;

	/// Walk a single subtree on demand, ignoring the gitignore-
	/// collapse filter that `collect_paths` applies. Returns paths
	/// relative to the workspace root (same shape as
	/// `collect_paths` output). Used by the file tree's lazy-load
	/// flow: when the user expands a directory that was collapsed
	/// because git ignored it (`node_modules/`, `target/`, …) or
	/// because the depth cap stopped its enumeration short, this
	/// fetches its direct children so they slot into Pierre's
	/// path store without a full re-walk.
	///
	/// `max_depth` counts levels below `rel` (1 = direct children
	/// only). The walker still hides `.git/` and emits directories
	/// with a trailing slash. Errors if `rel` escapes the root.
	/// `depth_capped` in the response is populated when a child
	/// directory itself hit the cap — drilling deeper re-issues
	/// the call against that path.
	async fn collect_paths_under(&self, rel: &Utf8Path, max_depth: u32) -> MoonResult<CollectPathsResult>;

	/// Per-path git status for the file tree — added, modified,
	/// deleted, untracked, and ignored. Deleted entries are included
	/// even when the frontend hasn't enumerated them on disk; the
	/// tree re-adds those phantom rows so working-tree deletions
	/// don't silently disappear before the commit lands.
	///
	/// `paths` is only consulted in the walker fallback path (no git
	/// binary / no repo) where the tree's own enumeration is the
	/// only source of candidates. Inside a git repo we trust `git
	/// status` to surface the complete set of changed + ignored
	/// entries and ignore `paths` altogether.
	///
	/// Directories in the returned set carry a trailing `/`, to
	/// match `read_dir` output and Pierre's path convention; deleted
	/// entries never do (git can't track a directory).
	async fn git_status_entries(&self, paths: &[String]) -> MoonResult<Vec<GitStatusEntry>>;

	/// Discard working-tree *and* index changes for `paths` by
	/// restoring them to `HEAD`. Runs
	/// `git restore --source=HEAD --staged --worktree -- <paths>`
	/// in one subprocess so a multi-selection is atomic from git's
	/// perspective.
	///
	/// Callers are responsible for routing: only `modified` and
	/// `deleted` paths should come through here. Untracked files
	/// belong in `trash_path` (the backend has no special "discard"
	/// for them — removing them from disk *is* the discard). Added
	/// files (staged new files not yet in HEAD) would be *deleted*
	/// from disk by this call because HEAD doesn't know them; the
	/// frontend currently omits them from the menu rather than pick
	/// a default between "unstage" and "delete".
	async fn git_restore_paths(&self, paths: &[String]) -> MoonResult<()>;

	/// `git add -- <paths>` — stage the given paths' current
	/// working-tree content. Used by the merge-resolution flow
	/// to auto-clear a file's unmerged index entry once the
	/// user has saved it without conflict markers (saving alone
	/// doesn't touch the index, so without this the SCM panel
	/// would keep showing the row as conflicted until the
	/// commit step's `git add -A` ran).
	///
	/// Errors propagate git's stderr; the merge-resolution
	/// flow swallows them and lets the next status refresh
	/// surface any persistent mismatch (the file genuinely
	/// still has conflict markers, an index lock collision,
	/// etc.).
	async fn git_add_paths(&self, paths: &[String]) -> MoonResult<()>;

	/// Per-line blame for a single tracked file. Returns `None` when
	/// the path isn't inside a git repo, isn't tracked, or when the
	/// `git` binary can't be found — the UI skips blame annotations
	/// silently in all three cases, which is exactly the behaviour a
	/// file-tree that only shows a subset of tracked files wants.
	///
	/// The current version shells out to `git blame --porcelain -w`
	/// and parses the stable porcelain format. `gix` has blame
	/// support in progress but hasn't stabilised; swapping the
	/// implementation is a contained change behind this trait method.
	async fn git_blame(&self, path: &Utf8Path) -> MoonResult<Option<GitFileBlame>>;

	/// Build a GitHub permalink for `path` spanning lines
	/// `start_line..=end_line` (1-based, inclusive). The link pins
	/// the current `HEAD` commit SHA (not a branch ref) so it keeps
	/// pointing at the same bytes after later commits — matching what
	/// GitHub's own "Copy permalink" does.
	///
	/// Returns `Ok(None)` for every case the UI should treat as "no
	/// link available, grey the menu item out": the folder's
	/// `origin` / `upstream` remote isn't a recognised host
	/// (currently `github.com`), the repo has no commits yet, or
	/// `git` isn't on PATH. Path containment is enforced the same way
	/// as [`Self::git_blame`] (no absolute paths, no `..` escapes).
	async fn git_permalink(&self, path: &Utf8Path, start_line: u32, end_line: u32) -> MoonResult<Option<GitPermalink>>;

	/// Blob SHA of the working-tree version of `path` (`git
	/// hash-object`). Used to fingerprint reviewed-file marks
	/// (Phase 5.7): a "Viewed" tick is pinned to this SHA, and the
	/// frontend clears the mark when a later refresh reports a
	/// different SHA — i.e. the file changed since it was ticked.
	///
	/// Returns `Ok(None)` for the cases the UI treats as "can't
	/// fingerprint, leave unticked": the file doesn't exist on disk
	/// (e.g. a working-tree-deleted entry) or `git` isn't on PATH.
	/// Path containment is enforced the same way as
	/// [`Self::git_permalink`].
	async fn git_blob_sha(&self, path: &Utf8Path) -> MoonResult<Option<String>>;

	/// Publish a batch of local review-comment drafts to the current
	/// branch's GitHub PR as a single review (Phase 5.7.2). Shells
	/// out to `gh` (never a raw token), so auth is inherited from the
	/// user's `gh` login / the container's forwarded `GH_TOKEN`.
	///
	/// The flow: resolve the PR + head SHA via `gh pr view`; for each
	/// comment re-anchor its content fingerprint against the PR-head
	/// version of the file (`git show <head>:<path>`); post the
	/// survivors as one `event: COMMENT` review via `gh api`. Returns
	/// [`PublishReviewResult::NoPr`] when the branch has no open PR,
	/// or [`PublishReviewResult::Published`] with the count posted,
	/// the ids that couldn't be placed at the head (kept as local
	/// drafts), and the review URL.
	async fn publish_pr_review(&self, request: PublishReviewRequest) -> MoonResult<PublishReviewResult>;

	/// Contents of `path` at `HEAD`. Used as the "before" side of the
	/// editor's git diff view, and as the displayable text for a
	/// working-tree-deleted file whose bytes aren't on disk anymore.
	///
	/// Returns `Ok(None)` for anything the UI should surface as "no
	/// diff to show" rather than as an error: the path isn't inside a
	/// git repo, the file doesn't exist in `HEAD` (freshly-added /
	/// untracked), or the `git` binary can't be found. Binary files
	/// in `HEAD` also return `None` — the diff view only deals in
	/// text. Real errors (join failures, unreadable UTF-8 from a file
	/// we thought was text) still bubble up.
	async fn git_head_content(&self, path: &Utf8Path) -> MoonResult<Option<String>>;

	/// `git show <rev>:<path>` — same shape as
	/// [`git_head_content`] but for an arbitrary rev. The
	/// `Default` compare baseline reads the working tree's
	/// merge-base blob through this method; the diff view picks
	/// the rev based on the active folder's
	/// [`moon_protocol::git::CompareBaseline`].
	///
	/// `rev` is validated to be either the literal `"HEAD"` or a
	/// 40-character hex SHA: those are the only two shapes the
	/// frontend ever passes, and constraining rejects an
	/// adversarial caller from feeding `git show` a flag-shaped
	/// rev string. Same `Ok(None)` collapse rules as
	/// `git_head_content` (path missing at rev, binary blob, no
	/// repo, git absent).
	async fn git_ref_content(&self, rev: &str, path: &Utf8Path) -> MoonResult<Option<String>>;

	/// File-level diff between the working tree (committed +
	/// uncommitted) and the merge-base with the repo's default
	/// branch. The SCM panel's `Default` compare baseline reads
	/// this; the file tree's per-row decoration, the change
	/// gutter, and the diff view all swap their data source to
	/// the returned [`BranchDiffStatus`].
	///
	/// `Ok(None)` covers the "this comparison isn't applicable"
	/// states the UI silently downgrades to `Head` mode for: not
	/// a git repo, no `default_branch_remote_ref` resolvable,
	/// HEAD is detached, HEAD already points at the default
	/// branch's commit, or no merge-base exists. Real errors
	/// (join / spawn failures) still bubble up.
	async fn git_default_branch_diff(&self) -> MoonResult<Option<BranchDiffStatus>>;

	/// Lightweight branch / HEAD info for the SCM panel header.
	/// Returns the all-`None` default when the active folder isn't
	/// a git repo or `git` itself can't run — the UI treats that
	/// as "show no branch label" rather than a hard error so
	/// non-git workspaces still render cleanly.
	async fn git_branch(&self) -> MoonResult<GitBranchInfo>;

	/// Absolute host path to the repo's `.git/info/exclude` —
	/// resolved via `git rev-parse --git-common-dir`. Returns `None`
	/// when the folder isn't a git repo (no `.git` dir). The command
	/// palette uses this to open the exclude file for the active
	/// folder.
	async fn git_exclude_path(&self) -> MoonResult<Option<String>>;

	/// Local branch names only (`git for-each-ref refs/heads`),
	/// newest committer-date first and capped, with no `gh` /
	/// network round-trip. Cheaper than [`Self::branch_list`] —
	/// used by the terminal `Ctrl+K` command suggester to give
	/// the model the set of real branch names so it can match a
	/// user's abbreviated reference. Empty vec when the folder
	/// isn't a git repo or `git` can't run.
	async fn git_local_branches(&self) -> MoonResult<Vec<String>>;

	/// Stage every working-tree change (`git add -A`) and commit
	/// with `message`. When `amend` is `true`, `git commit --amend`
	/// replaces the previous commit instead of creating a new one;
	/// an empty `message` in that mode falls through to
	/// `--no-edit` (keep the previous commit's message verbatim,
	/// just absorb the newly-staged changes).
	///
	/// Errors with a useful message when:
	///   - The active folder isn't a git repo.
	///   - `message` is empty *and* `amend` is false (a fresh commit
	///     needs a subject; an amend without a new message is
	///     valid via `--no-edit`).
	///   - There's nothing to commit (clean tree, non-amend mode).
	///   - The author identity isn't configured (`user.name` /
	///     `user.email` missing) — we surface git's own complaint
	///     so the user can fix it from the terminal.
	///
	/// The "stage everything" gesture matches the SCM panel's
	/// "commit current changes" affordance — same behaviour as
	/// `git commit -a` plus untracked-file inclusion. Per-file
	/// staging UI is a later phase.
	async fn git_commit(&self, message: &str, amend: bool) -> MoonResult<GitCommitResult>;

	/// Create a fresh branch from the current `HEAD`, switch to it,
	/// then stage everything and commit with `message`. The caller
	/// is responsible for picking a sensible name; the host
	/// validates with `git check-ref-format --branch <name>` before
	/// touching anything so a malformed name fails fast without
	/// leaving the repo half-mutated.
	///
	/// Errors when:
	///   - The active folder isn't a git repo.
	///   - `branch` is empty or fails `check-ref-format`.
	///   - A branch with that name already exists locally — we don't
	///     guess between "switch to it and commit" and "rename it";
	///     the user gets git's own "already exists" message and can
	///     pick a different name.
	///   - The commit step fails (empty `message`, nothing to commit,
	///     missing identity) — same diagnostics as
	///     [`Self::git_commit`].
	///
	/// On any failure after the branch was created we attempt to
	/// switch back to the original branch and delete the freshly-
	/// created one so the user's HEAD position is what they expect;
	/// best-effort, errors are logged but not surfaced (the original
	/// commit failure is the actionable one).
	async fn git_commit_on_new_branch(&self, branch: &str, message: &str) -> MoonResult<GitCommitResult>;

	/// Create a linked worktree at `path` (`git worktree add`). Backs
	/// worktree-backed coder sessions (ADR 0028): an isolated session
	/// checks its branch out into its own working directory so several
	/// agents can work one repo without colliding. `branch` selects a
	/// fresh branch off `HEAD` ([`WorktreeBranch::New`]) or an
	/// existing local/remote branch ([`WorktreeBranch::Existing`]) —
	/// either way the parent's checkout is untouched.
	///
	/// A new branch name is validated with `git check-ref-format
	/// --branch` before anything is touched. Errors when the active
	/// folder isn't a git repo, `path` is non-empty, or (for
	/// `Existing`) the branch is already checked out in another
	/// worktree — git's own stderr is surfaced verbatim. Returns the
	/// freshly-created [`GitWorktree`].
	async fn git_worktree_add(&self, path: &Utf8Path, branch: WorktreeBranch) -> MoonResult<GitWorktree>;

	/// Create a worktree at `path` on `branch`, optionally **moving**
	/// the active checkout's current branch into it first (ADR 0028 —
	/// "move this session into a worktree"). When `reset_main_to` is
	/// `Some(default)`, the main tree is switched to `default` (which
	/// frees the current branch so it can be checked out in the
	/// worktree); the tree must be clean or this errors, and a failed
	/// worktree-add rolls the switch back. When `None`, the main tree
	/// is left untouched (the fresh-branch case). The whole switch +
	/// add runs under the git lock so nothing interleaves.
	async fn git_worktree_add_moving(
		&self,
		path: &Utf8Path,
		branch: WorktreeBranch,
		reset_main_to: Option<String>,
	) -> MoonResult<GitWorktree>;

	/// List the repository's working trees (`git worktree list
	/// --porcelain`), including the main one (`is_main`). Used to
	/// reconcile the IDE's session→worktree bindings against reality
	/// at startup and on discard.
	async fn git_worktree_list(&self) -> MoonResult<Vec<GitWorktree>>;

	/// Prune the linked worktree at `path` (`git worktree remove`).
	/// `force` passes `--force`, needed when the worktree has
	/// uncommitted changes or is locked — the caller gates that
	/// behind a confirm so an agent's work isn't silently dropped.
	/// The branch is never deleted here; pruning a worktree leaves
	/// its branch in place as the deliverable.
	///
	/// `path` is given **as the active shell target sees it**: a host
	/// path in host mode, the in-container
	/// `/workspace/<parent>/.worktrees/…` path when the workspace runs
	/// in a container. The worktree's relative git links resolve under
	/// either mount (ADR 0029), so the caller just passes the path for
	/// the active target.
	async fn git_worktree_remove(&self, path: &Utf8Path, force: bool) -> MoonResult<()>;

	/// Lightweight diff summary of the working tree against `HEAD`,
	/// suitable for feeding to a small LLM that's suggesting a
	/// branch / commit name. Output is `git diff HEAD --stat -M -C`
	/// trimmed to a manageable size — file paths plus per-file
	/// `+/-` counts plus the totals line. Returns an empty string
	/// when there's nothing to summarise (clean tree, not a repo,
	/// `git` not installed) — the caller decides what to do with
	/// the void.
	async fn git_diff_summary(&self) -> MoonResult<String>;

	/// `git push` with no arguments — uses the configured upstream
	/// for the current branch. Errors propagate git's own stderr
	/// verbatim so messages like "the current branch X has no
	/// upstream branch" stay actionable. The SCM panel calls
	/// `git_publish_branch` instead when no upstream is set; the
	/// distinction is made client-side from `GitBranchInfo`.
	async fn git_push(&self) -> MoonResult<()>;

	/// `git push -u origin HEAD` — first-push affordance for a
	/// freshly-created local branch with no upstream yet. Hardcoded
	/// to `origin` (matching the "hardcode first, configure later"
	/// rule); a multi-remote chooser is a later concern. Errors
	/// (no `origin` remote, auth, network) propagate git's stderr
	/// verbatim.
	async fn git_publish_branch(&self) -> MoonResult<()>;

	/// `git pull --rebase` — replay local commits on top of the
	/// upstream tip. On conflict (or any other rebase failure) we
	/// run `git rebase --abort` before returning so the working
	/// tree is left exactly as it was before the call; the user
	/// can then resolve in a terminal and retry. Errors propagate
	/// git's stderr verbatim — conflicts, dirty-tree refusals,
	/// and missing-upstream messages all stay readable.
	async fn git_pull(&self) -> MoonResult<()>;

	/// `git merge --no-edit <remote_ref>` — fast-forward when
	/// possible, otherwise create a merge commit with git's
	/// default subject. The SCM panel calls this when the user
	/// clicks "Update from main"; `<remote_ref>` is the same
	/// `default_branch_remote_ref` (e.g. `"origin/main"`) the
	/// branch info exposes, so the backend doesn't have to
	/// re-resolve the default. We rely on the periodic auto-fetch
	/// to keep the remote-tracking ref current; this op never
	/// fetches itself, matching the "merge means merge" contract
	/// the button label sets up. Errors (conflicts, dirty tree,
	/// unknown ref) propagate git's stderr verbatim — including
	/// the `CONFLICT (content)` lines that the SCM panel uses to
	/// shift into merge-in-progress mode after the call returns
	/// (the panel then polls `git_merge_state`, which reads
	/// `.git/MERGE_HEAD` directly).
	async fn git_merge_default_branch(&self, remote_ref: &str) -> MoonResult<()>;

	/// Snapshot of the working tree's in-flight merge — whether
	/// one is in progress, the ref being merged, the default
	/// commit message git wrote to `.git/MERGE_MSG`, and the
	/// list of paths with unresolved conflicts.
	///
	/// Read from on-disk metadata (`.git/MERGE_HEAD`,
	/// `.git/MERGE_MSG`) plus a single `git ls-files --unmerged`
	/// — no `git status` invocation, so this is cheap to poll
	/// when the SCM panel needs to refresh (after every
	/// commit / restore / merge / abort gesture, and whenever the
	/// fs-watcher reports a write inside `.git/`).
	///
	/// Returns the all-default value (`in_progress: false`) for
	/// every "no merge to report" case: not a repo, git
	/// unavailable, `MERGE_HEAD` missing. Real I/O errors (a
	/// `MERGE_HEAD` we can't read, an `ls-files` failure)
	/// downgrade to the same default — the panel should never
	/// crash on a transient probe glitch.
	async fn git_merge_state(&self) -> MoonResult<GitMergeState>;

	/// `git merge --abort` — wind back an in-progress merge to
	/// the pre-merge `HEAD`, restoring the working tree and
	/// clearing `.git/MERGE_HEAD` / `.git/MERGE_MSG`. The SCM
	/// panel's "Abort merge" button calls this when the user
	/// decides the merge isn't worth resolving. Errors
	/// (no merge in progress, dirty pre-merge state git
	/// refuses to clobber) propagate stderr verbatim.
	async fn git_merge_abort(&self) -> MoonResult<()>;

	/// Subject + body of the current `HEAD` commit (`git log -1
	/// --pretty=%B`). Used by the SCM panel to prefill the commit
	/// composer when "amend" is toggled on with an empty message
	/// — the user almost always wants to start from the existing
	/// message and edit, not re-type it. Returns an empty string
	/// when the repo has no commits yet, isn't a repo at all, or
	/// git is unavailable; callers treat the empty case as "nothing
	/// to prefill" without branching on `Result`.
	async fn git_head_commit_message(&self) -> MoonResult<String>;

	/// Working-tree diff against `HEAD` (`git diff HEAD --no-color`),
	/// capped at ~64 KB so a sprawling refactor doesn't blow up the
	/// LLM prompt that consumes this. Used by the SCM panel's "AI
	/// commit message" sparkle button. The cap is byte-based and
	/// truncates at the next newline boundary so half-rendered hunk
	/// headers don't confuse the model. Returns an empty string when
	/// there's nothing to diff (clean tree, not a repo, git
	/// unavailable).
	async fn git_diff_patch(&self) -> MoonResult<String>;

	/// Recent commits on the current branch (`git log --pretty=…`),
	/// capped at `limit` entries (default 50). Returns short SHA,
	/// full SHA, subject, author, and a relative date per commit —
	/// enough for the SCM panel's recent-commits list without pulling
	/// the full body / parents / stat. Empty vec when the repo has no
	/// commits yet, isn't a repo, or git is unavailable.
	async fn git_log(&self, limit: u32) -> MoonResult<Vec<CommitEntry>>;

	/// File-level diff for a single commit vs its first parent:
	/// `git diff --name-status -z --no-renames <parent> <sha>`.
	/// Resolves the parent SHA from `<sha>^` (so the frontend can pass
	/// the commit SHA directly; `is_safe_rev` would reject the `^`
	/// suffix downstream). Returns the parent SHA, commit SHA, subject,
	/// and the changed-file list so the `commit://` pseudo-tab can
	/// render per-file sections. `Ok(None)` when the commit doesn't
	/// exist, isn't a repo, or git is unavailable.
	async fn git_commit_diff(&self, sha: &str) -> MoonResult<Option<CommitDiff>>;

	/// Recent branches + open PRs for the active folder, formatted
	/// for the branch-switcher palette. Two sections in the
	/// returned [`BranchList`]:
	///
	/// 1. `local` — `git for-each-ref refs/heads`, sorted newest
	///    first by committer date, capped at 20. The repo's
	///    default branch (`main` / `master`, from `origin/HEAD`)
	///    is always included even when the cap would drop it, and
	///    its row carries `is_default = true` so the palette can
	///    keep it visible — switching back to it is the most
	///    common destination.
	/// 2. `prs` — open GitHub PRs via `gh pr list` (capped at
	///    30). `pr_scope == All` is "every open PR";
	///    `Participating` runs two `--search` queries
	///    (`involves:@me` + `review-requested:@me`) in parallel
	///    and merges them.
	///    Empty when `gh` isn't installed (`pr_status =
	///    GhMissing`), when `gh` isn't authenticated (`GhNotAuthed`),
	///    when the active folder's `origin` / `upstream` isn't
	///    GitHub (`NotGithub`), or when the call exited non-zero
	///    (`Failed { detail }`). The frontend uses
	///    [`PrListStatus`] verbatim to render the section's
	///    empty-state row.
	///
	/// Both sections are produced in parallel — local always
	/// returns in single-digit milliseconds; the gh probe can take
	/// a network round-trip but the local list paints
	/// independently. Failures in either are best-effort: a broken
	/// git or gh leaves the affected section empty rather than
	/// taking down the whole call.
	async fn branch_list(&self, pr_scope: PrListScope) -> MoonResult<BranchList>;

	/// URL of the open GitHub PR whose head ref matches the active
	/// folder's current branch, if one exists. Single
	/// `gh pr list --head <branch> --state open --json url --limit 1`
	/// call — much cheaper than [`Self::branch_list`]'s full PR
	/// fetch, but still a network round-trip, so callers should
	/// refresh on branch change rather than on every status pass.
	///
	/// Returns `Ok(None)` for every "no existing PR" case the SCM
	/// panel needs to fall back from: detached HEAD, non-GitHub
	/// remote, `gh` missing or not authenticated, `gh` exited
	/// non-zero, no PR open for this branch, or the call timed out.
	/// The intent is "give me a URL to navigate to if you have
	/// one"; ambiguity collapses to `None` so the UI's create-PR
	/// fallback stays consistent.
	async fn git_existing_pr_url(&self) -> MoonResult<Option<String>>;

	/// Switch the active folder to `target`. `Local { name }` runs
	/// `git switch <name>`; `Pr { number }` runs
	/// `gh pr checkout <number>` so cross-fork PRs get the
	/// fork-fetching dance for free.
	///
	/// Errors propagate stderr verbatim (dirty-tree refusal,
	/// missing branch, gh auth required, network failure) so the
	/// user gets the actionable hint without us re-wrapping it.
	async fn branch_switch(&self, target: &BranchSwitchTarget) -> MoonResult<()>;

	/// `git fetch --quiet --no-tags` against the current branch's
	/// upstream remote (defaults to `origin`). Used by the periodic
	/// auto-fetch loop in the SCM panel so the "Sync Changes" button
	/// surfaces when commits land upstream — `git_branch`'s
	/// ahead/behind read is local-ref-only, so without a fetch the
	/// `behind` counter never moves on its own.
	///
	/// Best-effort. We pin `GIT_TERMINAL_PROMPT=0` and zero out
	/// `GIT_ASKPASS` / `SSH_ASKPASS` so a remote needing
	/// credentials fails fast instead of hanging on a TTY prompt
	/// that the desktop process can't even render. Capped at 30s
	/// to bound a hung fetch (DNS stall, dropped TCP) — we'd rather
	/// retry on the next tick than starve the work pool.
	///
	/// Errors propagate git's stderr verbatim. Common ones the UI
	/// is expected to swallow (offline / no remote / no upstream /
	/// auth refused) are still returned so callers can choose to
	/// surface them in dev mode; the auto-fetch loop downgrades
	/// them to `tracing::debug!`.
	async fn git_fetch(&self) -> MoonResult<()>;
}

pub struct LocalHost {
	root: Utf8PathBuf,
	editorconfig: EditorConfigService,
	lint_staged: LintStagedService,
	/// Where to spawn host-issued subprocesses (today: format-on-save).
	/// `None` → always run on the host's userland; injected by the
	/// Tauri layer so format-on-save uses the workspace shell
	/// container when it's `Running`. See `crate::shell` and
	/// ADR 0002.
	shell_resolver: Option<ShellResolverHandle>,
	/// Diagnostic log sink. `None` in tests / pure-library use; the
	/// Tauri layer plugs in `AppState::logs` at startup so
	/// format-on-save decisions land in the bottom-panel logs view
	/// under source `"format-on-save"`. See test plan 0069.
	log_sink: Option<Arc<crate::logs::LogSink>>,
	/// Serialises every git invocation against this folder. The
	/// auto-fetch loop, status polling, blame, ref reads, etc. all
	/// briefly take `.git/index.lock`; without coordination they
	/// race against user-initiated commits whose hooks (lint-staged,
	/// pre-commit) do their own stash dance. The race surfaces as
	/// data loss when a hook's `git stash apply` is interrupted
	/// mid-write. Holding this mutex around every git subprocess
	/// closes the window. FIFO so background ops can't starve the
	/// user. See [ADR 0015](../../specs/decisions/0015-git-serialisation.md).
	git_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl LocalHost {
	pub fn new(root: Utf8PathBuf) -> Self {
		Self {
			editorconfig: EditorConfigService::new(root.clone()),
			lint_staged: LintStagedService::new(root.clone()),
			root,
			shell_resolver: None,
			log_sink: None,
			git_mutex: Arc::new(tokio::sync::Mutex::new(())),
		}
	}

	/// Acquire the per-folder git mutex as an owned guard. The
	/// `OwnedMutexGuard` is `Send + 'static`, so the caller can
	/// move it into a `tokio::task::spawn_blocking` closure and
	/// keep the lock held across the full subprocess lifetime —
	/// which is the point: git's index lock isn't process-aware,
	/// so two of *our own* git commands racing is enough to break
	/// a hook mid-`git stash apply`. See [ADR 0015].
	///
	/// [ADR 0015]: ../../specs/decisions/0015-git-serialisation.md
	async fn git_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
		self.git_mutex.clone().lock_owned().await
	}

	/// Re-stat `path` after the lint-staged chain ran, returning a
	/// fresh `WriteFileResult` so the caller's `mtime_ms` /
	/// `bytes_written` match disk truth. Errors collapse to `None`
	/// — same posture as the rest of the format pipeline: a stat
	/// failure on a file we just wrote is worth a warn but never
	/// aborts the save.
	async fn restat_after_format(&self, path: &Utf8Path) -> Option<WriteFileResult> {
		let abs_str = self.absolute_path(path).await.ok()?;
		let abs = Utf8PathBuf::from(abs_str);
		match tokio::fs::metadata(abs.as_std_path()).await {
			Ok(metadata) => Some(WriteFileResult {
				mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
				bytes_written: metadata.len(),
			}),
			Err(err) => {
				tracing::warn!(path = %abs, %err, "format-on-save: post-format stat failed");
				None
			}
		}
	}

	/// Return the current `(mtime, size)` pair for `path` shaped as
	/// a `WriteFileResult`. Used by `format_file` when the
	/// editorconfig pre-save was a no-op and the lint-staged chain
	/// didn't run either — the caller still needs a result, and the
	/// current stat is the most accurate thing we can hand back.
	async fn stat_as_write_result(&self, path: &Utf8Path) -> MoonResult<WriteFileResult> {
		let resolved = self.resolve(path)?;
		let metadata = tokio::fs::metadata(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;
		Ok(WriteFileResult {
			mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
			bytes_written: metadata.len(),
		})
	}

	/// Plug in a [`ShellResolverHandle`] so format-on-save (and any
	/// future host-issued subprocess) can route to the workspace
	/// shell container when it's running. With no resolver every
	/// subprocess stays on the host — the existing behaviour.
	pub fn with_shell_resolver(mut self, resolver: ShellResolverHandle) -> Self {
		self.shell_resolver = Some(resolver);
		self
	}

	/// Plug in the workspace's shared [`LogSink`] so format-on-save
	/// emits user-visible breadcrumbs (decision points, command
	/// runs, exit codes) under source `"format-on-save"`. Tests
	/// and non-Tauri callers can leave it unset; emits become a
	/// no-op.
	pub fn with_log_sink(mut self, sink: Arc<crate::logs::LogSink>) -> Self {
		self.log_sink = Some(sink);
		self
	}

	/// Convenience: emit one info-level entry on source
	/// `format-on-save`. No-op when no sink is installed (tests
	/// and library use). The string is built lazily by the
	/// closure so callers pay nothing for a missing sink.
	fn format_log<F>(&self, level: crate::logs::LogLevel, msg: F)
	where
		F: FnOnce() -> String,
	{
		let Some(sink) = &self.log_sink else { return };
		sink.emit("format-on-save", level, msg());
	}

	pub fn root(&self) -> &Utf8Path {
		&self.root
	}

	/// Resolve the target shell for subprocesses spawned against
	/// this host. Defaults to [`ShellTarget::Host`] when no resolver
	/// is plugged in.
	async fn shell_target(&self) -> ShellTarget {
		match &self.shell_resolver {
			Some(handle) => handle.resolve(&self.root).await,
			None => ShellTarget::Host,
		}
	}

	/// Run a formatter for `rel` against the file already on disk.
	/// Two-layer dispatch:
	///
	/// 1. **lint-staged** is the team's per-repo source of truth and
	///    wins whenever it has a matching rule. The command is spawned
	///    with the absolute file path appended as a positional argument
	///    and is expected to mutate the file in place — same shape
	///    `bun run lint-staged` uses on commit.
	/// 2. **Language defaults** (see [`format::default_format_command`])
	///    fire only when (1) didn't apply, so projects that don't ship a
	///    `.lintstagedrc.json` (`workloads` is pure Rust + Cargo, no JS
	///    tooling) still get format-on-save for the languages we have
	///    a default for. lint-staged still takes precedence whenever
	///    it matches, so adding a default never overrides an explicit
	///    team config.
	///
	/// Every miss path is logged inside [`crate::format::run_formatter`].
	///
	/// **Chain semantics**: when the matched lint-staged rule has more
	/// than one command, **every** command in the chain runs in order.
	/// Unlike `bun run lint-staged` on commit, format-on-save **does
	/// not** abort on the first non-zero exit / timeout / spawn error
	/// — each command's failure logs its own warning and the next one
	/// spawns regardless. The rationale: format-on-save is best-effort
	/// by design, and a flaky linter must not stop the trailing
	/// `prettier -w` from reaching the file the user just saved.
	/// See ADR 0013 § Chain semantics.
	///
	/// Returns `true` when a command ran (whether successfully or not —
	/// either way the on-disk bytes may have changed and the caller
	/// should re-read). `false` means "nothing happened, the caller
	/// can keep the pre-chain bytes".
	async fn run_formatter_chain(&self, rel: &Utf8Path) -> bool {
		// `absolute_path` is the only way to get the host-side absolute
		// path for a workspace-relative input. The host-to-container
		// translation for the `Container` shell target rebases this
		// through the bind mount so the in-container process sees the
		// same file under `/workspace/<basename>/...`.
		let abs_str = match self.absolute_path(rel).await {
			Ok(s) => s,
			Err(err) => {
				self.format_log(crate::logs::LogLevel::Warn, || {
					format!("could not resolve absolute path for {rel}: {err}; nothing ran")
				});
				return false;
			}
		};
		let abs = Utf8PathBuf::from(abs_str);
		let target = self.shell_target().await;
		let target_label = match &target {
			ShellTarget::Host => "host".to_owned(),
			ShellTarget::Container { container_name, .. } => format!("container {container_name}"),
		};
		self.format_log(crate::logs::LogLevel::Info, || {
			format!("save: {rel} (formatter dispatch target = {target_label})")
		});

		if self.run_lint_staged_chain_for(rel, &abs, &target).await {
			return true;
		}
		if self.run_default_formatter_for(&abs, &target).await {
			return true;
		}
		self.format_log(crate::logs::LogLevel::Info, || {
			let ext = abs.extension().unwrap_or("");
			format!(
				"no formatter configured for {rel} (no lint-staged match, no language default for .{ext}); bytes left as-is"
			)
		});
		false
	}

	/// Layer 1 of `run_formatter_chain`: matched lint-staged rule.
	/// Returns `true` when a command ran.
	async fn run_lint_staged_chain_for(&self, rel: &Utf8Path, abs: &Utf8Path, target: &ShellTarget) -> bool {
		let rules = match self.lint_staged.for_path(rel).await {
			Ok(r) => r,
			Err(err) => {
				self.format_log(crate::logs::LogLevel::Warn, || {
					format!("lint-staged: failed to load config: {err}")
				});
				return false;
			}
		};
		// Surface parse-time warnings (likely-broken globs etc.) on
		// the format-on-save panel. Deduped process-wide on the
		// warning string itself so a misconfigured pattern logs
		// once, not on every save until the user fixes it.
		for w in rules.parse_warnings() {
			if warn_lint_staged_config_once(w) {
				self.format_log(crate::logs::LogLevel::Warn, || format!("lint-staged config: {w}"));
			}
		}
		if rules.is_empty() {
			self.format_log(crate::logs::LogLevel::Info, || {
				"lint-staged: no `.lintstagedrc.*` / `package.json#lint-staged` between this file and the workspace root".into()
			});
			return false;
		}
		let Some(commands) = rules.match_commands(abs.as_std_path()) else {
			self.format_log(crate::logs::LogLevel::Info, || {
				let config = rules
					.config_dir()
					.map(|d| d.as_str().to_owned())
					.unwrap_or_else(|| self.root.as_str().to_owned());
				format!("lint-staged: config found at {config} but no glob matched {abs}")
			});
			return false;
		};
		let total = commands.len();
		if total == 0 {
			return false;
		}
		// `config_dir` is `Some` whenever `match_commands` returned a
		// hit (the rule came from a real file on disk); the workspace
		// root is just a defensive fallback the type system asks for.
		let cwd = rules.config_dir().unwrap_or(&self.root).to_path_buf();
		// Run every command in the chain in order. Failures don't
		// abort: format-on-save is best-effort, and a flaky linter
		// must not stop the trailing `prettier -w` (or equivalent)
		// from reaching the file the user just saved. See
		// ADR 0013 § Chain semantics.
		for (idx, cmd) in commands.iter().enumerate() {
			self.format_log(crate::logs::LogLevel::Info, || {
				let step = if total > 1 {
					format!(" (step {}/{total})", idx + 1)
				} else {
					String::new()
				};
				format!("lint-staged: running `{cmd}` in {cwd}{step}")
			});
			let started = std::time::Instant::now();
			let ok = format::run_formatter(&self.root, &cwd, abs, cmd, target).await;
			let elapsed_ms = started.elapsed().as_millis();
			let outcome_level = if ok {
				crate::logs::LogLevel::Info
			} else {
				crate::logs::LogLevel::Warn
			};
			self.format_log(outcome_level, || {
				let verb = if ok { "succeeded" } else { "failed (see warnings above)" };
				format!("lint-staged: `{cmd}` {verb} in {elapsed_ms}ms")
			});
		}
		true
	}

	/// Layer 2 of `run_formatter_chain`: language-default formatter
	/// keyed by file extension. The resolver in
	/// [`format::default_format_command`] decides both the command
	/// to run and the `cwd` to run it in, so a language with
	/// project-local tooling (Python's `.venv/bin/ruff`) can pin
	/// the cwd to the project root while a language with no such
	/// requirement (Rust → `rustfmt`) falls through to the file's
	/// parent directory. Anchoring `cwd` like this lets a relative
	/// bin token resolve correctly on both host and container
	/// (`docker exec -w <translated_cwd> … .venv/bin/ruff`) without
	/// us having to translate the bin token itself.
	async fn run_default_formatter_for(&self, abs: &Utf8Path, target: &ShellTarget) -> bool {
		let Some(default) = format::default_format_command(abs) else {
			self.format_log(crate::logs::LogLevel::Info, || {
				let ext = abs.extension().unwrap_or("");
				format!("default formatter: no built-in rule for .{ext}")
			});
			return false;
		};
		self.format_log(crate::logs::LogLevel::Info, || {
			format!("default formatter: running `{}` in {}", default.command, default.cwd)
		});
		let started = std::time::Instant::now();
		let ok = format::run_formatter(&self.root, &default.cwd, abs, &default.command, target).await;
		let elapsed_ms = started.elapsed().as_millis();
		let outcome_level = if ok {
			crate::logs::LogLevel::Info
		} else {
			crate::logs::LogLevel::Warn
		};
		self.format_log(outcome_level, || {
			let verb = if ok { "succeeded" } else { "failed (see warnings above)" };
			format!("default formatter: `{}` {verb} in {elapsed_ms}ms", default.command)
		});
		true
	}

	/// Resolve a workspace-relative or absolute path against the workspace root.
	/// Rejects paths that escape the root via `..`.
	fn resolve(&self, path: &Utf8Path) -> MoonResult<Utf8PathBuf> {
		let candidate = if path.is_absolute() {
			path.to_path_buf()
		} else {
			self.root.join(path)
		};

		// Canonicalize via std::path then re-wrap. We accept the trade-off
		// that the path must exist for canonicalization to work; for create
		// operations we canonicalize the parent instead. Done in callers.
		let canonical = std::fs::canonicalize(candidate.as_std_path())
			.map_err(MoonError::from)
			.and_then(|p| {
				Utf8PathBuf::from_path_buf(p).map_err(|p| MoonError::Internal(format!("non-utf8 path: {}", p.display())))
			})?;

		if !canonical.starts_with(&self.root) {
			return Err(MoonError::PermissionDenied(format!(
				"path {canonical} escapes workspace root"
			)));
		}
		Ok(canonical)
	}

	/// Resolve a write target's parent directory, creating any
	/// missing intermediate directories (`mkdir -p`) while staying
	/// inside the workspace root.
	///
	/// `resolve` can't help here: it canonicalizes, which requires
	/// the path to already exist. Instead we walk up to the deepest
	/// existing ancestor, canonicalize *that* (resolving any symlinks
	/// and `..` in the existing portion) and confirm it sits inside
	/// the root, then create the missing tail beneath it. The missing
	/// components don't exist yet, so they can't be symlinks and can't
	/// escape the anchor; we still reject a `.`/`..` tail component so
	/// a crafted relative path can't climb back out before the
	/// directories are created.
	async fn resolve_for_write(&self, parent: &Utf8Path) -> MoonResult<Utf8PathBuf> {
		let candidate = if parent.is_absolute() {
			parent.to_path_buf()
		} else {
			self.root.join(parent)
		};

		// Walk up until we hit an ancestor we can canonicalize,
		// collecting the not-yet-existing tail (leaf-first).
		let mut missing: Vec<String> = Vec::new();
		let mut cursor = candidate.clone();
		let anchor = loop {
			match std::fs::canonicalize(cursor.as_std_path()) {
				Ok(p) => {
					break Utf8PathBuf::from_path_buf(p)
						.map_err(|p| MoonError::Internal(format!("non-utf8 path: {}", p.display())))?;
				}
				Err(_) => {
					// `file_name` is `None` for a `.` / `..` tail, so a
					// path trying to climb out surfaces as an error
					// rather than a stray `mkdir`.
					let name = cursor
						.file_name()
						.ok_or_else(|| MoonError::invalid("path component is not a valid directory name"))?;
					missing.push(name.to_string());
					cursor = cursor
						.parent()
						.ok_or_else(|| MoonError::invalid("path has no parent directory"))?
						.to_path_buf();
				}
			}
		};

		if !anchor.starts_with(&self.root) {
			return Err(MoonError::PermissionDenied(format!(
				"path {anchor} escapes workspace root"
			)));
		}

		// Rebuild the tail root-first under the canonical anchor.
		let mut resolved = anchor;
		for name in missing.iter().rev() {
			resolved = resolved.join(name);
		}

		if !missing.is_empty() {
			tokio::fs::create_dir_all(resolved.as_std_path())
				.await
				.map_err(MoonError::from)?;
		}
		Ok(resolved)
	}
}

#[async_trait]
impl WorkspaceHost for LocalHost {
	async fn read_dir(&self, path: &Utf8Path) -> MoonResult<Vec<DirEntry>> {
		let resolved = self.resolve(path)?;
		let mut read = tokio::fs::read_dir(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;

		let mut out = Vec::new();
		while let Some(entry) = read.next_entry().await.map_err(MoonError::from)? {
			let file_type = entry.file_type().await.map_err(MoonError::from)?;
			let kind = if file_type.is_dir() {
				EntryKind::Dir
			} else if file_type.is_symlink() {
				EntryKind::Symlink
			} else if file_type.is_file() {
				EntryKind::File
			} else {
				EntryKind::Other
			};

			let name = entry.file_name().to_string_lossy().to_string();

			// Skip directories that are noisy and never useful in the tree.
			// `.git/` alone is enough today; Phase 5 will replace this with a
			// gitignore-aware filter and visual fading instead of hiding.
			if matches!(kind, EntryKind::Dir) && name == ".git" {
				continue;
			}

			let metadata = entry.metadata().await.ok();
			let size = metadata.as_ref().filter(|m| m.is_file()).map(|m| m.len());
			let mtime_ms = metadata
				.as_ref()
				.and_then(|m| m.modified().ok())
				.and_then(system_time_to_ms);

			let entry_path = Utf8PathBuf::from_path_buf(entry.path())
				.map_err(|p| MoonError::Internal(format!("non-utf8 dir entry: {}", p.display())))?;

			// The UI only ever sees paths relative to the workspace root.
			// This keeps it portable across hosts (a path string is meaningful
			// independent of where the workspace happens to live on disk).
			let rel = entry_path
				.strip_prefix(&self.root)
				.map(|p| p.to_string())
				.unwrap_or_else(|_| entry_path.to_string());

			out.push(DirEntry {
				is_hidden: name.starts_with('.'),
				name,
				path: rel,
				kind,
				size,
				mtime_ms,
			});
		}

		out.sort_by(|a, b| match (a.kind, b.kind) {
			(EntryKind::Dir, EntryKind::Dir) => a.name.cmp(&b.name),
			(EntryKind::Dir, _) => std::cmp::Ordering::Less,
			(_, EntryKind::Dir) => std::cmp::Ordering::Greater,
			_ => a.name.cmp(&b.name),
		});

		Ok(out)
	}

	async fn read_file(&self, path: &Utf8Path) -> MoonResult<ReadFileResult> {
		let resolved = self.resolve(path)?;
		let bytes = tokio::fs::read(resolved.as_std_path()).await.map_err(MoonError::from)?;

		let metadata = tokio::fs::metadata(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;
		let mtime_ms = metadata.modified().ok().and_then(system_time_to_ms);

		if looks_binary(&bytes) {
			return Ok(ReadFileResult {
				text: String::new(),
				mtime_ms,
				is_binary: true,
			});
		}

		let text = String::from_utf8(bytes).map_err(|e| MoonError::IoError(e.to_string()))?;

		Ok(ReadFileResult {
			text,
			mtime_ms,
			is_binary: false,
		})
	}

	async fn write_file(&self, path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult> {
		// Write goes through the parent directory because the file may not yet exist.
		let candidate = if path.is_absolute() {
			path.to_path_buf()
		} else {
			self.root.join(path)
		};
		let parent = candidate
			.parent()
			.ok_or_else(|| MoonError::invalid("path has no parent directory"))?;

		// Auto-mkdir the parent chain (`mkdir -p`) so a write to a
		// not-yet-existing subtree just works; `resolve_for_write`
		// keeps the creation inside the workspace root.
		let resolved_parent = self.resolve_for_write(parent).await?;
		let file_name = candidate
			.file_name()
			.ok_or_else(|| MoonError::invalid("path has no file name"))?;
		let resolved = resolved_parent.join(file_name);

		tokio::fs::write(resolved.as_std_path(), text.as_bytes())
			.await
			.map_err(MoonError::from)?;

		// `.editorconfig` saves invalidate the resolution cache; the
		// next `editorconfig_for` call picks up the new rules. We clear
		// the whole cache rather than the affected subtree because
		// editorconfig's upward-walk semantics mean a single file can
		// influence anything below it; figuring out exactly which
		// directories that touches isn't worth the bookkeeping.
		if file_name == ".editorconfig" {
			self.editorconfig.clear().await;
		}
		// Same story for the lint-staged map: `.lintstagedrc.json` and
		// any `package.json` carrying a `lint-staged` field can change
		// what formatter applies to files anywhere below them. We don't
		// know whether a `package.json` was previously a config-source
		// (it depends on whether it had the `lint-staged` field), so we
		// clear unconditionally on any `package.json` save.
		if file_name == ".lintstagedrc.json" || file_name == "package.json" {
			self.lint_staged.clear().await;
		}

		let metadata = tokio::fs::metadata(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;

		Ok(WriteFileResult {
			mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
			bytes_written: text.len() as u64,
		})
	}

	async fn create_file(&self, path: &Utf8Path) -> MoonResult<()> {
		let candidate = if path.is_absolute() {
			path.to_path_buf()
		} else {
			self.root.join(path)
		};
		let parent = candidate
			.parent()
			.ok_or_else(|| MoonError::invalid("path has no parent directory"))?;
		let resolved_parent = self.resolve(parent)?;
		let file_name = candidate
			.file_name()
			.ok_or_else(|| MoonError::invalid("path has no file name"))?;
		let resolved = resolved_parent.join(file_name);

		// `OpenOptions::create_new(true)` is the atomic "create or
		// fail-if-exists" primitive — sidesteps the TOCTOU window of
		// a separate `metadata` check followed by `write`.
		let std_path = resolved.into_std_path_buf();
		tokio::task::spawn_blocking(move || {
			std::fs::OpenOptions::new()
				.write(true)
				.create_new(true)
				.open(&std_path)
				.map(drop)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("create_file join error: {e}")))?
		.map_err(MoonError::from)?;

		// Newly created `.editorconfig` / `.lintstagedrc.json` /
		// `package.json` files invalidate caches the same way a save
		// of an existing one would — the upward walks need to find
		// the new file on the next lookup.
		if file_name == ".editorconfig" {
			self.editorconfig.clear().await;
		}
		if file_name == ".lintstagedrc.json" || file_name == "package.json" {
			self.lint_staged.clear().await;
		}

		Ok(())
	}

	async fn create_dir(&self, path: &Utf8Path) -> MoonResult<()> {
		let candidate = if path.is_absolute() {
			path.to_path_buf()
		} else {
			self.root.join(path)
		};
		let parent = candidate
			.parent()
			.ok_or_else(|| MoonError::invalid("path has no parent directory"))?;
		let resolved_parent = self.resolve(parent)?;
		let dir_name = candidate
			.file_name()
			.ok_or_else(|| MoonError::invalid("path has no name"))?;
		let resolved = resolved_parent.join(dir_name);

		// `create_dir` (vs `create_dir_all`) errors when the target
		// already exists — exactly the strict semantics the file-tree
		// "New folder" flow wants: if the user typed an existing
		// directory's name, surface the error rather than silently
		// no-op. Parents are required to exist; `resolve(parent)`
		// above enforces that.
		tokio::fs::create_dir(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;
		Ok(())
	}

	async fn rename_path(&self, from: &Utf8Path, to: &Utf8Path) -> MoonResult<()> {
		let resolved_from = self.resolve(from)?;
		if resolved_from == self.root {
			return Err(MoonError::invalid("refusing to rename the workspace root"));
		}

		let to_candidate = if to.is_absolute() {
			to.to_path_buf()
		} else {
			self.root.join(to)
		};
		let to_parent = to_candidate
			.parent()
			.ok_or_else(|| MoonError::invalid("rename target has no parent directory"))?;
		let resolved_to_parent = self.resolve(to_parent)?;
		let to_name = to_candidate
			.file_name()
			.ok_or_else(|| MoonError::invalid("rename target has no name"))?;
		let resolved_to = resolved_to_parent.join(to_name);

		if resolved_to == resolved_from {
			return Ok(());
		}

		// `rename(2)` on Linux silently replaces a regular-file
		// target. We pre-check existence so the rename feels
		// symmetric with `create_file` / `create_dir`: clobber-by-
		// accident is the worst failure mode for a UI-driven rename.
		// Tiny TOCTOU window remains; acceptable for an interactive
		// gesture.
		if tokio::fs::metadata(resolved_to.as_std_path()).await.is_ok() {
			return Err(MoonError::invalid(format!(
				"rename target already exists: {resolved_to}"
			)));
		}

		tokio::fs::rename(resolved_from.as_std_path(), resolved_to.as_std_path())
			.await
			.map_err(MoonError::from)?;

		// Either side of the rename might be a cache-affecting
		// config file. We clear conservatively when the *name* on
		// either end matches; tracking which directory was affected
		// adds bookkeeping for no real win, since the caches are
		// cheap to refill.
		let from_name = resolved_from.file_name();
		if to_name == ".editorconfig" || from_name == Some(".editorconfig") {
			self.editorconfig.clear().await;
		}
		let from_lintstaged = matches!(from_name, Some(".lintstagedrc.json") | Some("package.json"));
		let to_lintstaged = matches!(to_name, ".lintstagedrc.json" | "package.json");
		if from_lintstaged || to_lintstaged {
			self.lint_staged.clear().await;
		}

		Ok(())
	}

	async fn stat(&self, path: &Utf8Path) -> MoonResult<StatResult> {
		let resolved = self.resolve(path)?;
		let metadata = tokio::fs::metadata(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;

		let kind = if metadata.is_dir() {
			EntryKind::Dir
		} else if metadata.is_symlink() {
			EntryKind::Symlink
		} else if metadata.is_file() {
			EntryKind::File
		} else {
			EntryKind::Other
		};

		Ok(StatResult {
			kind,
			size: metadata.len(),
			mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
		})
	}

	async fn absolute_path(&self, path: &Utf8Path) -> MoonResult<String> {
		Ok(self.resolve(path)?.to_string())
	}

	async fn trash_path(&self, path: &Utf8Path) -> MoonResult<()> {
		let resolved = self.resolve(path)?;
		if resolved == self.root {
			return Err(MoonError::invalid("refusing to trash the workspace root"));
		}
		// `trash` is sync; offload to the blocking pool so we don't
		// stall the tokio runtime on slow trash backends (XDG trash
		// over a network mount, Finder calls, etc.).
		let target = resolved.into_std_path_buf();
		tokio::task::spawn_blocking(move || trash::delete(&target))
			.await
			.map_err(|e| MoonError::Internal(format!("trash join error: {e}")))?
			.map_err(|e| MoonError::IoError(format!("trash failed: {e}")))?;
		// Mirrors `delete_path`: editorconfig resolution may have
		// indexed something we just sent to the trash, easier to clear
		// the cache than walk it.
		self.editorconfig.clear().await;
		self.lint_staged.clear().await;
		Ok(())
	}

	async fn delete_path(&self, path: &Utf8Path) -> MoonResult<()> {
		let resolved = self.resolve(path)?;
		// Refuse to delete the workspace root itself. `resolve` already
		// blocks paths that escape via `..`, but a literal `.` resolves
		// to the root — and erasing your own workspace from inside the
		// IDE is never what you wanted.
		if resolved == self.root {
			return Err(MoonError::invalid("refusing to delete the workspace root"));
		}
		let metadata = tokio::fs::symlink_metadata(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;
		if metadata.is_dir() {
			tokio::fs::remove_dir_all(resolved.as_std_path())
				.await
				.map_err(MoonError::from)?;
		} else {
			tokio::fs::remove_file(resolved.as_std_path())
				.await
				.map_err(MoonError::from)?;
		}
		// Editorconfig cache may reference the deleted path (or, for a
		// directory delete, anything under it). Cheaper to clear and
		// re-resolve on demand than to walk the cache.
		self.editorconfig.clear().await;
		self.lint_staged.clear().await;
		Ok(())
	}

	async fn editorconfig_for(&self, path: &Utf8Path) -> MoonResult<EditorConfig> {
		self.editorconfig.for_path(path).await
	}

	async fn lint_staged_for(&self, path: &Utf8Path) -> MoonResult<LintStagedRules> {
		self.lint_staged.for_path(path).await
	}

	async fn save_file(&self, path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult> {
		// Two-stage save (per ADR 0012):
		//
		// 1. Editorconfig normalisation in-memory, then write the bytes
		//    to disk so the formatter has something coherent to read.
		// 2. Run the lint-staged chain (if any) against that file, in
		//    the same shape `bun run lint-staged` uses on commit: each
		//    command spawns with the absolute file path appended and is
		//    expected to mutate the file in place. Re-stat afterwards
		//    to pick up the post-format mtime / size for the response.
		//
		// Failures in stage 2 never abort the save — the editorconfig
		// pass already landed on disk, so the worst case is the bytes
		// are normalised but unformatted.
		let ec = self.editorconfig.for_path(path).await?;
		let normalized = pre_save::apply_pipeline(text, &ec);
		let initial = self.write_file(path, &normalized).await?;

		if !self.run_formatter_chain(path).await {
			return Ok(initial);
		}
		Ok(self.restat_after_format(path).await.unwrap_or(initial))
	}

	async fn format_file(&self, path: &Utf8Path) -> MoonResult<WriteFileResult> {
		// Coder turn-end flush entry point. The bytes are already on
		// disk (a previous `write_file` from the edit/write tool
		// landed them). Run the same two stages `save_file` runs but
		// sourced from disk: read → editorconfig → write back → lint
		// -staged chain. Same best-effort posture: a missing tool or
		// non-zero exit collapses to a warn and we return the current
		// stat instead of aborting.
		let read = self.read_file(path).await?;
		if read.is_binary {
			return self.stat_as_write_result(path).await;
		}
		let ec = self.editorconfig.for_path(path).await?;
		let normalized = pre_save::apply_pipeline(&read.text, &ec);
		let after_editorconfig = if normalized == read.text {
			// Skip the write if editorconfig is a no-op so the mtime
			// only moves when bytes actually change. Important for
			// the file-watcher path: a turn full of trivial edits
			// that leave bytes unchanged after editorconfig
			// shouldn't fire watcher events.
			self.stat_as_write_result(path).await?
		} else {
			self.write_file(path, &normalized).await?
		};
		if !self.run_formatter_chain(path).await {
			return Ok(after_editorconfig);
		}
		Ok(self.restat_after_format(path).await.unwrap_or(after_editorconfig))
	}

	async fn git_status_entries(&self, paths: &[String]) -> MoonResult<Vec<GitStatusEntry>> {
		// Both the `git status` subprocess and the walker fallback
		// are blocking work, so hop onto the blocking pool. The git
		// path is dominated by git itself anyway; the walker is
		// single-threaded but fast enough for IDE-sized trees (tens
		// of thousands of files) without `build_parallel`'s wiring.
		let guard = self.git_lock().await;
		let target = self.shell_target().await;
		let root = self.root.clone();
		let paths = paths.to_vec();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			classify_git_status(&target, &root, &paths)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_status_entries join error: {e}")))?
	}

	async fn collect_paths_under(&self, rel: &Utf8Path, max_depth: u32) -> MoonResult<CollectPathsResult> {
		// Lazy-load entry point for ignored directories. Compared
		// to `collect_paths` we skip the `collapsed_ignored_dirs`
		// probe — the caller already knows this subtree is
		// gitignored and explicitly wants its contents — and we
		// root the walk at `rel` so a single click only pays for
		// the directory the user just expanded. An empty `rel`
		// would re-walk the whole workspace without the ignore
		// filter and is rejected: the caller must always name a
		// specific subdirectory.
		let raw = rel.as_str();
		if raw.is_empty() {
			return Err(MoonError::InvalidArgument(
				"collect_paths_under requires a non-empty subdirectory".into(),
			));
		}
		// `resolve` confirms the path exists and stays inside the
		// workspace root; we discard its absolute form and walk
		// with the caller-provided relative segment so the emitted
		// paths stay root-relative (Pierre stores them that way).
		let _ = self.resolve(rel)?;
		let rel_owned = raw.trim_end_matches('/').to_string();
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let mut paths = Vec::new();
			let mut depth_capped = Vec::new();
			let empty = std::collections::BTreeSet::new();
			walk_paths(&root, &rel_owned, &mut paths, &mut depth_capped, 0, max_depth, &empty);
			Ok(CollectPathsResult { paths, depth_capped })
		})
		.await
		.map_err(|e| MoonError::Internal(format!("collect_paths_under join error: {e}")))?
	}

	async fn collect_paths(&self, max_depth: u32) -> MoonResult<CollectPathsResult> {
		// Pure `std::fs` walk on the blocking pool. Tried using
		// `tokio::fs::read_dir` recursively here — it kept the
		// reactor busy with tiny awaits per entry and wound up
		// slower than the sync version, presumably because the
		// actual read_dir syscall is already non-blocking-ish on
		// modern kernels.
		//
		// Before walking we run a quick `git status` to learn
		// which directories git would collapse to a single ignored
		// row (the typical suspects are `node_modules/`, `target/`,
		// `build/`, `dist/`, `.next/`, etc.). The walk then emits
		// each such directory as a single collapsed entry and skips
		// recursing into it — without this, a single moon-ide-sized
		// repo handed Pierre ~127k paths, the bulk of which were
		// `node_modules/**/*` Pierre would dutifully add to its
		// path store and the user never wants to expand. The skip
		// is purely "don't enumerate descendants"; the dir itself
		// stays in the tree so the user can still see it and click
		// it (which today does nothing more than reveal the
		// collapsed badge — lazy descendant fetch is a follow-up).
		// Non-repo folders return an empty skip set and the walk
		// behaves exactly as before.
		//
		// Lock scope: the per-folder git mutex is held **only**
		// across `collapsed_ignored_dirs` — the `git status`
		// shell-out that competes with the index. The fs walk is
		// pure `read_dir` and is explicitly run with the guard
		// dropped so concurrent git operations (blame, branch
		// list, autoStage on save, status refreshes …) aren't
		// serialised behind a slow seed. This matters during a
		// merge conflict against a divergent base, where
		// `git status` against the unmerged index can take tens
		// of seconds — see [ADR 0015](crate::specs#0015).
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let skip = {
				let _guard = guard;
				collapsed_ignored_dirs(&root)
			};
			// Guard dropped here. The walk below runs without
			// the git mutex held; nothing in `walk_paths` touches
			// git.
			let mut paths = Vec::new();
			let mut depth_capped = Vec::new();
			walk_paths(&root, "", &mut paths, &mut depth_capped, 0, max_depth, &skip);
			Ok(CollectPathsResult { paths, depth_capped })
		})
		.await
		.map_err(|e| MoonError::Internal(format!("collect_paths join error: {e}")))?
	}

	async fn git_restore_paths(&self, paths: &[String]) -> MoonResult<()> {
		if paths.is_empty() {
			return Ok(());
		}
		// Containment check runs before we hand anything to git. We
		// can't reuse `resolve` here because deleted files don't
		// exist on disk and `canonicalize` would 404 on them — which
		// is the whole point of restoring them. Lexical check is
		// enough: reject absolute paths, and reject any path whose
		// segments climb out of the root via `..`.
		let mut rels = Vec::with_capacity(paths.len());
		for raw in paths {
			let trimmed = raw.trim_end_matches('/');
			if trimmed.is_empty() {
				continue;
			}
			let rel = Utf8PathBuf::from(trimmed);
			if rel.is_absolute() {
				return Err(MoonError::invalid(format!(
					"git_restore_paths rejects absolute path: {rel}"
				)));
			}
			// Walk segments and bail if we ever climb above depth 0.
			// Extra-defensive: this also rejects `a/../b` even though
			// it's technically in-root, because a path that needs
			// normalisation is almost always a bug in the caller.
			let mut depth = 0i32;
			for seg in rel.components() {
				match seg {
					camino::Utf8Component::ParentDir => {
						depth -= 1;
						if depth < 0 {
							return Err(MoonError::invalid(format!(
								"git_restore_paths rejects path escape: {rel}"
							)));
						}
					}
					camino::Utf8Component::Normal(_) => depth += 1,
					camino::Utf8Component::CurDir => {}
					// Prefix / RootDir are absolute-path markers we
					// already rejected via `is_absolute` above, but
					// be explicit so a future camino change doesn't
					// silently re-admit them.
					camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir => {
						return Err(MoonError::invalid(format!(
							"git_restore_paths rejects rooted path: {rel}"
						)));
					}
				}
			}
			rels.push(rel);
		}
		if rels.is_empty() {
			return Ok(());
		}
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_restore(&root, &rels)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_restore_paths join error: {e}")))??;
		// Restored files may have been open in the editor; the
		// editorconfig cache doesn't key on content, so no entry is
		// stale, but staying symmetric with trash/delete keeps future
		// maintainers from wondering why this one skips the clear.
		self.editorconfig.clear().await;
		self.lint_staged.clear().await;
		Ok(())
	}

	async fn git_add_paths(&self, paths: &[String]) -> MoonResult<()> {
		if paths.is_empty() {
			return Ok(());
		}
		// Same lexical containment as `git_restore_paths` — we
		// won't hand `..`-escaping or absolute strings to `git
		// add`. Borrowed verbatim because the validation is the
		// same and there's no third caller worth factoring it
		// out for yet.
		let mut rels = Vec::with_capacity(paths.len());
		for raw in paths {
			let trimmed = raw.trim_end_matches('/');
			if trimmed.is_empty() {
				continue;
			}
			let rel = Utf8PathBuf::from(trimmed);
			if rel.is_absolute() {
				return Err(MoonError::invalid(format!(
					"git_add_paths rejects absolute path: {rel}"
				)));
			}
			let mut depth = 0i32;
			for seg in rel.components() {
				match seg {
					camino::Utf8Component::ParentDir => {
						depth -= 1;
						if depth < 0 {
							return Err(MoonError::invalid(format!("git_add_paths rejects path escape: {rel}")));
						}
					}
					camino::Utf8Component::Normal(_) => depth += 1,
					camino::Utf8Component::CurDir => {}
					camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir => {
						return Err(MoonError::invalid(format!("git_add_paths rejects rooted path: {rel}")));
					}
				}
			}
			rels.push(rel);
		}
		if rels.is_empty() {
			return Ok(());
		}
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			let mut args = vec!["add", "--"];
			let rel_strs: Vec<&str> = rels.iter().map(|r| r.as_str()).collect();
			args.extend(rel_strs.iter().copied());
			run_git_simple(&root, &args, "git add")
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_add_paths join error: {e}")))?
	}

	async fn git_blame(&self, path: &Utf8Path) -> MoonResult<Option<GitFileBlame>> {
		// Path has to live inside the active folder — same lexical
		// containment as `git_restore_paths`. We also refuse
		// directories outright because `git blame` doesn't blame a
		// directory; the UI should never send one, but belt-and-brace.
		if path.as_str().is_empty() {
			return Ok(None);
		}
		let rel = Utf8PathBuf::from(path.as_str().trim_end_matches('/'));
		if rel.is_absolute() {
			return Err(MoonError::invalid(format!("git_blame rejects absolute path: {rel}")));
		}
		let mut depth = 0i32;
		for seg in rel.components() {
			match seg {
				camino::Utf8Component::ParentDir => {
					depth -= 1;
					if depth < 0 {
						return Err(MoonError::invalid(format!("git_blame rejects path escape: {rel}")));
					}
				}
				camino::Utf8Component::Normal(_) => depth += 1,
				camino::Utf8Component::CurDir => {}
				camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir => {
					return Err(MoonError::invalid(format!("git_blame rejects rooted path: {rel}")));
				}
			}
		}
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_blame(&root, &rel)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_blame join error: {e}")))?
	}

	async fn git_permalink(&self, path: &Utf8Path, start_line: u32, end_line: u32) -> MoonResult<Option<GitPermalink>> {
		// Same containment envelope as `git_blame` / `git_ref_content`:
		// reject empty, absolute, rooted, and `..`-escaping paths. The
		// editor never legitimately asks for a permalink outside the
		// active folder.
		if path.as_str().is_empty() {
			return Ok(None);
		}
		let rel = reject_uncontained_path("git_permalink", path)?;
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_permalink(&root, &rel, start_line, end_line))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_permalink join error: {e}")))?
	}

	async fn git_blob_sha(&self, path: &Utf8Path) -> MoonResult<Option<String>> {
		let rel = reject_uncontained_path("git_blob_sha", path)?;
		if rel.as_str().is_empty() {
			return Ok(None);
		}
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_blob_sha(&root, &rel))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_blob_sha join error: {e}")))?
	}

	async fn publish_pr_review(&self, request: PublishReviewRequest) -> MoonResult<PublishReviewResult> {
		run_publish_pr_review(&self.root, request).await
	}

	async fn git_head_content(&self, path: &Utf8Path) -> MoonResult<Option<String>> {
		self.git_ref_content("HEAD", path).await
	}

	async fn git_ref_content(&self, rev: &str, path: &Utf8Path) -> MoonResult<Option<String>> {
		if !is_safe_rev(rev) {
			return Err(MoonError::invalid(format!(
				"git_ref_content rejects rev: {rev:?} (expected \"HEAD\" or 40-char hex SHA)"
			)));
		}
		// Same containment envelope as `git_blame`: reject absolute,
		// reject `..` escapes, reject rooted paths. The diff view
		// never legitimately asks for anything outside the active
		// folder.
		if path.as_str().is_empty() {
			return Ok(None);
		}
		let rel = Utf8PathBuf::from(path.as_str().trim_end_matches('/'));
		if rel.is_absolute() {
			return Err(MoonError::invalid(format!(
				"git_ref_content rejects absolute path: {rel}"
			)));
		}
		let mut depth = 0i32;
		for seg in rel.components() {
			match seg {
				camino::Utf8Component::ParentDir => {
					depth -= 1;
					if depth < 0 {
						return Err(MoonError::invalid(format!(
							"git_ref_content rejects path escape: {rel}"
						)));
					}
				}
				camino::Utf8Component::Normal(_) => depth += 1,
				camino::Utf8Component::CurDir => {}
				camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir => {
					return Err(MoonError::invalid(format!(
						"git_ref_content rejects rooted path: {rel}"
					)));
				}
			}
		}
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let rev = rev.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_ref_content(&root, &rev, &rel)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_ref_content join error: {e}")))?
	}

	async fn git_default_branch_diff(&self) -> MoonResult<Option<BranchDiffStatus>> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_default_branch_diff(&root))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_default_branch_diff join error: {e}")))?
	}

	async fn git_branch(&self) -> MoonResult<GitBranchInfo> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_branch(&root))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_branch join error: {e}")))?
	}

	async fn git_exclude_path(&self) -> MoonResult<Option<String>> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let target = self.shell_target().await;
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(git_exclude_path(&root, &target).map(|p| p.to_string()))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_exclude_path join error: {e}")))?
	}

	async fn git_local_branches(&self) -> MoonResult<Vec<String>> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			let names = run_branch_list_local(&root)
				.into_iter()
				.filter_map(|entry| match entry {
					BranchListEntry::Local { name, .. } => Some(name),
					BranchListEntry::Pr { .. } => None,
				})
				.collect();
			Ok(names)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_local_branches join error: {e}")))?
	}

	async fn git_commit_on_new_branch(&self, branch: &str, message: &str) -> MoonResult<GitCommitResult> {
		let guard = self.git_lock().await;
		let target = self.shell_target().await;
		let root = self.root.clone();
		let branch = branch.to_owned();
		let message = message.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_commit_on_new_branch(&root, &branch, &message, &target)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_commit_on_new_branch join error: {e}")))?
	}

	async fn git_worktree_add(&self, path: &Utf8Path, branch: WorktreeBranch) -> MoonResult<GitWorktree> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let path = path.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			// Always host (ADR 0029): the worktree is created inside
			// the parent repo (`<parent>/.worktrees/…`) with
			// `--relative-paths` links, so the same checkout resolves
			// host-side and inside the container via the parent's bind
			// mount. No container-side repair needed.
			run_git_worktree_add(&root, &path, &branch, &ShellTarget::Host)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_worktree_add join error: {e}")))?
	}

	async fn git_worktree_add_moving(
		&self,
		path: &Utf8Path,
		branch: WorktreeBranch,
		reset_main_to: Option<String>,
	) -> MoonResult<GitWorktree> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let path = path.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			// Host git (ADR 0028 W.4): the switch happens on the host
			// main tree, and the worktree itself lives host-side.
			run_git_worktree_add_moving(&root, &path, &branch, reset_main_to.as_deref(), &ShellTarget::Host)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_worktree_add_moving join error: {e}")))?
	}

	async fn git_worktree_list(&self) -> MoonResult<Vec<GitWorktree>> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_worktree_list(&root, &ShellTarget::Host)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_worktree_list join error: {e}")))?
	}

	async fn git_worktree_remove(&self, path: &Utf8Path, force: bool) -> MoonResult<()> {
		let guard = self.git_lock().await;
		// Remove runs where the worktree's metadata is valid: the
		// container when the workspace runs there (the caller passes
		// the `/workspace/.worktrees/…` path), the host otherwise.
		let target = self.shell_target().await;
		let root = self.root.clone();
		let path = path.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_worktree_remove(&root, &path, force, &target)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_worktree_remove join error: {e}")))?
	}

	async fn git_diff_summary(&self) -> MoonResult<String> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_diff_summary(&root))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_diff_summary join error: {e}")))?
	}

	async fn git_commit(&self, message: &str, amend: bool) -> MoonResult<GitCommitResult> {
		let trimmed = message.trim();
		if trimmed.is_empty() && !amend {
			return Err(MoonError::invalid("commit message is empty"));
		}
		let guard = self.git_lock().await;
		let target = self.shell_target().await;
		let root = self.root.clone();
		let owned = trimmed.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_commit(&root, &owned, amend, &target)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_commit join error: {e}")))?
	}

	async fn git_push(&self) -> MoonResult<()> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_push(&root)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_push join error: {e}")))?
	}

	async fn git_publish_branch(&self) -> MoonResult<()> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_simple(
				&root,
				&["push", "--set-upstream", "origin", "HEAD"],
				"git push -u origin HEAD",
			)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_publish_branch join error: {e}")))?
	}

	async fn git_pull(&self) -> MoonResult<()> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			let result = run_git_simple(&root, &["pull", "--rebase"], "git pull --rebase");
			if result.is_err() {
				// Rebase mid-conflict leaves the working tree in
				// detached "REBASE-IN-PROGRESS" state. The SCM
				// panel's contract is "sync either succeeds or
				// leaves the tree exactly as it was", so when
				// a rebase is in flight we abort it so the user
				// can resolve in their terminal of choice (or
				// just try again later) without the worktree
				// being half-rewritten. Best-effort: if the
				// abort itself fails, we still surface the
				// original pull error.
				let rebase_dir = root.join(".git").join("rebase-merge");
				let rebase_apply = root.join(".git").join("rebase-apply");
				if rebase_dir.exists() || rebase_apply.exists() {
					let _ = run_git_simple(&root, &["rebase", "--abort"], "git rebase --abort");
				}
			}
			result
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_pull join error: {e}")))?
	}

	async fn git_merge_default_branch(&self, remote_ref: &str) -> MoonResult<()> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let remote_ref = remote_ref.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_simple(
				&root,
				&["merge", "--no-edit", &remote_ref],
				&format!("git merge {remote_ref}"),
			)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_merge_default_branch join error: {e}")))?
	}

	async fn git_merge_state(&self) -> MoonResult<GitMergeState> {
		// Holds the git mutex for the `ls-files` call. The
		// MERGE_HEAD / MERGE_MSG reads are pure filesystem
		// operations against `.git/` and don't need git's index
		// lock, but bundling them under the same guard keeps
		// the snapshot internally consistent — a concurrent
		// `git merge --abort` can't half-clear the state from
		// under us.
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(read_git_merge_state(&root))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_merge_state join error: {e}")))?
	}

	async fn git_merge_abort(&self) -> MoonResult<()> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_git_simple(&root, &["merge", "--abort"], "git merge --abort")
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_merge_abort join error: {e}")))?
	}

	async fn git_fetch(&self) -> MoonResult<()> {
		// `run_git_fetch_quiet` is async (uses tokio's
		// `Command` for the timeout), so the guard just needs to
		// outlive the await.
		let _guard = self.git_lock().await;
		run_git_fetch_quiet(&self.root).await
	}

	async fn branch_list(&self, pr_scope: PrListScope) -> MoonResult<BranchList> {
		// `run_branch_list` shells out to `git for-each-ref` and
		// `gh pr list`; both can compete with concurrent index
		// writes. `gh` is the slow bit (network), so the worst
		// case is a commit waiting a few seconds for an in-flight
		// PR list — acceptable.
		let _guard = self.git_lock().await;
		run_branch_list(&self.root, pr_scope).await
	}

	async fn git_existing_pr_url(&self) -> MoonResult<Option<String>> {
		// Same git lock as `branch_list` — gh's `pr list` resolves
		// the active repo via the .git directory and we don't want
		// to race a concurrent commit / switch. Best-effort: every
		// failure collapses to `Ok(None)` so the SCM panel just
		// falls back to its create-PR URL.
		let _guard = self.git_lock().await;
		Ok(run_git_existing_pr_url(&self.root).await)
	}

	async fn branch_switch(&self, target: &BranchSwitchTarget) -> MoonResult<()> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let target = target.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			run_branch_switch(&root, &target)
		})
		.await
		.map_err(|e| MoonError::Internal(format!("branch_switch join error: {e}")))?
	}

	async fn git_head_commit_message(&self) -> MoonResult<String> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_head_commit_message(&root))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_head_commit_message join error: {e}")))?
	}

	async fn git_diff_patch(&self) -> MoonResult<String> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_diff_patch(&root))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_diff_patch join error: {e}")))?
	}

	async fn git_log(&self, limit: u32) -> MoonResult<Vec<CommitEntry>> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_log(&root, limit))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_log join error: {e}")))?
	}

	async fn git_commit_diff(&self, sha: &str) -> MoonResult<Option<CommitDiff>> {
		let guard = self.git_lock().await;
		let root = self.root.clone();
		let sha = sha.to_owned();
		tokio::task::spawn_blocking(move || {
			let _guard = guard;
			Ok(run_git_commit_diff(&root, &sha))
		})
		.await
		.map_err(|e| MoonError::Internal(format!("git_commit_diff join error: {e}")))?
	}
}

/// `git symbolic-ref --short HEAD` for the branch name,
/// `git rev-parse --short HEAD` for the commit hash, plus
/// `git rev-list --count --left-right HEAD...@{u}` for the
/// ahead/behind counts vs upstream. Each can fail independently
/// — fresh `git init` with no commits has a resolvable branch
/// name but no HEAD, a detached HEAD has the reverse, and a
/// branch with no configured upstream errors on the rev-list
/// — so we run them separately and return whichever succeeded.
/// Any failure (including the folder not being a git repo)
/// leaves the corresponding field at its `None` / `0` default;
/// the SCM panel renders the all-default case as a bare "no
/// branch" label with no count badges.
fn run_git_branch(root: &Utf8Path) -> GitBranchInfo {
	use std::process::Command;

	let name = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["symbolic-ref", "--quiet", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty());

	let head_short_sha = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["rev-parse", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty());

	// "Upstream configured" means git knows where `push` / `pull`
	// should go — i.e. `branch.<name>.merge` is set (with an
	// accompanying `branch.<name>.remote`, named or URL). We read
	// the config keys directly instead of `git rev-parse @{u}`
	// because `@{u}` requires the remote to be a *named* remote
	// with a remote-tracking ref, and `gh pr checkout` on a fork
	// PR points `branch.<name>.remote` at the fork's git URL —
	// no named remote, no remote-tracking ref, but pushing /
	// pulling does work. Detached HEAD / non-repo collapse to
	// `false` via the `name` filter.
	let upstream = name.as_deref().and_then(|n| read_branch_upstream(root, n));
	let has_upstream = upstream.is_some();

	// `@{u}` resolves iff the upstream is a *tracked* named
	// remote (refs/remotes/<remote>/<branch> exists). For the
	// fork-PR shape (URL-only upstream) it doesn't resolve, so
	// ahead/behind below can't be computed and the SCM panel
	// switches to count-less Sync Changes. Cheap to call — no
	// network, just a ref lookup.
	let upstream_tracked = has_upstream
		&& Command::new("git")
			.arg("-C")
			.arg(root.as_std_path())
			.args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
			.output()
			.ok()
			.is_some_and(|o| o.status.success());

	// A tracked named-remote upstream whose branch name differs
	// from the local branch is *foreign*: we branched off a
	// shared ref (`git checkout -b feature origin/main` leaves
	// `branch.feature.merge = refs/heads/main`) and don't own a
	// remote branch yet. The SCM panel must treat this as an
	// unpublished branch, never as "push to its upstream" — that
	// would land our commits straight on `main`. The fork-PR
	// shape is excluded by the `upstream_tracked` gate (URL
	// remotes don't resolve `@{u}`), and a normal same-name
	// upstream is excluded by the name comparison.
	let upstream_foreign = upstream_tracked
		&& match (&name, &upstream) {
			(Some(local), Some((_, merge))) => {
				let upstream_branch = merge.strip_prefix("refs/heads/").unwrap_or(merge.as_str());
				upstream_branch != local.as_str()
			}
			_ => false,
		};

	// `rev-list --count --left-right HEAD...@{u}` prints
	// `<ahead>\t<behind>`: commits we have that upstream doesn't,
	// then commits upstream has that we don't. Errors silently
	// when `@{u}` can't be resolved — no upstream configured
	// (fresh branch not yet pushed, detached HEAD, fresh repo),
	// or the upstream points at a URL rather than a named remote
	// (fork-PR config — see `has_upstream` above). The (0, 0)
	// fallback is exactly the right "render no badges" signal
	// for the UI in those cases; the user can still hit Sync to
	// push, and a follow-up fetch from a named remote will fill
	// the counts in.
	let (ahead, behind) = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["rev-list", "--count", "--left-right", "HEAD...@{u}"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.and_then(|s| {
			let mut parts = s.split_whitespace();
			let ahead: u32 = parts.next()?.parse().ok()?;
			let behind: u32 = parts.next()?.parse().ok()?;
			Some((ahead, behind))
		})
		.unwrap_or((0, 0));

	// Compose the GitHub PR-create URL when we have both inputs:
	// a recognised remote + a non-detached branch. URL-escaping
	// follows GitHub's "branch name in path segment" rules: `/`
	// stays literal (forward slashes appear in `feat/foo` style
	// branches), the rest of the disallowed-in-path set goes
	// percent-encoded. The frontend gates visibility on UI
	// policy (`has_upstream`, non-main/master); we just produce
	// the URL whenever it's well-defined.
	let pr_url = match (remote_web_url(root), name.as_deref()) {
		(Some(base), Some(branch)) => Some(format!("{base}/pull/new/{}", encode_branch_segment(branch))),
		_ => None,
	};

	let default_branch_remote_ref = resolve_default_remote_ref(root);
	// Hide the "Update from main" affordance when we're already
	// on the default branch — the regular `Sync Changes` button
	// covers that case (its `behind` is the same `origin/main →
	// HEAD` count). Comparing the local short name against the
	// stripped remote-tracking ref is enough: `origin/main` →
	// `main`, which equals the local branch name when checked
	// out from `origin/main`.
	let default_branch_behind = match (&default_branch_remote_ref, &name) {
		(Some(remote_ref), Some(local_name)) => {
			let local_default = remote_ref
				.split_once('/')
				.map(|(_, b)| b)
				.unwrap_or(remote_ref.as_str());
			if local_default == local_name.as_str() {
				0
			} else {
				count_behind(root, remote_ref)
			}
		}
		_ => 0,
	};

	GitBranchInfo {
		name,
		head_short_sha,
		has_upstream,
		upstream_tracked,
		upstream_foreign,
		ahead,
		behind,
		pr_url,
		default_branch_remote_ref,
		default_branch_behind,
	}
}

/// Best-effort resolution of the repo's default branch on `origin`.
/// Three sources, tried in order:
///
/// 1. `git symbolic-ref --short refs/remotes/origin/HEAD` — set by
///    `git clone` and refreshable with `git remote set-head origin
///    --auto`. The right answer when it's there.
/// 2. `git rev-parse --verify --quiet origin/main` — modern default
///    that the symbolic ref usually points at.
/// 3. `git rev-parse --verify --quiet origin/master` — older default,
///    still common on long-lived repos.
///
/// Returns the short remote-tracking name (`"origin/main"` /
/// `"origin/master"`) so the SCM panel can both display the local
/// short name as a label and pass the full ref to `git merge`.
/// `None` when no `origin` remote exists, the symbolic ref isn't
/// set, and neither fallback ref resolves — the SCM panel hides
/// its "Update from <main>" button in that case.
fn resolve_default_remote_ref(root: &Utf8Path) -> Option<String> {
	use std::process::Command;

	let symbolic = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["symbolic-ref", "--short", "--quiet", "refs/remotes/origin/HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty());
	if symbolic.is_some() {
		return symbolic;
	}
	for candidate in ["origin/main", "origin/master"] {
		let exists = Command::new("git")
			.arg("-C")
			.arg(root.as_std_path())
			.args(["rev-parse", "--verify", "--quiet", candidate])
			.output()
			.ok()
			.is_some_and(|o| o.status.success());
		if exists {
			return Some(candidate.to_owned());
		}
	}
	None
}

/// `git rev-list --count HEAD..<remote_ref>` — number of commits
/// `<remote_ref>` has that `HEAD` doesn't. Same shape the upstream
/// `behind` counter uses; reports `0` on any failure (no HEAD,
/// missing ref, git unavailable).
fn count_behind(root: &Utf8Path, remote_ref: &str) -> u32 {
	use std::process::Command;

	Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["rev-list", "--count", &format!("HEAD..{remote_ref}")])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.and_then(|s| s.trim().parse::<u32>().ok())
		.unwrap_or(0)
}

/// Percent-encode a git branch name for use as a single path
/// segment under `https://github.com/owner/repo/`. We deliberately
/// leave `/` alone — branch names like `feat/foo` are valid and
/// GitHub renders them as nested path segments — and only escape
/// the bytes the URL spec disallows in a path. Branch names are
/// already constrained by git's `check-ref-format` to a fairly
/// narrow set, so this is mostly defensive.
fn encode_branch_segment(branch: &str) -> String {
	let mut out = String::with_capacity(branch.len());
	for byte in branch.bytes() {
		let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/');
		if safe {
			out.push(byte as char);
		} else {
			out.push('%');
			out.push_str(&format!("{byte:02X}"));
		}
	}
	out
}

/// `git add -A && git commit [-m <message> | --amend [-m <message>|--no-edit]]`.
/// Stages every working-tree change (including untracked) before
/// committing — matches the SCM panel's "commit current changes"
/// affordance. The two invocations are sequential rather than
/// `git commit -a` so untracked files are picked up too (`-a`
/// skips them).
///
/// Amend mode replaces the most recent commit instead of creating
/// a new one. Empty message + amend → `--no-edit` (preserve the
/// previous commit's message). Non-empty message + amend → `-m`
/// rewrites the message. Allows the SCM panel's amend toggle to
/// double as both "absorb these changes into HEAD without
/// changing the message" and "rewrite the last commit's message
/// while you're at it".
///
/// Errors propagate git's own stderr; for the empty-tree case we
/// detect git's "nothing to commit" preamble and rewrite into a
/// friendlier message. Amend with no staged changes and no
/// message change is a no-op git refuses with the same preamble;
/// the rewrite covers it too.
///
/// **Safety snapshot.** After `git add -A` lands every change in
/// the index, we take a `git stash create` snapshot of that
/// index. If `git commit` fails for any reason between then and
/// success — most importantly, a misbehaving pre-commit hook
/// (lint-staged, pre-commit) that crashes mid-stash-apply and
/// leaves the working tree in pieces — we restore via
/// [`try_restore_commit_safety_snapshot`] so the user never
/// loses files. The per-folder git mutex (ADR 0015) makes the
/// corruption window essentially zero from our side, but the
/// snapshot is cheap and gives us a last-resort if the hook
/// itself races against a sibling process or has a bug of its
/// own. On success the snapshot becomes an unreferenced commit
/// object that git GC will drop in the usual 30/90 day window.
///
/// We snapshot **after** `git add -A` rather than before because
/// `git stash create -u` silently drops untracked files on
/// git ≤ 2.43 — but staging-as-Added pulls them into the index
/// where a vanilla `git stash create` captures them.
/// Build a `git` invocation for `root` that lands in the right
/// userland. For [`ShellTarget::Host`] this is `git -C <root> …`,
/// exactly as before. For [`ShellTarget::Container`] it becomes
/// `docker exec -w <server_root> <container> git -C <server_root> …`
/// so the commit's pre-commit hook runs with the container's
/// toolchain (`node_modules/.bin/`, fnm's Node, the pinned
/// formatters) instead of whatever the host happens to have on
/// `PATH` — the same routing principle format-on-save already
/// uses (ADR 0013 § Container routing).
///
/// The `.git` directory is the same bytes on both sides through
/// the bind mount, so index / object writes are coherent
/// regardless of which userland git runs in; what changes is the
/// hook's environment. Callers append the git subcommand and its
/// arguments with `.arg(...)` / `.args(...)` as usual.
///
/// When the active folder lives outside the container bind mount
/// (path translation fails) we fall back to host execution rather
/// than spawning git against a path the in-container process
/// can't see.
fn git_command(target: &ShellTarget, root: &Utf8Path) -> std::process::Command {
	use std::process::Command;

	match target {
		ShellTarget::Host => {
			let mut cmd = Command::new("git");
			cmd.arg("-C").arg(root.as_std_path());
			cmd
		}
		ShellTarget::Container { container_name, .. } => {
			let Some(server_root) = target.translate_path(root) else {
				let mut cmd = Command::new("git");
				cmd.arg("-C").arg(root.as_std_path());
				return cmd;
			};
			let mut cmd = Command::new("docker");
			cmd
				.arg("exec")
				.arg("-w")
				.arg(server_root.as_str())
				.arg(container_name)
				.arg("git")
				.arg("-C")
				.arg(server_root.as_str());
			cmd
		}
	}
}

fn run_git_commit(root: &Utf8Path, message: &str, amend: bool, target: &ShellTarget) -> MoonResult<GitCommitResult> {
	let stage = git_command(target, root)
		.args(["add", "-A"])
		.output()
		.map_err(|e| MoonError::IoError(format!("git add failed to launch: {e}")))?;
	if !stage.status.success() {
		// `git add` failed before we touched anything reversible,
		// so there's nothing to restore — let the error propagate
		// straight through.
		let stderr = String::from_utf8_lossy(&stage.stderr).trim().to_string();
		return Err(MoonError::IoError(format!(
			"git add exited {}: {stderr}",
			stage.status.code().unwrap_or(-1)
		)));
	}

	let safety = take_commit_safety_snapshot(root, target);

	let mut commit = git_command(target, root);
	commit
		// Force the C locale so the "nothing to commit" detection
		// below works regardless of the user's system language —
		// otherwise git localises stdout (e.g. French outputs
		// "rien à valider") and we miss the friendly-error path.
		// Stderr passed verbatim to the flash toast also stays in
		// English, which we'd want anyway given the rest of the UI
		// is English.
		.env("LC_ALL", "C")
		.arg("commit");
	if amend {
		commit.arg("--amend");
		if message.is_empty() {
			commit.arg("--no-edit");
		} else {
			commit.arg("-m").arg(message);
		}
	} else {
		commit.arg("-m").arg(message);
	}
	let commit = commit
		.output()
		.map_err(|e| MoonError::IoError(format!("git commit failed to launch: {e}")))?;
	if !commit.status.success() {
		let stdout = String::from_utf8_lossy(&commit.stdout).to_string();
		let stderr = String::from_utf8_lossy(&commit.stderr).trim().to_string();
		if let Some(snap) = &safety {
			try_restore_commit_safety_snapshot(root, snap, target);
		}
		// `git commit` prints "nothing to commit, working tree clean"
		// (or one of several variants) on stdout when the index has
		// no staged changes after our `add -A` pass — typically
		// because every "change" the user saw was actually ignored
		// or already committed. Surface a friendlier message.
		if stdout.contains("nothing to commit") {
			return Err(MoonError::invalid("nothing to commit"));
		}
		// Author identity errors land on stderr with the standard
		// "Please tell me who you are" preamble. Pass them through
		// verbatim — the user needs the actionable hints git
		// itself gives ("git config --global user.email ...").
		let combined = if stderr.is_empty() { stdout } else { stderr };
		return Err(MoonError::IoError(format!(
			"git commit exited {}: {}",
			commit.status.code().unwrap_or(-1),
			combined.trim()
		)));
	}

	let short_sha = git_command(target, root)
		.args(["rev-parse", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.unwrap_or_default();

	// Echo back the **head commit's** subject when amend ran
	// without a fresh message — the SCM panel's success toast
	// should reflect what git actually recorded, not the empty
	// string the user passed in. The non-amend path uses the
	// message we just sent (it's by construction the new
	// subject).
	let summary = if amend && message.is_empty() {
		git_command(target, root)
			.args(["log", "-1", "--pretty=%s"])
			.output()
			.ok()
			.filter(|o| o.status.success())
			.and_then(|o| String::from_utf8(o.stdout).ok())
			.map(|s| s.trim().to_owned())
			.unwrap_or_default()
	} else {
		message.lines().next().unwrap_or("").trim().to_owned()
	};

	Ok(GitCommitResult { short_sha, summary })
}

/// Snapshot the current index as a free-floating stash commit
/// and return its SHA. Called **after** `git add -A` so the
/// index already has every working-tree change — including
/// previously-untracked files, which `git stash create -u`
/// silently drops on git ≤ 2.43. Returns `None` on a clean tree
/// (nothing to snapshot) or any git failure.
///
/// The created object is **not** in the stash list — it's a
/// dangling commit. Callers either reference it explicitly via
/// [`try_restore_commit_safety_snapshot`] or let it be GC'd.
/// Cost is sub-millisecond on a typical repo.
fn take_commit_safety_snapshot(root: &Utf8Path, target: &ShellTarget) -> Option<String> {
	let output = git_command(target, root).args(["stash", "create"]).output().ok()?;
	if !output.status.success() {
		return None;
	}
	let sha = String::from_utf8(output.stdout).ok()?.trim().to_owned();
	if sha.is_empty() {
		return None;
	}
	Some(sha)
}

/// Best-effort restore from a snapshot taken by
/// [`take_commit_safety_snapshot`]. Sequence:
///
/// 1. `git read-tree --reset <snap>` rewrites the index to match
///    the snapshot's tree (which is everything we had staged
///    just before `git commit` ran the hooks).
/// 2. `git checkout-index -a -f` writes every index entry back
///    to the working tree, restoring files a misbehaving hook
///    deleted and overwriting any half-applied modifications.
///
/// The "untracked" / "tracked" distinction collapses here
/// because step 1 happens after `git add -A` snapshotted both
/// into the index. **Side effect:** any files a successful
/// hook auto-fixed (`eslint --fix`, `prettier --write`, etc.)
/// in the working tree get wiped on restore — but the restore
/// only runs when the commit also failed, in which case the
/// auto-fix would have been discarded by lint-staged's own
/// "revert to original state" path anyway. Net: same end-state
/// the user expects from a failed commit, plus the data-loss
/// guarantee.
///
/// Falls back to `git stash store`-ing the snapshot under a
/// labelled message if any of those commands fail. The labelled
/// stash surfaces in `git stash list` so the recovery path is
/// discoverable without reading our logs.
///
/// Errors are never propagated — restoration is opportunistic
/// and the caller is already returning a commit failure. The
/// `tracing` lines are the supported triage channel.
fn try_restore_commit_safety_snapshot(root: &Utf8Path, sha: &str, target: &ShellTarget) {
	let read_tree = git_command(target, root).args(["read-tree", "--reset", sha]).output();
	let read_tree_ok = matches!(&read_tree, Ok(o) if o.status.success());
	if !read_tree_ok {
		log_safety_snapshot_failure(sha, "read-tree", &read_tree);
		store_safety_snapshot_for_manual_recovery(root, sha, target);
		return;
	}

	let checkout = git_command(target, root).args(["checkout-index", "-a", "-f"]).output();
	let checkout_ok = matches!(&checkout, Ok(o) if o.status.success());
	if !checkout_ok {
		log_safety_snapshot_failure(sha, "checkout-index", &checkout);
		store_safety_snapshot_for_manual_recovery(root, sha, target);
		return;
	}

	tracing::info!(
		snapshot = %sha,
		"moon-ide: commit failed; restored index + working tree from safety snapshot",
	);
}

fn log_safety_snapshot_failure(sha: &str, step: &str, result: &std::io::Result<std::process::Output>) {
	match result {
		Ok(o) => {
			let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
			tracing::warn!(
				snapshot = %sha,
				step,
				%stderr,
				"moon-ide: safety snapshot restore step failed",
			);
		}
		Err(e) => {
			tracing::warn!(
				snapshot = %sha,
				step,
				error = %e,
				"moon-ide: safety snapshot restore step failed to launch",
			);
		}
	}
}

fn store_safety_snapshot_for_manual_recovery(root: &Utf8Path, sha: &str, target: &ShellTarget) {
	let store = git_command(target, root)
		.args([
			"stash",
			"store",
			"--quiet",
			"-m",
			"moon-ide commit safety snapshot — recover with `git stash pop`",
			sha,
		])
		.output();
	if let Err(e) = store {
		tracing::warn!(
			snapshot = %sha,
			error = %e,
			"moon-ide: safety snapshot store failed to launch — snapshot survives only as a dangling commit",
		);
	}
}

/// Validate `branch` with `git check-ref-format --branch`, create
/// it from current `HEAD` (`git switch -c <branch>`), then route
/// to [`run_git_commit`]. On any failure after the branch has
/// been created we attempt a rollback (`git switch -` plus
/// `git branch -D <branch>`) so the user's `HEAD` is back where
/// it started — best-effort, the original error is what the
/// caller surfaces.
fn run_git_commit_on_new_branch(
	root: &Utf8Path,
	branch: &str,
	message: &str,
	target: &ShellTarget,
) -> MoonResult<GitCommitResult> {
	if branch.is_empty() {
		return Err(MoonError::invalid("branch name is empty"));
	}
	let check = git_command(target, root)
		.args(["check-ref-format", "--branch", branch])
		.output()
		.map_err(|e| MoonError::IoError(format!("git check-ref-format failed to launch: {e}")))?;
	if !check.status.success() {
		let stderr = String::from_utf8_lossy(&check.stderr).trim().to_string();
		let detail = if stderr.is_empty() {
			format!("{branch:?} is not a valid git branch name")
		} else {
			format!("{branch:?}: {stderr}")
		};
		return Err(MoonError::invalid(detail));
	}

	// Snapshot the previous branch so a failed commit can roll
	// back to it. Detached HEAD returns a non-zero exit; we treat
	// that as "no name to roll back to" and fall back to switching
	// by SHA.
	let previous_ref = git_command(target, root)
		.args(["symbolic-ref", "--quiet", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty());
	let previous_sha = git_command(target, root)
		.args(["rev-parse", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty());

	let switch = git_command(target, root)
		.args(["switch", "-c", branch])
		.output()
		.map_err(|e| MoonError::IoError(format!("git switch -c failed to launch: {e}")))?;
	if !switch.status.success() {
		let stderr = String::from_utf8_lossy(&switch.stderr).trim().to_string();
		let stdout = String::from_utf8_lossy(&switch.stdout).trim().to_string();
		let combined = if stderr.is_empty() { stdout } else { stderr };
		return Err(MoonError::IoError(format!(
			"git switch -c exited {}: {combined}",
			switch.status.code().unwrap_or(-1)
		)));
	}

	let commit_result = run_git_commit(root, message, false, target);
	match commit_result {
		Ok(result) => Ok(result),
		Err(err) => {
			// Roll back: switch back to the previous ref, then
			// delete the freshly-created branch. Both are best-
			// effort — if either fails we log and return the
			// original commit error, since that's the one the
			// user has to act on.
			let rollback_target = previous_ref.as_deref().or(previous_sha.as_deref());
			if let Some(rollback_ref) = rollback_target {
				let switch_back = git_command(target, root).args(["switch", rollback_ref]).output();
				if let Err(e) = switch_back {
					tracing::warn!(target = %rollback_ref, error = %e, "rollback: git switch failed to launch");
				} else if let Ok(out) = git_command(target, root).args(["branch", "-D", branch]).output() {
					if !out.status.success() {
						let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
						tracing::warn!(branch = %branch, stderr = %msg, "rollback: failed to delete fresh branch");
					}
				}
			}
			Err(err)
		}
	}
}

/// Resolve the worktree-`path` argument for a `git worktree` command.
/// [`git_command`] only translates `root` through the bind mount, so
/// under a container target we translate the destination path too —
/// otherwise the in-container git would create the checkout at a host
/// path it can't see. Falls back to the raw path when translation
/// fails (the same best-effort posture `git_command` itself takes).
fn worktree_path_arg(target: &ShellTarget, path: &Utf8Path) -> String {
	match target {
		ShellTarget::Host => path.to_string(),
		ShellTarget::Container { .. } => target
			.translate_path(path)
			.map(|p| p.to_string())
			.unwrap_or_else(|| path.to_string()),
	}
}

/// `git worktree add -b <branch> <path>` — check out a fresh branch
/// into its own working directory. Validates the branch name with
/// `check-ref-format` first (same gate as
/// [`run_git_commit_on_new_branch`]) so a malformed name fails before
/// anything is written. See ADR 0028.
fn run_git_worktree_add(
	root: &Utf8Path,
	path: &Utf8Path,
	branch: &WorktreeBranch,
	target: &ShellTarget,
) -> MoonResult<GitWorktree> {
	let branch_name = branch.name();
	if branch_name.is_empty() {
		return Err(MoonError::invalid("branch name is empty"));
	}
	// Validate only a *new* branch name we're about to mint. An
	// existing branch is git's to validate when it resolves it —
	// `check-ref-format` would also reject a remote-qualified DWIM
	// target the user is allowed to pass.
	if let WorktreeBranch::New(name) = branch {
		let check = git_command(target, root)
			.args(["check-ref-format", "--branch", name])
			.output()
			.map_err(|e| MoonError::IoError(format!("git check-ref-format failed to launch: {e}")))?;
		if !check.status.success() {
			let stderr = String::from_utf8_lossy(&check.stderr).trim().to_string();
			let detail = if stderr.is_empty() {
				format!("{name:?} is not a valid git branch name")
			} else {
				format!("{name:?}: {stderr}")
			};
			return Err(MoonError::invalid(detail));
		}
	}

	// Worktrees live *inside* the parent repo (`<parent>/.worktrees/…`)
	// with `--relative-paths` links, so they resolve identically on the
	// host and inside the dev container (the parent's bind mount carries
	// the worktree along). That flag needs git >= 2.48; fail clearly so
	// the user updates git rather than getting a cryptic worktree error.
	require_relative_worktree_git(target, root)?;

	let path_arg = worktree_path_arg(target, path);
	// `New(name)`  -> `git worktree add --relative-paths -b <name> <path>`  (fresh branch off HEAD)
	// `Existing(n)` -> `git worktree add --relative-paths <path> <n>`        (check out existing;
	//                  DWIM-creates a local tracking branch from a remote of
	//                  the same name when no local branch exists, like `git switch`)
	let mut cmd = git_command(target, root);
	cmd.args(["worktree", "add", "--relative-paths"]);
	match branch {
		WorktreeBranch::New(name) => {
			cmd.args(["-b", name, &path_arg]);
		}
		WorktreeBranch::Existing(name) => {
			cmd.arg(&path_arg).arg(name);
		}
	}
	let add = cmd
		.output()
		.map_err(|e| MoonError::IoError(format!("git worktree add failed to launch: {e}")))?;
	if !add.status.success() {
		let stderr = String::from_utf8_lossy(&add.stderr).trim().to_string();
		let stdout = String::from_utf8_lossy(&add.stdout).trim().to_string();
		let combined = if stderr.is_empty() { stdout } else { stderr };
		return Err(MoonError::IoError(format!(
			"git worktree add exited {}: {combined}",
			add.status.code().unwrap_or(-1)
		)));
	}

	// Keep `<parent>/.worktrees/` and `/.moon/` out of the parent
	// repo's `git status`. The worktree lives inside the parent now
	// (ADR 0029), and `.moon/AGENTS.md` is a per-dev instructions file
	// (gitignored in moon-ide's own repo, but not in other projects
	// the user opens). Both are local-only, best-effort — a clean
	// status is cosmetic, not worth failing a created worktree.
	for dir in [crate::WORKTREES_DIR_NAME, ".moon"] {
		if let Err(err) = add_dir_to_git_exclude(root, dir, target) {
			tracing::warn!(%err, repo = %root, dir, "could not add to the parent repo's git exclude");
		}
	}

	// Lock the worktree to mark it IDE-managed (WORKTREE_LOCK_REASON)
	// and keep `git worktree prune` from severing it if its checkout
	// is ever temporarily absent. The discard path unlocks before
	// removing (`run_git_worktree_remove`). Best-effort: a lock
	// failure leaves a working (if unprotected) worktree rather than
	// failing create.
	let locked = git_command(target, root)
		.args(["worktree", "lock", "--reason", WORKTREE_LOCK_REASON])
		.arg(&path_arg)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false);
	if !locked {
		tracing::warn!(worktree = %path, "git worktree lock failed; worktree is unprotected from prune");
	}

	// The new branch points at the commit we branched from — i.e.
	// `root`'s current HEAD. Resolve it for the returned record; an
	// empty string is acceptable (the frontend treats it as unknown).
	let head = git_command(target, root)
		.args(["rev-parse", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.unwrap_or_default();

	Ok(GitWorktree {
		path: path.to_string(),
		branch: Some(branch_name.to_owned()),
		head,
		is_main: false,
		is_locked: locked,
	})
}

/// Resolve the repo's git-common-dir via `git rev-parse
/// --git-common-dir`, returning an absolute path. Correct even when
/// `.git` is a file (submodule / linked worktree parent) — that's
/// why `--git-common-dir` rather than `--git-dir`.
fn git_common_dir(root: &Utf8Path, target: &ShellTarget) -> MoonResult<Utf8PathBuf> {
	let out = git_command(target, root)
		.args(["rev-parse", "--git-common-dir"])
		.output()
		.map_err(|e| MoonError::IoError(format!("git rev-parse --git-common-dir failed to launch: {e}")))?;
	if !out.status.success() {
		return Err(MoonError::IoError("git rev-parse --git-common-dir failed".to_string()));
	}
	let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
	let p = Utf8Path::new(&common);
	Ok(if p.is_absolute() { p.to_owned() } else { root.join(p) })
}

/// Idempotently append a `/<dir>/` line to the repo's
/// `.git/info/exclude` so that directory never shows up as untracked
/// in the parent's `git status`. Resolves the real git dir via
/// [`git_common_dir`]. Creates the `info/` dir if missing.
fn add_dir_to_git_exclude(root: &Utf8Path, dir: &str, target: &ShellTarget) -> MoonResult<()> {
	let line = format!("/{dir}/");
	let common_dir = git_common_dir(root, target)?;
	let exclude = common_dir.join("info").join("exclude");
	let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
	if existing.lines().any(|l| l.trim() == line) {
		return Ok(());
	}
	if let Some(dir) = exclude.parent() {
		std::fs::create_dir_all(dir).map_err(MoonError::from)?;
	}
	let mut content = existing;
	if !content.is_empty() && !content.ends_with('\n') {
		content.push('\n');
	}
	content.push_str(&line);
	content.push('\n');
	std::fs::write(&exclude, content).map_err(MoonError::from)?;
	Ok(())
}

/// Path to the repo's `.git/info/exclude` — resolved via
/// [`git_common_dir`] so it's correct even when `.git` is a file.
/// Returns `None` when the repo has no git-common-dir (not a repo,
/// git unavailable).
fn git_exclude_path(root: &Utf8Path, target: &ShellTarget) -> Option<Utf8PathBuf> {
	git_common_dir(root, target)
		.ok()
		.map(|d| d.join("info").join("exclude"))
}

/// Create a worktree, optionally resetting the main tree first (ADR
/// 0028 — "move this session into a worktree"). With `reset_main_to =
/// Some(default)`: requires a clean tree, switches the main checkout
/// to `default` (freeing the current branch), then adds the worktree
/// on `branch`; a failed add rolls the switch back so the main tree
/// isn't stranded on `default` with the branch un-worktree'd. With
/// `None`: a plain [`run_git_worktree_add`].
fn run_git_worktree_add_moving(
	root: &Utf8Path,
	path: &Utf8Path,
	branch: &WorktreeBranch,
	reset_main_to: Option<&str>,
	target: &ShellTarget,
) -> MoonResult<GitWorktree> {
	// Fail fast on old git before touching the main tree (the reset
	// below would otherwise switch then roll back for nothing).
	require_relative_worktree_git(target, root)?;
	if let Some(default) = reset_main_to {
		// `git status --porcelain` lists tracked changes *and*
		// untracked files; either would be left behind by the move (the
		// worktree is a fresh checkout at the branch tip), so block on
		// both rather than silently lose work.
		let status = git_command(target, root)
			.args(["status", "--porcelain"])
			.output()
			.map_err(|e| MoonError::IoError(format!("git status failed to launch: {e}")))?;
		if !status.stdout.is_empty() {
			return Err(MoonError::invalid(
				"Working tree has uncommitted changes — commit or stash them before moving the branch to a worktree.",
			));
		}
		let switch = git_command(target, root)
			.args(["switch", default])
			.output()
			.map_err(|e| MoonError::IoError(format!("git switch failed to launch: {e}")))?;
		if !switch.status.success() {
			let stderr = String::from_utf8_lossy(&switch.stderr).trim().to_string();
			return Err(MoonError::IoError(format!("git switch {default} failed: {stderr}")));
		}
	}

	match run_git_worktree_add(root, path, branch, target) {
		Ok(worktree) => Ok(worktree),
		Err(err) => {
			// Roll the main tree back to the branch we moved off of, so
			// a failed add leaves the checkout exactly where it started.
			if reset_main_to.is_some() {
				if let WorktreeBranch::Existing(original) = branch {
					let _ = git_command(target, root).args(["switch", original]).output();
				}
			}
			Err(err)
		}
	}
}

/// Lowest git that supports `git worktree add --relative-paths`
/// (git 2.48, Jan 2025). Isolated worktree sessions require it:
/// relative links resolve both on the host *and* inside the dev
/// container, where the same worktree is reachable via the parent
/// repo's bind mount at a different absolute path (ADR 0029).
const MIN_GIT_FOR_RELATIVE_WORKTREES: (u32, u32) = (2, 48);

/// Parse `(major, minor)` from `git version`, e.g. `git version
/// 2.48.1` → `(2, 48)`. `None` if git can't run or the output is
/// unparseable.
fn git_major_minor(target: &ShellTarget, root: &Utf8Path) -> Option<(u32, u32)> {
	let out = git_command(target, root).arg("version").output().ok()?;
	if !out.status.success() {
		return None;
	}
	let text = String::from_utf8_lossy(&out.stdout);
	// "git version 2.48.1" (Apple git appends " (Apple Git-…)").
	let version = text.split_whitespace().nth(2)?;
	let mut parts = version.split('.');
	let major = parts.next()?.parse().ok()?;
	let minor = parts.next()?.parse().ok()?;
	Some((major, minor))
}

/// Error unless git is new enough for relative worktree links. Keeps
/// the message actionable so the user updates git rather than hitting
/// a downstream "unknown option `--relative-paths`" failure.
fn require_relative_worktree_git(target: &ShellTarget, root: &Utf8Path) -> MoonResult<()> {
	let (want_major, want_minor) = MIN_GIT_FOR_RELATIVE_WORKTREES;
	match git_major_minor(target, root) {
		Some(v) if v >= MIN_GIT_FOR_RELATIVE_WORKTREES => Ok(()),
		Some((major, minor)) => Err(MoonError::invalid(format!(
			"Isolated worktree sessions need git {want_major}.{want_minor} or newer (for relative \
			 worktree links that work both on the host and inside the dev container). Your git is \
			 {major}.{minor} — please update git and try again."
		))),
		None => Err(MoonError::invalid(format!(
			"Could not determine your git version; isolated worktree sessions need git \
			 {want_major}.{want_minor} or newer."
		))),
	}
}

/// Lock reason stamped on every IDE-managed worktree so
/// `git worktree list` explains why it's locked. See
/// [`run_git_worktree_add`].
const WORKTREE_LOCK_REASON: &str = "moon-ide isolated session (ADR 0028)";

/// `git worktree list --porcelain` → [`GitWorktree`] rows. The main
/// working tree is git's first entry and gets `is_main = true`.
fn run_git_worktree_list(root: &Utf8Path, target: &ShellTarget) -> MoonResult<Vec<GitWorktree>> {
	let output = git_command(target, root)
		.args(["worktree", "list", "--porcelain"])
		.output()
		.map_err(|e| MoonError::IoError(format!("git worktree list failed to launch: {e}")))?;
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		return Err(MoonError::IoError(format!(
			"git worktree list exited {}: {stderr}",
			output.status.code().unwrap_or(-1)
		)));
	}
	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(parse_worktree_porcelain(&stdout))
}

/// Parse `git worktree list --porcelain` output. Blocks are
/// separated by blank lines; each opens with `worktree <path>` and
/// carries `HEAD <sha>`, then either `branch refs/heads/<name>` or
/// `detached`, and optionally `locked` / `bare`. The first block is
/// always the repo's main working tree.
fn parse_worktree_porcelain(stdout: &str) -> Vec<GitWorktree> {
	let mut out = Vec::new();
	let mut cur: Option<GitWorktree> = None;
	let mut is_first = true;
	for line in stdout.lines() {
		if let Some(path) = line.strip_prefix("worktree ") {
			if let Some(done) = cur.take() {
				out.push(done);
			}
			cur = Some(GitWorktree {
				path: path.trim().to_owned(),
				branch: None,
				head: String::new(),
				is_main: std::mem::take(&mut is_first),
				is_locked: false,
			});
		} else if let Some(head) = line.strip_prefix("HEAD ") {
			if let Some(w) = cur.as_mut() {
				w.head = head.trim().to_owned();
			}
		} else if let Some(refname) = line.strip_prefix("branch ") {
			if let Some(w) = cur.as_mut() {
				let name = refname.trim();
				w.branch = Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_owned());
			}
		} else if line.trim() == "detached" {
			if let Some(w) = cur.as_mut() {
				w.branch = None;
			}
		} else if line.trim() == "locked" || line.starts_with("locked ") {
			if let Some(w) = cur.as_mut() {
				w.is_locked = true;
			}
		}
	}
	if let Some(done) = cur.take() {
		out.push(done);
	}
	out
}

/// `git worktree remove [--force] <path>` — prune a linked worktree.
/// Never deletes the branch (ADR 0028: the branch is the deliverable).
///
/// IDE-managed worktrees are locked at creation (see
/// [`run_git_worktree_add`]), and `git worktree remove` refuses a
/// locked worktree, so we unlock first. The unlock is best-effort —
/// it errors harmlessly on an already-unlocked worktree (older
/// worktrees created before locking shipped), which we ignore.
fn run_git_worktree_remove(root: &Utf8Path, path: &Utf8Path, force: bool, target: &ShellTarget) -> MoonResult<()> {
	let path_arg = worktree_path_arg(target, path);
	let _ = git_command(target, root)
		.args(["worktree", "unlock"])
		.arg(&path_arg)
		.output();
	let mut cmd = git_command(target, root);
	cmd.args(["worktree", "remove"]);
	if force {
		cmd.arg("--force");
	}
	cmd.arg(&path_arg);
	let out = cmd
		.output()
		.map_err(|e| MoonError::IoError(format!("git worktree remove failed to launch: {e}")))?;
	if !out.status.success() {
		let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
		let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
		let combined = if stderr.is_empty() { stdout } else { stderr };
		return Err(MoonError::IoError(format!(
			"git worktree remove exited {}: {combined}",
			out.status.code().unwrap_or(-1)
		)));
	}
	Ok(())
}

/// Diff summary the SCM panel feeds to the AI branch-name
/// suggester: `git diff HEAD --stat -M -C --no-color` plus
/// synthesised stat lines for untracked, non-ignored files. The
/// reconciled totals line covers tracked + untracked together so
/// the small model sees a single coherent header rather than two
/// disjoint chunks.
///
/// Same rationale as [`run_git_diff_patch`]: the SCM panel's
/// commit path runs `git add -A` first, so untracked files are
/// part of the eventual commit and naming the branch off
/// tracked-only changes would be misleading. Empty string on full
/// failure (no repo, git unavailable) so callers can keep
/// treating the absence as "nothing to summarise". Char-boundary
/// safe truncation kicks in at ~16 KB.
fn run_git_diff_summary(root: &Utf8Path) -> String {
	const MAX_BYTES: usize = 16_000;

	let tracked = run_git_diff_summary_tracked(root);
	let untracked = collect_untracked_summary_entries(root);
	let combined = merge_diff_summary(&tracked, &untracked);
	cap_summary_at_char_boundary(combined, MAX_BYTES)
}

/// `git diff HEAD --stat=200,80 -M -C --no-color`. Returns the
/// raw stdout on success; empty string on any failure (fresh repo
/// without `HEAD`, git unavailable, etc.). Empty here is fine —
/// the untracked pass downstream still produces a useful summary
/// for the "first commit" case.
fn run_git_diff_summary_tracked(root: &Utf8Path) -> String {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["diff", "HEAD", "--stat=200,80", "-M", "-C", "--no-color"])
		.output();
	let Ok(output) = output else {
		return String::new();
	};
	if !output.status.success() {
		return String::new();
	}
	String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Per-untracked-file summary entry: enough to render a stat line
/// and feed the totals reconciliation. `lines` is `None` for
/// binary files so the merge step can render git's `Bin` marker
/// without having to re-detect.
struct UntrackedSummary {
	path: String,
	lines: Option<usize>,
}

/// Walk untracked, non-ignored files and synthesise a summary
/// entry per file. Skips files we can't read (race with
/// concurrent edits, permission errors); dropped files are silent
/// because the summary is best-effort context for an LLM, not a
/// load-bearing signal.
fn collect_untracked_summary_entries(root: &Utf8Path) -> Vec<UntrackedSummary> {
	use std::process::Command;

	const BINARY_PROBE: usize = 8_000;

	let listing = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["ls-files", "--others", "--exclude-standard", "-z"])
		.output();
	let Ok(listing) = listing else {
		return Vec::new();
	};
	if !listing.status.success() {
		return Vec::new();
	}
	let mut entries = Vec::new();
	for entry in listing.stdout.split(|&b| b == 0) {
		if entry.is_empty() {
			continue;
		}
		let Ok(rel_path) = std::str::from_utf8(entry) else {
			continue;
		};
		let abs = root.join(rel_path);
		let Ok(bytes) = std::fs::read(abs.as_std_path()) else {
			continue;
		};
		let probe = &bytes[..bytes.len().min(BINARY_PROBE)];
		let lines = if probe.contains(&0) {
			None
		} else {
			// Match `git diff --stat`: a final trailing newline
			// closes the previous line rather than starting a new
			// empty one.
			let count = bytes.iter().filter(|b| **b == b'\n').count();
			let extra = if !bytes.is_empty() && !bytes.ends_with(b"\n") {
				1
			} else {
				0
			};
			Some(count + extra)
		};
		entries.push(UntrackedSummary {
			path: rel_path.to_owned(),
			lines,
		});
	}
	entries
}

/// Splice `untracked` entries into the tracked stat output and
/// rewrite the trailing `N files changed, ...` totals line so it
/// covers both. When tracked is empty (fresh repo, no commits
/// yet, etc.) and there's nothing untracked either, returns an
/// empty string so callers keep the "nothing to summarise"
/// short-circuit. Pure on string inputs; tested directly.
fn merge_diff_summary(tracked: &str, untracked: &[UntrackedSummary]) -> String {
	if tracked.is_empty() && untracked.is_empty() {
		return String::new();
	}
	let (entries_block, prior_totals) = split_diff_summary(tracked);
	let mut out = entries_block.to_string();
	if !out.is_empty() && !out.ends_with('\n') {
		out.push('\n');
	}
	for entry in untracked {
		out.push_str(&format_untracked_stat_line(entry));
		out.push('\n');
	}
	let totals = reconcile_totals_line(prior_totals, untracked);
	if !totals.is_empty() {
		out.push_str(&totals);
		out.push('\n');
	}
	out
}

/// Split the raw `git diff --stat` output into the per-file
/// entries (everything except the trailing summary line) and the
/// summary line itself. Returns `("", "")` for an empty input.
/// The split is line-based: the totals line is always last and
/// always begins with ` N files changed,` / ` N file changed,`.
fn split_diff_summary(tracked: &str) -> (&str, &str) {
	if tracked.is_empty() {
		return ("", "");
	}
	let trimmed = tracked.trim_end_matches('\n');
	let Some(last_newline) = trimmed.rfind('\n') else {
		// Single-line input: either pure totals or pure entry.
		// Treat the totals shape as totals; otherwise fall through
		// as a single entry with no totals line.
		if looks_like_summary_totals(trimmed) {
			return ("", trimmed);
		}
		return (trimmed, "");
	};
	let last_line = &trimmed[last_newline + 1..];
	if looks_like_summary_totals(last_line) {
		return (&trimmed[..last_newline], last_line);
	}
	(trimmed, "")
}

fn looks_like_summary_totals(line: &str) -> bool {
	let stripped = line.trim_start();
	stripped.starts_with(|c: char| c.is_ascii_digit())
		&& (stripped.contains("file changed") || stripped.contains("files changed"))
}

/// Render a single untracked-file stat line that mirrors `git
/// diff --stat`'s shape. We don't reproduce git's auto-scaled bar
/// width (it'd need cross-file knowledge for a tiny visual win the
/// LLM doesn't care about); a fixed-cap bar of `+` characters is
/// good enough.
fn format_untracked_stat_line(entry: &UntrackedSummary) -> String {
	const MAX_BAR: usize = 50;

	match entry.lines {
		None => format!(" {} | Bin 0 -> ? bytes", entry.path),
		Some(lines) => {
			let bar_width = lines.min(MAX_BAR);
			let bar = "+".repeat(bar_width);
			format!(" {} | {lines} {bar}", entry.path)
		}
	}
}

/// Build a single totals line covering both the existing
/// `prior_totals` (if any) and the untracked entries we're
/// appending. The line shape matches what git itself emits so the
/// model sees one continuous summary; we recompute counts rather
/// than appending a second totals line because that would read as
/// stale / contradictory.
fn reconcile_totals_line(prior_totals: &str, untracked: &[UntrackedSummary]) -> String {
	let (mut files, mut insertions, deletions) = parse_totals_line(prior_totals);
	for entry in untracked {
		files += 1;
		insertions += entry.lines.unwrap_or(0);
	}
	if files == 0 {
		return String::new();
	}
	let file_word = if files == 1 { "file" } else { "files" };
	let mut out = format!(" {files} {file_word} changed");
	if insertions > 0 {
		let word = if insertions == 1 { "insertion" } else { "insertions" };
		out.push_str(&format!(", {insertions} {word}(+)"));
	}
	if deletions > 0 {
		let word = if deletions == 1 { "deletion" } else { "deletions" };
		out.push_str(&format!(", {deletions} {word}(-)"));
	}
	out
}

/// Pull the (files, insertions, deletions) tuple out of git's
/// own totals line. Returns zeroed counts when the line is empty
/// or doesn't parse — we tolerate parse failures because the
/// caller's recompute pass still produces something usable
/// (untracked-only totals).
fn parse_totals_line(line: &str) -> (usize, usize, usize) {
	let mut files = 0usize;
	let mut insertions = 0usize;
	let mut deletions = 0usize;
	for chunk in line.split(',') {
		let trimmed = chunk.trim();
		let Some((num_str, _)) = trimmed.split_once(' ') else {
			continue;
		};
		let Ok(num) = num_str.parse::<usize>() else {
			continue;
		};
		if trimmed.contains("file") {
			files = num;
		} else if trimmed.contains("insertion") {
			insertions = num;
		} else if trimmed.contains("deletion") {
			deletions = num;
		}
	}
	(files, insertions, deletions)
}

/// Cap `text` at `cap` bytes, trimming back to the previous char
/// boundary so we don't slice through a multi-byte path, and
/// append a `[truncated]` marker when truncation actually
/// happened.
fn cap_summary_at_char_boundary(text: String, cap: usize) -> String {
	if text.len() <= cap {
		return text;
	}
	let mut idx = cap;
	while idx > 0 && !text.is_char_boundary(idx) {
		idx -= 1;
	}
	let mut clipped = text[..idx].to_owned();
	clipped.push_str("\n[truncated]");
	clipped
}

/// Push the current branch. Plain `git push` in almost every
/// case — we only override git's own routing with an explicit
/// `git push <remote> HEAD:<merge_ref>` refspec for the fork-PR
/// shape, detected as a configured upstream whose `@{u}` does
/// *not* resolve to a remote-tracking ref (a bare-URL remote).
///
/// `gh pr checkout` on a fork PR names the local branch
/// `<owner>/<head>` (to avoid colliding with the upstream's
/// default branch) and points `branch.<name>.remote` at the
/// fork URL with `merge` = `refs/heads/<head>`. Plain `git push`
/// dies there with `fatal: The upstream branch of your current
/// branch does not match the name of your current branch`
/// (`push.default = simple`, git's default since 2.0), so the
/// explicit refspec tells git exactly what we want.
///
/// For a *tracked* named-remote upstream we deliberately keep
/// plain `git push` so git's `simple` name-match safety stays in
/// force. That matters for the foreign-upstream shape — a branch
/// created with `git checkout -b feature origin/main` tracks
/// `refs/heads/main`, and an explicit `HEAD:refs/heads/main`
/// would fast-forward the shared branch (land the feature
/// commits on `main`). The SCM panel surfaces "Publish branch"
/// for that state (and for the no-upstream state) so the user
/// rarely reaches this with such a branch, but the gate here is
/// the real guarantee.
///
/// Stderr is surfaced verbatim on failure (auth refused, non-
/// fast-forward, network down, …) so the user gets git's own
/// actionable hint.
fn run_git_push(root: &Utf8Path) -> MoonResult<()> {
	use std::process::Command;

	let name = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["symbolic-ref", "--quiet", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty());

	if let Some(branch) = name {
		if let Some((remote, merge)) = read_branch_upstream(root, &branch) {
			// Only override git's own push routing for the fork-PR
			// shape: a URL remote with no remote-tracking ref, where
			// the local branch name intentionally differs from the
			// PR head and `push.default = simple` would refuse. We
			// detect it by `@{u}` *not* resolving. For any tracked
			// named-remote upstream we defer to plain `git push` so
			// git's own name-match safety stays in force — crucially,
			// that stops us from fast-forwarding a *foreign* tracked
			// branch (a feature branch made with `git checkout -b foo
			// origin/main` tracks `refs/heads/main`; pushing
			// `HEAD:refs/heads/main` would land the feature commits on
			// `main`). The SCM panel routes such branches to
			// `git_publish_branch` instead, but we guard here too so a
			// direct `git_push` call can never do the wrong thing.
			let tracked = Command::new("git")
				.arg("-C")
				.arg(root.as_std_path())
				.args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
				.output()
				.ok()
				.is_some_and(|o| o.status.success());
			if !tracked {
				let refspec = format!("HEAD:{merge}");
				return run_git_simple(root, &["push", &remote, &refspec], "git push");
			}
		}
	}
	run_git_simple(root, &["push"], "git push")
}

/// Run `git <args>` from `root` and surface stderr verbatim on
/// failure. Used by `git_pull` (and any future "shoot a git
/// command and see if it worked" SCM action) so network / auth /
/// merge-conflict messages reach the user without us second-
/// guessing their wording.
fn run_git_simple(root: &Utf8Path, args: &[&str], label: &str) -> MoonResult<()> {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(args)
		.output()
		.map_err(|e| MoonError::IoError(format!("{label} failed to launch: {e}")))?;
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
		let combined = match (stderr.is_empty(), stdout.is_empty()) {
			(false, _) => stderr,
			(true, false) => stdout,
			(true, true) => format!("exit {}", output.status.code().unwrap_or(-1)),
		};
		return Err(MoonError::IoError(format!("{label}: {combined}")));
	}
	Ok(())
}

/// Read the working tree's in-flight merge state. Pure I/O:
///
/// 1. `.git/MERGE_HEAD` existence drives `in_progress`. When it
///    isn't there, every other field stays at its default and we
///    skip the rest of the probes.
/// 2. `.git/MERGE_HEAD`'s first line carries the SHA being
///    merged in. We hand it to `git name-rev --name-only` to
///    get back something human-recognisable (`origin/main`
///    rather than 40 hex chars). Falls back to the short SHA
///    when `name-rev` can't resolve a name — pathological case,
///    but cheaper than panicking the UI.
/// 3. `.git/MERGE_MSG` is git's default commit message for the
///    merge (set the moment `git merge` is invoked). Used by
///    the SCM panel to prefill the composer when the user
///    enters merge-in-progress mode without a draft.
/// 4. `git ls-files --unmerged -z` lists conflicted entries.
///    A single conflicted file shows up three times (one per
///    stage); we dedupe by path so the count matches what the
///    SCM panel renders.
///
/// All probes are best-effort: an I/O failure on any one of
/// them collapses that field to its default without dropping
/// the others. The most common cause of a transient failure is
/// a concurrent `git merge --abort` racing the read — by the
/// next refresh tick the consistent post-abort state lands.
fn read_git_merge_state(root: &Utf8Path) -> GitMergeState {
	use std::process::Command;

	let merge_head_path = root.join(".git").join("MERGE_HEAD");
	let merge_head_raw = match std::fs::read_to_string(merge_head_path.as_std_path()) {
		Ok(s) => s,
		Err(_) => return GitMergeState::default(),
	};
	let merge_head = merge_head_raw
		.lines()
		.next()
		.map(|s| s.trim().to_owned())
		.unwrap_or_default();
	if merge_head.is_empty() {
		return GitMergeState::default();
	}

	// `name-rev --name-only` resolves a SHA to the closest
	// ref-shaped name (`origin/main`, `tags/v1.2`, …).
	// `--no-undefined` returns non-zero rather than the literal
	// "undefined" placeholder when there's no resolvable name,
	// which we'd rather fall through to the short SHA fallback
	// for.
	let merging_ref = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["name-rev", "--name-only", "--no-undefined", &merge_head])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty())
		.or_else(|| {
			// Short-SHA fallback when `name-rev` declined.
			// `rev-parse --short` is idempotent on an already-
			// 40-char hex string, and on failure we just hand
			// back the raw long SHA so the panel still has
			// something to render.
			Some(
				Command::new("git")
					.arg("-C")
					.arg(root.as_std_path())
					.args(["rev-parse", "--short", &merge_head])
					.output()
					.ok()
					.filter(|o| o.status.success())
					.and_then(|o| String::from_utf8(o.stdout).ok())
					.map(|s| s.trim().to_owned())
					.filter(|s| !s.is_empty())
					.unwrap_or(merge_head.clone()),
			)
		});

	let merge_msg_path = root.join(".git").join("MERGE_MSG");
	let default_message = std::fs::read_to_string(merge_msg_path.as_std_path())
		.ok()
		.map(|s| s.trim_end_matches('\n').to_owned())
		.filter(|s| !s.is_empty());

	let unmerged_paths = list_unmerged_paths(root);

	GitMergeState {
		in_progress: true,
		merging_ref,
		default_message,
		unmerged_paths,
	}
}

/// `git ls-files --unmerged -z` — one record per index stage,
/// `<mode> <sha> <stage>\tpath\0`. We only need the path; multiple
/// stages for the same file collapse to a single entry because the
/// SCM panel renders per-path, not per-stage.
///
/// Returns an empty vec on any failure (not a repo, git missing,
/// no unmerged entries) — same "render no badges" fallback
/// `read_git_merge_state` relies on.
fn list_unmerged_paths(root: &Utf8Path) -> Vec<String> {
	use std::process::Command;

	let output = match Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["ls-files", "--unmerged", "-z"])
		.output()
	{
		Ok(o) if o.status.success() => o,
		_ => return Vec::new(),
	};

	// `-z` makes each record null-terminated and uses a tab
	// between the metadata and the path. The metadata block is
	// `<mode> <sha> <stage>`, so a `split_once('\t')` peels off
	// the path. Dedupe with an ordered set so the surface stays
	// deterministic across calls.
	use std::collections::BTreeSet;
	let mut paths: BTreeSet<String> = BTreeSet::new();
	for record in output.stdout.split(|&b| b == 0) {
		if record.is_empty() {
			continue;
		}
		let Ok(text) = std::str::from_utf8(record) else {
			continue;
		};
		let Some((_, path)) = text.split_once('\t') else {
			continue;
		};
		if path.is_empty() {
			continue;
		}
		paths.insert(path.replace('\\', "/"));
	}
	paths.into_iter().collect()
}

/// `git log -1 --pretty=%B` for the current `HEAD`. Returns the
/// raw subject + body verbatim (with whatever trailing newlines git
/// emits stripped); empty string on any failure (no repo, no
/// commits yet, git unavailable). Synchronous; runs on the
/// blocking pool via `git_head_commit_message`.
fn run_git_head_commit_message(root: &Utf8Path) -> String {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["log", "-1", "--pretty=%B"])
		.output();
	let Ok(output) = output else {
		return String::new();
	};
	if !output.status.success() {
		return String::new();
	}
	String::from_utf8(output.stdout)
		.unwrap_or_default()
		.trim_end_matches('\n')
		.to_string()
}

/// Recent commits on the current branch for the SCM panel's
/// recent-commits list. `git log` with a `%x1f` (unit separator)
/// delimiter between fields so we can split each line cleanly
/// without fighting git's default newline-joined pretty format.
/// Returns empty vec on any failure (no repo, no commits, git
/// unavailable).
fn run_git_log(root: &Utf8Path, limit: u32) -> Vec<CommitEntry> {
	use std::process::Command;

	let limit = limit.clamp(1, 200);
	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args([
			"log",
			&format!("-{limit}"),
			"--pretty=%H%x1f%h%x1f%s%x1f%an%x1f%cr",
			"--no-color",
		])
		.output();
	let Ok(output) = output else {
		return Vec::new();
	};
	if !output.status.success() {
		return Vec::new();
	}
	let stdout = String::from_utf8_lossy(&output.stdout);
	stdout
		.lines()
		.filter_map(|line| {
			let mut parts = line.splitn(5, '\u{1f}');
			let sha = parts.next()?.to_owned();
			let short_sha = parts.next()?.to_owned();
			let subject = parts.next()?.to_owned();
			let author = parts.next()?.to_owned();
			let date_relative = parts.next()?.to_owned();
			Some(CommitEntry {
				sha,
				short_sha,
				subject,
				author,
				date_relative,
			})
		})
		.collect()
}

/// File-level diff for a single commit vs its first parent. Resolves
/// `<sha>^` to a real 40-hex SHA (since `is_safe_rev` rejects the `^`
/// suffix downstream), then runs `git diff --name-status -z --no-renames`
/// between parent and commit and parses with the existing
/// `parse_diff_name_status_z`. Root commits (no parent) produce an empty
/// `parent_sha`; every file reads as a pure addition. Returns `None`
/// when the commit doesn't exist, isn't a repo, or git is unavailable.
fn run_git_commit_diff(root: &Utf8Path, sha: &str) -> Option<CommitDiff> {
	use std::process::Command;

	// Validate the commit SHA is a 40-char hex string before passing
	// it to git — mirrors `is_safe_rev` so we never feed git a
	// flag-shaped string.
	if !is_safe_rev(sha) {
		return None;
	}

	// Resolve the first parent: `git rev-parse <sha>^`. For a root
	// commit this exits non-zero; we treat that as "no parent" and
	// leave `parent_sha` empty so the frontend renders every file as
	// a pure addition.
	let parent = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["rev-parse", &format!("{sha}^")])
		.output()
		.ok()?;
	let parent_sha = if parent.status.success() {
		String::from_utf8_lossy(&parent.stdout).trim().to_owned()
	} else {
		String::new()
	};

	// Grab the subject for the banner.
	let subject_output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["log", "-1", "--pretty=%s", sha])
		.output()
		.ok()?;
	let subject = if subject_output.status.success() {
		String::from_utf8_lossy(&subject_output.stdout).trim().to_owned()
	} else {
		return None;
	};

	// Root commit: every tracked file is an addition. `git diff`
	// against a non-existent parent would fail, so diff against the
	// empty tree instead.
	let diff_arg = if parent_sha.is_empty() {
		format!("4b825dc642cb6eb9a060e54bf8d69288fbee4904..{sha}")
	} else {
		format!("{parent_sha}..{sha}")
	};
	let diff = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["diff", "--name-status", "-z", "--no-renames", &diff_arg])
		.output()
		.ok()?;
	if !diff.status.success() {
		return None;
	}
	let entries = parse_diff_name_status_z(&diff.stdout);
	Some(CommitDiff {
		parent_sha,
		commit_sha: sha.to_owned(),
		subject,
		entries,
	})
}

/// Working-tree patch the SCM panel feeds to the AI commit-message
/// suggester: `git diff HEAD --no-color` plus a synthesised
/// "new file" entry per untracked, non-ignored file. Byte-capped
/// at ~64 KB; the cap truncates at the next newline boundary so we
/// don't hand a half-formed hunk header to the LLM.
///
/// Untracked files are appended because the SCM panel's commit
/// path runs `git add -A` before `git commit`, so brand-new files
/// **are** committed. `git diff HEAD` alone misses them, which
/// would leave the model writing a commit message that ignores
/// the new files entirely. The synthesised entry mirrors what
/// `git diff` would emit for the same file once it's been added,
/// so the model sees a homogeneous patch.
///
/// Empty string when there's nothing to surface (clean tree, not
/// a repo, git unavailable). Errors on the underlying commands
/// are swallowed — this is a best-effort hint, not a load-bearing
/// signal.
fn run_git_diff_patch(root: &Utf8Path) -> String {
	const MAX_BYTES: usize = 64_000;

	let mut combined = run_git_diff_head(root);
	if combined.len() < MAX_BYTES {
		append_untracked_synthesised_patches(root, &mut combined, MAX_BYTES);
	}
	cap_patch_at_newline(combined, MAX_BYTES)
}

/// `git diff HEAD --no-color`. Returns whatever git emitted on
/// success; empty string on any failure (no repo, no commits yet,
/// git unavailable). A repo with no `HEAD` commit is the common
/// "fail" case; in that scenario the untracked-files pass
/// downstream still produces a useful patch, so callers can rely
/// on the combined output being non-empty whenever there's
/// anything at all to commit.
fn run_git_diff_head(root: &Utf8Path) -> String {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["diff", "HEAD", "--no-color"])
		.output();
	let Ok(output) = output else {
		return String::new();
	};
	if !output.status.success() {
		return String::new();
	}
	String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Walk every untracked, non-ignored file under `root` and append
/// a synthesised "new file" diff entry to `combined`. Stops as
/// soon as `combined` reaches `cap` so the caller's truncation
/// pass has bytes to work with.
///
/// Binary files (heuristic: any null byte in the first 8 KB)
/// surface as the same `Binary files /dev/null and b/<path>
/// differ` line real `git diff` emits, so the model sees the file
/// is part of the commit without us shovelling raw bytes into the
/// prompt.
fn append_untracked_synthesised_patches(root: &Utf8Path, combined: &mut String, cap: usize) {
	use std::process::Command;

	// `-z` so paths with spaces / quotes survive a single `\0` split
	// without git applying its quote-escape pass. `--exclude-standard`
	// drops `.gitignore`-matched paths so we don't slurp in the
	// dev's `node_modules/`.
	let listing = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["ls-files", "--others", "--exclude-standard", "-z"])
		.output();
	let Ok(listing) = listing else {
		return;
	};
	if !listing.status.success() {
		return;
	}
	for entry in listing.stdout.split(|&b| b == 0) {
		if entry.is_empty() {
			continue;
		}
		if combined.len() >= cap {
			return;
		}
		let Ok(rel_path) = std::str::from_utf8(entry) else {
			continue;
		};
		let abs = root.join(rel_path);
		let Ok(bytes) = std::fs::read(abs.as_std_path()) else {
			continue;
		};
		combined.push_str(&synthesise_new_file_patch(rel_path, &bytes));
	}
}

/// Build a `git diff`-shaped "new file" entry for `bytes` so the
/// LLM sees an untracked file the same way it sees a tracked
/// modification. The hash field is a placeholder zero — the
/// receiving prompt only reads the structural envelope and the
/// content lines, not the SHA.
fn synthesise_new_file_patch(rel_path: &str, bytes: &[u8]) -> String {
	const BINARY_PROBE: usize = 8_000;

	let probe = &bytes[..bytes.len().min(BINARY_PROBE)];
	let header = format!("diff --git a/{rel_path} b/{rel_path}\nnew file mode 100644\nindex 0000000..0000000\n");
	if probe.contains(&0) {
		return format!("{header}Binary files /dev/null and b/{rel_path} differ\n");
	}
	let Ok(text) = std::str::from_utf8(bytes) else {
		return format!("{header}Binary files /dev/null and b/{rel_path} differ\n");
	};
	if text.is_empty() {
		// Empty file: still emit the header so the path is
		// surfaced to the model. No hunk body — git itself emits
		// no `@@` header for zero-length new files either.
		return format!("{header}--- /dev/null\n+++ b/{rel_path}\n");
	}
	let trailing_newline = text.ends_with('\n');
	let body_lines: Vec<&str> = if trailing_newline {
		text.strip_suffix('\n').unwrap_or(text).split('\n').collect()
	} else {
		text.split('\n').collect()
	};
	let line_count = body_lines.len();
	let mut out = String::with_capacity(header.len() + bytes.len() + 64);
	out.push_str(&header);
	out.push_str(&format!(
		"--- /dev/null\n+++ b/{rel_path}\n@@ -0,0 +1,{line_count} @@\n"
	));
	for line in &body_lines {
		out.push('+');
		out.push_str(line);
		out.push('\n');
	}
	if !trailing_newline {
		// Mirror real git so the model can tell the file has no
		// final newline (matters for some lints / tools).
		out.push_str("\\ No newline at end of file\n");
	}
	out
}

/// Trim `combined` so the result is at most `cap` bytes and ends
/// at a newline boundary, with a trailing `... (diff truncated)`
/// marker when truncation actually happened. Pure function;
/// extracted so the assembly path above can keep its append logic
/// flat.
fn cap_patch_at_newline(combined: String, cap: usize) -> String {
	if combined.len() <= cap {
		return combined;
	}
	// Cut just past the last newline at or before `cap` so the
	// trailing chunk handed to the LLM is structurally complete
	// (no half-line hunk headers). `+ 1` includes the newline
	// itself, so the prefix ends in `\n` and the sentinel sits on
	// its own line. Hard byte cut as a last resort if the prefix
	// has no newline at all (pathologically long single line).
	let cut = combined[..cap].rfind('\n').map(|i| i + 1).unwrap_or(cap);
	let mut out = combined[..cut].to_owned();
	out.push_str("... (diff truncated)\n");
	out
}

/// `git fetch --quiet --no-tags` with prompts disabled and a 30s
/// timeout. Used by the periodic auto-fetch loop so the upstream
/// tracking ref (`refs/remotes/origin/<branch>`) refreshes without
/// the user clicking anything; `git_branch`'s ahead/behind read
/// then surfaces the new "Sync Changes" affordance.
///
/// Async on purpose: `tokio::process::Command` + `tokio::time::timeout`
/// gives us the deadline for free; the existing `run_git_simple`
/// (sync, on the blocking pool) has no timeout and would let a
/// hung fetch park a worker indefinitely.
async fn run_git_fetch_quiet(root: &Utf8Path) -> MoonResult<()> {
	use std::process::Stdio;
	use tokio::process::Command;
	use tokio::time::{timeout, Duration};

	const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

	let mut cmd = Command::new("git");
	cmd.arg("-C")
		.arg(root.as_std_path())
		.args(["fetch", "--quiet", "--no-tags"])
		// Without these env knobs a remote that needs auth (HTTPS
		// without a credential helper, or SSH without an agent)
		// hangs waiting on stdin we can't even render. Fail fast so
		// the auto-fetch loop logs and moves on.
		.env("GIT_TERMINAL_PROMPT", "0")
		.env("GIT_ASKPASS", "")
		.env("SSH_ASKPASS", "")
		// `LC_ALL=C` matches the convention used elsewhere
		// (`run_git_commit`) so the few stderr matches we do later
		// don't drift on localised installs.
		.env("LC_ALL", "C")
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::piped());

	let child = cmd
		.spawn()
		.map_err(|e| MoonError::IoError(format!("git fetch failed to launch: {e}")))?;

	let output = match timeout(FETCH_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(e)) => return Err(MoonError::IoError(format!("git fetch: {e}"))),
		Err(_) => {
			return Err(MoonError::IoError(format!(
				"git fetch: timed out after {}s",
				FETCH_TIMEOUT.as_secs()
			)));
		}
	};

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		let detail = if stderr.is_empty() {
			format!("exit {}", output.status.code().unwrap_or(-1))
		} else {
			stderr
		};
		// `tracing::debug!` (not `warn!`) because the auto-fetch loop
		// hits this on every offline / no-upstream / auth-refused
		// run; promoting them to warn would spam dev terminals.
		// `RUST_LOG=moon_core=debug` is the supported channel for
		// triaging "why isn't Sync Changes appearing?".
		tracing::debug!(root = %root, detail = %detail, "git_fetch failed");
		return Err(MoonError::IoError(format!("git fetch: {detail}")));
	}

	Ok(())
}

/// FNV-1a 32-bit hash of the trimmed-and-joined anchored line text.
/// MUST match the frontend `reviewFingerprint` / `reviewLineFingerprint`
/// byte-for-byte (trim each line, join with `\n`, FNV-1a, 8-hex-char
/// lowercase) so a fingerprint written at comment-creation time
/// re-resolves here against the PR-head version of the file.
fn review_fingerprint(line_text: &str) -> String {
	let normalized = line_text.split('\n').map(str::trim).collect::<Vec<_>>().join("\n");
	let mut hash: u32 = 0x811c_9dc5;
	for ch in normalized.chars() {
		// The frontend hashes UTF-16 code units (`charCodeAt`); for
		// the BMP that equals the char's code point. Non-BMP chars in
		// an anchored line are vanishingly rare for source review;
		// match the common case (code point == code unit) and accept
		// that an astral-plane char would disagree (the comment just
		// goes "lost" and stays a local draft, which is the safe
		// failure mode).
		hash ^= ch as u32;
		hash = hash.wrapping_mul(0x0100_0193);
	}
	format!("{hash:08x}")
}

/// Re-locate a comment's anchor in `lines` (0-indexed file content)
/// by fingerprint, mirroring the frontend `resolveAnchor`. Returns
/// the 1-based `(start_line, end_line)` where the content now sits,
/// or `None` if it can't be found within the search window (the
/// "lost" case — the line changed at the PR head out from under us).
fn resolve_anchor_in(lines: &[&str], start_line: u32, end_line: u32, fingerprint: &str) -> Option<(u32, u32)> {
	const RADIUS: i64 = 40;
	let span = end_line.saturating_sub(start_line) as i64;
	let total = lines.len() as i64;
	let range_text = |start_1based: i64| -> Option<String> {
		let end_1based = start_1based + span;
		if start_1based < 1 || end_1based > total {
			return None;
		}
		let from = (start_1based - 1) as usize;
		let to = end_1based as usize;
		Some(lines[from..to].join("\n"))
	};
	let hint = start_line as i64;
	if let Some(text) = range_text(hint) {
		if review_fingerprint(&text) == fingerprint {
			return Some((start_line, (start_line as i64 + span) as u32));
		}
	}
	for delta in 1..=RADIUS {
		for dir in [-1_i64, 1] {
			let start = hint + dir * delta;
			if let Some(text) = range_text(start) {
				if review_fingerprint(&text) == fingerprint {
					return Some((start as u32, (start + span) as u32));
				}
			}
		}
	}
	None
}

/// `gh` timeout for the publish round-trips. Same ceiling as the PR
/// list — a GitHub review POST is a single API call.
const GH_PUBLISH_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// Resolve the open PR for the current branch via
/// `gh pr view --json number,headRefOid,headRefName`. Returns
/// `Ok(None)` when there's no PR (gh exits non-zero / empty), which
/// the caller surfaces as [`PublishReviewResult::NoPr`].
async fn gh_pr_head(root: &Utf8Path) -> MoonResult<Option<(u64, String)>> {
	let mut cmd = tokio::process::Command::new("gh");
	cmd
		.current_dir(root.as_std_path())
		.args(["pr", "view", "--json", "number,headRefOid,state"])
		.env("GH_PROMPT_DISABLED", "1")
		.env("LC_ALL", "C")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped());
	let child = match cmd.spawn() {
		Ok(c) => c,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
			return Err(MoonError::invalid("gh is not installed"));
		}
		Err(err) => return Err(MoonError::IoError(format!("gh pr view spawn: {err}"))),
	};
	let output = match tokio::time::timeout(GH_PUBLISH_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(err)) => return Err(MoonError::IoError(format!("gh pr view: {err}"))),
		Err(_) => return Err(MoonError::IoError("gh pr view timed out".to_owned())),
	};
	if !output.status.success() {
		// gh exits non-zero when the branch has no associated PR —
		// the expected "nothing to publish to" signal.
		return Ok(None);
	}
	let value: serde_json::Value =
		serde_json::from_slice(&output.stdout).map_err(|e| MoonError::Internal(format!("gh pr view JSON: {e}")))?;
	let number = value.get("number").and_then(serde_json::Value::as_u64);
	let head = value
		.get("headRefOid")
		.and_then(serde_json::Value::as_str)
		.map(str::to_owned);
	match (number, head) {
		(Some(n), Some(h)) if !h.is_empty() => Ok(Some((n, h))),
		_ => Ok(None),
	}
}

/// Fetch the PR-head version of `path` (`git show <head>:<path>`).
/// `None` when the path isn't in the head tree (a comment on a file
/// the head doesn't have → every comment on it goes "lost").
async fn git_show_at(root: &Utf8Path, rev: &str, path: &str) -> Option<String> {
	let spec = format!("{}:{}", rev, path.replace('\\', "/"));
	let output = tokio::process::Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.arg("show")
		.arg(&spec)
		.env("LC_ALL", "C")
		.output()
		.await
		.ok()?;
	if !output.status.success() {
		return None;
	}
	String::from_utf8(output.stdout).ok()
}

/// `WorkspaceHost::publish_pr_review` body. See the trait doc for
/// the flow. Kept as a free async fn so it composes the `gh` / `git`
/// subprocess calls without holding the blocking pool.
async fn run_publish_pr_review(root: &Utf8Path, request: PublishReviewRequest) -> MoonResult<PublishReviewResult> {
	// 1. Resolve the PR + head SHA. No PR → caller shows create-PR CTA.
	let branch = run_git_branch(root).name.unwrap_or_default();
	let Some((number, head_sha)) = gh_pr_head(root).await? else {
		return Ok(PublishReviewResult::NoPr { branch });
	};

	// 2. Reconcile each comment's anchor against the PR-head file.
	//    Cache head-version content per path so N comments on one
	//    file only fetch it once.
	let mut head_files: std::collections::HashMap<String, Option<Vec<String>>> = std::collections::HashMap::new();
	let mut review_comments: Vec<serde_json::Value> = Vec::new();
	let mut lost: Vec<String> = Vec::new();

	for comment in &request.comments {
		let path = &comment.anchor.path;
		if !head_files.contains_key(path) {
			let content = git_show_at(root, &head_sha, path).await;
			let lines = content.map(|c| c.split('\n').map(str::to_owned).collect::<Vec<_>>());
			head_files.insert(path.clone(), lines);
		}
		let Some(Some(owned_lines)) = head_files.get(path) else {
			// File not present at head → comment can't be placed.
			lost.push(comment.id.clone());
			continue;
		};
		let refs: Vec<&str> = owned_lines.iter().map(String::as_str).collect();
		match resolve_anchor_in(
			&refs,
			comment.anchor.start_line,
			comment.anchor.end_line,
			&comment.anchor.fingerprint,
		) {
			Some((start, end)) => {
				let side = match comment.anchor.side {
					ReviewSide::Base => "LEFT",
					ReviewSide::Working => "RIGHT",
				};
				let mut obj = serde_json::Map::new();
				obj.insert("path".to_owned(), serde_json::Value::String(path.clone()));
				obj.insert("body".to_owned(), serde_json::Value::String(comment.body.clone()));
				obj.insert("line".to_owned(), serde_json::Value::Number(end.into()));
				obj.insert("side".to_owned(), serde_json::Value::String(side.to_owned()));
				if end > start {
					obj.insert("start_line".to_owned(), serde_json::Value::Number(start.into()));
					obj.insert("start_side".to_owned(), serde_json::Value::String(side.to_owned()));
				}
				review_comments.push(serde_json::Value::Object(obj));
			}
			None => lost.push(comment.id.clone()),
		}
	}

	if review_comments.is_empty() {
		// Nothing placed — don't post an empty review. Report all as
		// lost so the UI keeps them and explains why.
		return Ok(PublishReviewResult::Published {
			posted: 0,
			lost,
			review_url: String::new(),
		});
	}

	// 3. Post one atomic COMMENT review via `gh api`.
	let mut payload = serde_json::Map::new();
	payload.insert("commit_id".to_owned(), serde_json::Value::String(head_sha.clone()));
	payload.insert("event".to_owned(), serde_json::Value::String("COMMENT".to_owned()));
	if let Some(body) = request.body.as_ref().filter(|b| !b.trim().is_empty()) {
		payload.insert("body".to_owned(), serde_json::Value::String(body.clone()));
	}
	let posted_count = review_comments.len() as u32;
	payload.insert("comments".to_owned(), serde_json::Value::Array(review_comments));
	let body_json = serde_json::Value::Object(payload).to_string();

	let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews");
	let review_url = gh_api_post_review(root, &endpoint, &body_json).await?;

	Ok(PublishReviewResult::Published {
		posted: posted_count,
		lost,
		review_url,
	})
}

/// POST a review payload via `gh api --input -` (reads JSON from
/// stdin so the nested `comments[]` array survives). Returns the
/// posted review's `html_url`. `gh api` targets `api.github.com`,
/// sets the auth header from the logged-in token, and adds the API
/// version — no token handling on our side.
async fn gh_api_post_review(root: &Utf8Path, endpoint: &str, body_json: &str) -> MoonResult<String> {
	use tokio::io::AsyncWriteExt;

	let mut cmd = tokio::process::Command::new("gh");
	cmd
		.current_dir(root.as_std_path())
		.args([
			"api",
			"--method",
			"POST",
			"-H",
			"Accept: application/vnd.github+json",
			endpoint,
			"--input",
			"-",
		])
		.env("GH_PROMPT_DISABLED", "1")
		.env("LC_ALL", "C")
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped());
	let mut child = match cmd.spawn() {
		Ok(c) => c,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
			return Err(MoonError::invalid("gh is not installed"));
		}
		Err(err) => return Err(MoonError::IoError(format!("gh api spawn: {err}"))),
	};
	if let Some(mut stdin) = child.stdin.take() {
		stdin
			.write_all(body_json.as_bytes())
			.await
			.map_err(|e| MoonError::IoError(format!("gh api stdin: {e}")))?;
		// Drop stdin so gh sees EOF.
		drop(stdin);
	}
	let output = match tokio::time::timeout(GH_PUBLISH_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(err)) => return Err(MoonError::IoError(format!("gh api: {err}"))),
		Err(_) => return Err(MoonError::IoError("gh api timed out".to_owned())),
	};
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		return Err(MoonError::IoError(format!("gh api review POST failed: {stderr}")));
	}
	let value: serde_json::Value =
		serde_json::from_slice(&output.stdout).map_err(|e| MoonError::Internal(format!("gh api review JSON: {e}")))?;
	Ok(
		value
			.get("html_url")
			.and_then(serde_json::Value::as_str)
			.unwrap_or_default()
			.to_owned(),
	)
}

/// Cap on local-branch rows. Bumps when a real project hits it; 20
/// is "Cmd+Shift+B for the last few branches I touched" territory.
const BRANCH_LIST_LOCAL_CAP: usize = 20;
/// Cap on `gh pr list` rows. The team's repos run well under this
/// today; if a noisy repo lands, type-to-filter handles the volume
/// before we bump the cap.
const BRANCH_LIST_PR_CAP: usize = 30;
/// `gh pr list` timeout. Matches `run_git_fetch_quiet`'s 30s ceiling
/// — same "we'd rather fail than freeze the UI" trade-off; 30s is
/// well past the worst observed GitHub API round-trip.
const GH_PR_LIST_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// Top-level entry point for `WorkspaceHost::branch_list`. Runs the
/// git half on the blocking pool (cheap, sync `Command::output`)
/// and the gh half on the async runtime (single-shot
/// `tokio::process::Command` with a timeout) in parallel via
/// `tokio::join!`. Either half failing collapses to an empty
/// section + a `PrListStatus` (for the gh side) — the call
/// itself never errors out today, so the trait could return
/// `BranchList` directly, but we keep `MoonResult` for symmetry
/// with the other host methods and to leave room for a future
/// hard-error path (e.g. "active folder doesn't exist").
async fn run_branch_list(root: &Utf8Path, pr_scope: PrListScope) -> MoonResult<BranchList> {
	let local_root = root.to_owned();
	let local_fut = tokio::task::spawn_blocking(move || run_branch_list_local(&local_root));
	let prs_fut = run_branch_list_prs(root, pr_scope);
	let (local_join, (prs, pr_status)) = tokio::join!(local_fut, prs_fut);
	let local = local_join.unwrap_or_else(|err| {
		tracing::warn!(%err, "branch_list: local section join error");
		Vec::new()
	});
	Ok(BranchList { local, prs, pr_status })
}

/// `git for-each-ref refs/heads --sort=-committerdate` →
/// [`BranchListEntry::Local`] rows. NUL-separated fields (`%00` in
/// the format string) so a tab- or space-containing commit subject
/// doesn't corrupt the parse — subjects regularly contain
/// whitespace and the occasional control character. The default
/// branch is force-included past the recency cap (see the body).
fn run_branch_list_local(root: &Utf8Path) -> Vec<BranchListEntry> {
	use std::process::Command;

	let current = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["symbolic-ref", "--quiet", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned());

	let output = match Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args([
			"for-each-ref",
			"--sort=-committerdate",
			&format!("--count={BRANCH_LIST_LOCAL_CAP}"),
			"--format=%(refname:short)%00%(committerdate:relative)%00%(subject)",
			"refs/heads",
		])
		.output()
	{
		Ok(o) if o.status.success() => o,
		Ok(_) | Err(_) => return Vec::new(),
	};

	let default_name = resolve_default_local_branch(root);

	let stdout = String::from_utf8_lossy(&output.stdout);
	let mut rows = Vec::new();
	for line in stdout.lines() {
		let mut parts = line.splitn(3, '\0');
		let Some(name) = parts.next() else { continue };
		let Some(date) = parts.next() else { continue };
		let subject = parts.next().unwrap_or("");
		if name.is_empty() {
			continue;
		}
		let is_current = current.as_deref() == Some(name);
		let is_default = default_name.as_deref() == Some(name);
		rows.push(BranchListEntry::Local {
			name: name.to_owned(),
			last_commit_subject: subject.to_owned(),
			committer_date_relative: date.to_owned(),
			is_current,
			is_default,
		});
	}

	// The `--count` cap is sorted by recency, so on a repo with
	// more than `BRANCH_LIST_LOCAL_CAP` branches the default branch
	// (`main` / `master`) can fall off the end if the user hasn't
	// touched it lately. Switching back to it is the single most
	// common destination, so always surface it: if a local branch
	// matching the resolved default exists and didn't make the cut,
	// fetch its row and append it.
	if let Some(default_name) = default_name {
		let already_listed = rows
			.iter()
			.any(|row| matches!(row, BranchListEntry::Local { name, .. } if *name == default_name));
		if !already_listed {
			if let Some(entry) = run_branch_list_local_one(root, &default_name, current.as_deref()) {
				rows.push(entry);
			}
		}
	}

	rows
}

/// Short name of the local branch that tracks (or is) the repo's
/// default branch — `main` / `master` for the common case. Derived
/// from [`resolve_default_remote_ref`] by stripping the leading
/// `origin/`, then confirming a matching local `refs/heads/<name>`
/// actually exists (a fresh clone might have the remote-tracking
/// ref without the local branch). Returns `None` when no default
/// can be resolved or the local branch is absent.
fn resolve_default_local_branch(root: &Utf8Path) -> Option<String> {
	use std::process::Command;

	let remote_ref = resolve_default_remote_ref(root)?;
	let name = remote_ref.strip_prefix("origin/").unwrap_or(&remote_ref);
	if name.is_empty() {
		return None;
	}
	let exists = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")])
		.output()
		.ok()
		.is_some_and(|o| o.status.success());
	exists.then(|| name.to_owned())
}

/// `git for-each-ref` for a single local branch — used to fetch the
/// default branch's row when it fell outside the recency-capped
/// listing. Mirrors the field layout of [`run_branch_list_local`].
fn run_branch_list_local_one(root: &Utf8Path, name: &str, current: Option<&str>) -> Option<BranchListEntry> {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args([
			"for-each-ref",
			"--format=%(committerdate:relative)%00%(subject)",
			&format!("refs/heads/{name}"),
		])
		.output()
		.ok()
		.filter(|o| o.status.success())?;
	let stdout = String::from_utf8_lossy(&output.stdout);
	let line = stdout.lines().next()?;
	let mut parts = line.splitn(2, '\0');
	let date = parts.next().unwrap_or("");
	let subject = parts.next().unwrap_or("");
	Some(BranchListEntry::Local {
		name: name.to_owned(),
		last_commit_subject: subject.to_owned(),
		committer_date_relative: date.to_owned(),
		is_current: current == Some(name),
		// Only ever called for the resolved default branch.
		is_default: true,
	})
}

/// PR section: probe the active folder's remote for GitHub-ness,
/// then `gh pr list --json … --limit <cap>`. Returns the rows
/// plus a [`PrListStatus`] so the frontend renders the right
/// empty-state row when the section is empty.
async fn run_branch_list_prs(root: &Utf8Path, scope: PrListScope) -> (Vec<BranchListEntry>, PrListStatus) {
	if remote_web_url(root).is_none() {
		return (Vec::new(), PrListStatus::NotGithub);
	}
	match scope {
		PrListScope::All => {
			// Single canonical query: every open PR in the repo.
			// `gh pr list --state open` orders by createdAt desc;
			// we want updatedAt desc instead, so we resort on the
			// way out using the timestamp the parser already
			// extracted. (gh has no `--sort` flag for `pr list`,
			// and `--search` would override `--state` so it's not
			// any cheaper to push the sort server-side.)
			let (mut rows, status) = run_gh_pr_list_query(root, None).await;
			rows.sort_by_key(|row| std::cmp::Reverse(row.1));
			let dropped = rows.into_iter().map(|(entry, _)| entry).collect();
			(dropped, status)
		}
		PrListScope::Participating => {
			// Two queries in parallel — `involves:@me` covers
			// author / assignee / mentioned / commenter, but
			// **not** review-requested (that's its own qualifier).
			// Merge by PR number, sort by raw updatedAt desc.
			//
			// We use `sort:updated-desc` in the search query so
			// each side already lands ordered, but resort after
			// merging for the same reason the `All` branch does:
			// the merge can interleave freshly-replied review
			// requests with older `involves:` rows, and only a
			// post-merge sort gives the user the chronological
			// "what moved last" view.
			let involves = run_gh_pr_list_query(root, Some("state:open involves:@me sort:updated-desc"));
			let review = run_gh_pr_list_query(root, Some("state:open review-requested:@me sort:updated-desc"));
			let ((involves_rows, involves_status), (review_rows, review_status)) = tokio::join!(involves, review);
			// Status reconciliation: if both calls landed on the
			// same hard error (`GhMissing` / `GhNotAuthed` /
			// `NotGithub`) report it; if one succeeded and the
			// other transient-failed we still return the
			// successful slice with `Ok` so the user sees
			// something rather than a blank failure.
			let status = match (&involves_status, &review_status) {
				(PrListStatus::Ok, _) | (_, PrListStatus::Ok) => PrListStatus::Ok,
				(a, b) if a == b => a.clone(),
				_ => involves_status,
			};
			let mut by_number: std::collections::HashMap<u32, (BranchListEntry, Option<i64>)> =
				std::collections::HashMap::new();
			for (entry, ts) in involves_rows.into_iter().chain(review_rows) {
				let BranchListEntry::Pr { number, .. } = entry else {
					continue;
				};
				by_number.entry(number).or_insert((entry, ts));
			}
			let mut rows: Vec<(BranchListEntry, Option<i64>)> = by_number.into_values().collect();
			// Sort by raw updatedAt timestamp desc so the merged
			// list reads the same way the unfiltered list does.
			// `None` (unparseable timestamp) sinks to the bottom.
			rows.sort_by_key(|row| std::cmp::Reverse(row.1));
			rows.truncate(BRANCH_LIST_PR_CAP);
			let dropped = rows.into_iter().map(|(entry, _)| entry).collect();
			(dropped, status)
		}
	}
}

/// One `gh pr list --json …` invocation. `search` is forwarded as
/// `--search "<q>"` when present (and replaces the default
/// `--state open` slice — gh's search qualifier handles state
/// itself); when absent the call falls back to `--state open`.
///
/// Returns `(rows, status)` so the caller can decide how to merge
/// multiple queries. Each row carries the parsed unix-second
/// timestamp alongside the [`BranchListEntry::Pr`] so `Participating`
/// can sort the merged set chronologically before dropping the
/// timestamp on the way out.
async fn run_gh_pr_list_query(
	root: &Utf8Path,
	search: Option<&str>,
) -> (Vec<(BranchListEntry, Option<i64>)>, PrListStatus) {
	let mut cmd = tokio::process::Command::new("gh");
	cmd.current_dir(root.as_std_path()).args([
		"pr",
		"list",
		"--limit",
		&BRANCH_LIST_PR_CAP.to_string(),
		"--json",
		"number,title,headRefName,isDraft,updatedAt,author",
	]);
	match search {
		Some(query) => {
			cmd.args(["--search", query]);
		}
		None => {
			cmd.args(["--state", "open"]);
		}
	}
	cmd
		.env("GH_PROMPT_DISABLED", "1")
		.env("LC_ALL", "C")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped());

	let child = match cmd.spawn() {
		Ok(c) => c,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
			return (Vec::new(), PrListStatus::GhMissing);
		}
		Err(err) => {
			tracing::debug!(%err, "branch_list: gh spawn failed");
			return (
				Vec::new(),
				PrListStatus::Failed {
					detail: err.to_string(),
				},
			);
		}
	};

	let output = match tokio::time::timeout(GH_PR_LIST_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(err)) => {
			return (
				Vec::new(),
				PrListStatus::Failed {
					detail: format!("gh pr list: {err}"),
				},
			);
		}
		Err(_) => {
			return (
				Vec::new(),
				PrListStatus::Failed {
					detail: format!("gh pr list: timed out after {}s", GH_PR_LIST_TIMEOUT.as_secs()),
				},
			);
		}
	};

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		let lower = stderr.to_ascii_lowercase();
		if lower.contains("not logged into") || lower.contains("gh auth login") || lower.contains("authentication") {
			return (Vec::new(), PrListStatus::GhNotAuthed);
		}
		return (Vec::new(), PrListStatus::Failed { detail: stderr });
	}

	let now = SystemTime::now();
	let rows = parse_gh_pr_list(&output.stdout, now);
	(rows, PrListStatus::Ok)
}

/// Parse `gh pr list --json …` output into [`BranchListEntry::Pr`]
/// rows paired with their raw updatedAt unix timestamp. The
/// timestamp is `None` when gh emits something we can't parse —
/// callers that don't merge can drop it; merging callers
/// (`Participating`) sort by it before dropping. Broken out so a
/// unit test can feed canned JSON without spawning gh. Skips rows
/// missing required fields rather than erroring — gh's schema is
/// stable but a future field rename shouldn't take the whole
/// palette down.
fn parse_gh_pr_list(stdout: &[u8], now: SystemTime) -> Vec<(BranchListEntry, Option<i64>)> {
	let value: serde_json::Value = match serde_json::from_slice(stdout) {
		Ok(v) => v,
		Err(err) => {
			tracing::warn!(%err, "branch_list: gh JSON parse failed");
			return Vec::new();
		}
	};
	let Some(arr) = value.as_array() else {
		return Vec::new();
	};
	let mut rows = Vec::with_capacity(arr.len());
	for item in arr {
		let Some(number) = item.get("number").and_then(|n| n.as_u64()) else {
			continue;
		};
		let Some(title) = item.get("title").and_then(|t| t.as_str()) else {
			continue;
		};
		let Some(head_ref) = item.get("headRefName").and_then(|h| h.as_str()) else {
			continue;
		};
		let is_draft = item.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false);
		let updated_at = item.get("updatedAt").and_then(|u| u.as_str()).unwrap_or("");
		let author = item
			.get("author")
			.and_then(|a| a.get("login"))
			.and_then(|l| l.as_str())
			.unwrap_or("");
		let updated_at_unix = parse_iso8601_utc(updated_at);
		let updated_at_relative = format_iso8601_relative(updated_at, now).unwrap_or_default();
		let entry = BranchListEntry::Pr {
			number: number.min(u32::MAX as u64) as u32,
			title: title.to_owned(),
			author: author.to_owned(),
			head_ref: head_ref.to_owned(),
			is_draft,
			updated_at_relative,
		};
		rows.push((entry, updated_at_unix));
	}
	rows
}

/// `gh pr list --head <branch> --state open --json url --limit 1` —
/// the single open PR (if any) whose head ref matches the active
/// folder's current branch. Returns `None` for every failure case
/// the SCM panel needs to fall back from: not on a branch,
/// non-GitHub remote, `gh` missing or unauthenticated, non-zero
/// exit, timeout, parse error, no matching PR.
///
/// The bound is `--limit 1` because the SCM panel button only
/// needs one URL to navigate to. GitHub doesn't allow two open PRs
/// from the same head ref against the same base anyway; in the
/// rare cross-base case we pick whichever `gh` returns first
/// (newest by createdAt desc per `gh pr list`'s default sort).
async fn run_git_existing_pr_url(root: &Utf8Path) -> Option<String> {
	// Short-circuit on the cheap local checks. Same gates as the
	// SCM panel's `prUrl` derived value, applied server-side so we
	// don't spawn `gh` for branches we'd never link to anyway.
	remote_web_url(root)?;
	let branch = std::process::Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["symbolic-ref", "--quiet", "--short", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.and_then(|o| String::from_utf8(o.stdout).ok())
		.map(|s| s.trim().to_owned())
		.filter(|s| !s.is_empty())?;

	let mut cmd = tokio::process::Command::new("gh");
	cmd
		.current_dir(root.as_std_path())
		.args([
			"pr", "list", "--head", &branch, "--state", "open", "--json", "url", "--limit", "1",
		])
		.env("GH_PROMPT_DISABLED", "1")
		.env("LC_ALL", "C")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped());

	let child = match cmd.spawn() {
		Ok(c) => c,
		Err(err) => {
			tracing::debug!(%err, "git_existing_pr_url: gh spawn failed");
			return None;
		}
	};

	let output = match tokio::time::timeout(GH_PR_LIST_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(err)) => {
			tracing::debug!(%err, "git_existing_pr_url: gh wait failed");
			return None;
		}
		Err(_) => {
			tracing::debug!(
				timeout = GH_PR_LIST_TIMEOUT.as_secs(),
				"git_existing_pr_url: gh timed out"
			);
			return None;
		}
	};

	if !output.status.success() {
		tracing::debug!(
			stderr = %String::from_utf8_lossy(&output.stderr).trim(),
			"git_existing_pr_url: gh exited non-zero"
		);
		return None;
	}

	parse_gh_pr_url(&output.stdout)
}

/// Pull the first `url` string out of `gh pr list --json url`'s
/// JSON array. Broken out for unit-testing without spawning `gh`.
/// Returns `None` on parse failure, empty array, or a missing /
/// non-string `url` field.
fn parse_gh_pr_url(stdout: &[u8]) -> Option<String> {
	let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
	let arr = value.as_array()?;
	let first = arr.first()?;
	let url = first.get("url")?.as_str()?;
	if url.is_empty() {
		return None;
	}
	Some(url.to_owned())
}

/// Parse a UTC ISO 8601 timestamp (`YYYY-MM-DDTHH:MM:SSZ`, what
/// `gh` emits) and format the duration since `now` as a
/// human-readable relative string ("3 hours ago", "yesterday",
/// "2 weeks ago", …). Returns `None` for unparseable input or
/// future timestamps.
///
/// Hand-rolled rather than pulling in a date crate: gh's format
/// is fixed, and the rounding thresholds are coarse enough that
/// timezone / leap-second precision doesn't matter.
fn format_iso8601_relative(iso: &str, now: SystemTime) -> Option<String> {
	let then = parse_iso8601_utc(iso)?;
	let now_secs = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
	let diff = now_secs.saturating_sub(then);
	if diff < 0 {
		return None;
	}
	const MIN: i64 = 60;
	const HOUR: i64 = 60 * MIN;
	const DAY: i64 = 24 * HOUR;
	const WEEK: i64 = 7 * DAY;
	const MONTH: i64 = 30 * DAY;
	const YEAR: i64 = 365 * DAY;
	let formatted = match diff {
		d if d < MIN => "just now".to_owned(),
		d if d < 2 * MIN => "1 minute ago".to_owned(),
		d if d < HOUR => format!("{} minutes ago", d / MIN),
		d if d < 2 * HOUR => "1 hour ago".to_owned(),
		d if d < DAY => format!("{} hours ago", d / HOUR),
		d if d < 2 * DAY => "yesterday".to_owned(),
		d if d < WEEK => format!("{} days ago", d / DAY),
		d if d < 2 * WEEK => "1 week ago".to_owned(),
		d if d < MONTH => format!("{} weeks ago", d / WEEK),
		d if d < 2 * MONTH => "1 month ago".to_owned(),
		d if d < YEAR => format!("{} months ago", d / MONTH),
		d if d < 2 * YEAR => "1 year ago".to_owned(),
		d => format!("{} years ago", d / YEAR),
	};
	Some(formatted)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` into Unix seconds. We accept the
/// trailing `Z` (UTC) as gh always emits it, and a fractional-
/// seconds `.` segment which gh sometimes emits — anything else
/// is rejected. No timezone offsets, no locale parsing.
fn parse_iso8601_utc(iso: &str) -> Option<i64> {
	let bytes = iso.as_bytes();
	if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
		return None;
	}
	if bytes[13] != b':' || bytes[16] != b':' {
		return None;
	}
	let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
	let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
	let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
	let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
	let min: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
	let sec: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;
	if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
		return None;
	}
	// Days from 1970-01-01 to year-month-day, treating gh's UTC
	// dates as proleptic Gregorian. Algorithm: count leap years up
	// to year-1, then add days-of-year up to month-1, then add day
	// (1-based). Plenty of room (i64) for any date gh would emit.
	let days = days_from_civil(year, month, day);
	let secs = days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
	Some(secs)
}

/// Howard Hinnant's "days_from_civil" — proleptic Gregorian
/// year-month-day → days since 1970-01-01. Public-domain
/// algorithm, tiny and verified against `chrono` for every
/// realistic year. We embed it here rather than pulling in
/// `chrono` for the one ISO timestamp gh emits.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
	let y = if m <= 2 { y - 1 } else { y };
	let era = (if y >= 0 { y } else { y - 399 }) / 400;
	let yoe = (y - era * 400) as u32;
	let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
	let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
	era * 146_097 + doe as i64 - 719_468
}

/// `git switch <name>` / `gh pr checkout <number>` dispatcher.
/// Surfaces stderr verbatim on non-zero exit so the user sees
/// git / gh's actionable hint without us re-wrapping it.
fn run_branch_switch(root: &Utf8Path, target: &BranchSwitchTarget) -> MoonResult<()> {
	use std::process::Command;

	let mut cmd = Command::new(match target {
		BranchSwitchTarget::Local { .. } => "git",
		// `gh pr checkout` resolves the PR's head ref via the
		// GitHub API (so it works for fork PRs too) and runs
		// the equivalent `git fetch` + `git switch` against the
		// active folder. The repo is inferred from `git remote`
		// in the cwd — gh has no `-C <dir>` flag, so the
		// dispatcher below uses `current_dir()` for the gh
		// branch.
		BranchSwitchTarget::Pr { .. } => "gh",
	});
	let label = match target {
		BranchSwitchTarget::Local { name } => {
			let trimmed = name.trim();
			if trimmed.is_empty() {
				return Err(MoonError::invalid("branch name is empty"));
			}
			cmd.arg("-C").arg(root.as_std_path()).args(["switch", trimmed]);
			format!("git switch {trimmed}")
		}
		BranchSwitchTarget::Pr { number } => {
			cmd
				.current_dir(root.as_std_path())
				.args(["pr", "checkout", &number.to_string()]);
			format!("gh pr checkout {number}")
		}
	};

	let output = cmd
		.env("GIT_TERMINAL_PROMPT", "0")
		.env("GH_PROMPT_DISABLED", "1")
		.env("LC_ALL", "C")
		.output()
		.map_err(|e| {
			if e.kind() == std::io::ErrorKind::NotFound {
				MoonError::IoError(format!("{label}: command not found on PATH"))
			} else {
				MoonError::IoError(format!("{label} failed to launch: {e}"))
			}
		})?;
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
		let detail = match (stderr.is_empty(), stdout.is_empty()) {
			(false, _) => stderr,
			(true, false) => stdout,
			(true, true) => format!("exit {}", output.status.code().unwrap_or(-1)),
		};
		return Err(MoonError::IoError(format!("{label}: {detail}")));
	}
	Ok(())
}

/// `git restore --source=HEAD --staged --worktree -- <paths>`.
///
/// Pulling both the index entry and the worktree back to `HEAD` in
/// one call is the safe discard semantics: a modified file is reset
/// to its committed content, a deleted file reappears, and a
/// staged-modification is unstaged-and-reverted in the same pass.
///
/// Invoked from the blocking pool; everything inside is synchronous.
fn run_git_restore(root: &Utf8Path, paths: &[Utf8PathBuf]) -> MoonResult<()> {
	use std::process::Command;

	let mut cmd = Command::new("git");
	cmd
		.arg("-C")
		.arg(root.as_std_path())
		.args(["restore", "--source=HEAD", "--staged", "--worktree", "--"]);
	for p in paths {
		cmd.arg(p.as_std_path());
	}
	let output = cmd
		.output()
		.map_err(|e| MoonError::IoError(format!("git restore failed to launch: {e}")))?;
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
		return Err(MoonError::IoError(format!(
			"git restore exited {}: {stderr}",
			output.status.code().unwrap_or(-1)
		)));
	}
	Ok(())
}

/// `git blame --porcelain --root -w -- <path>`. Returns `Ok(None)`
/// for any failure mode the UI should silently ignore: not a repo,
/// path not tracked, git binary missing, file is binary. Real errors
/// (unparseable output, join errors) still bubble up.
///
/// Flag choices:
/// - `--porcelain` gives the stable one-commit-per-line format with
///   full metadata on the first appearance and an abbreviated header
///   on repeats. Cheap to parse and version-locked.
/// - `--root` makes lines from the root commit look like any other
///   commit (instead of being omitted). Otherwise the first commit's
///   lines would get a blank blame, which looks like a bug.
/// - `-w` ignores whitespace-only changes when attributing lines —
///   users reformat their own code all the time and get annoyed when
///   the blame gets reset to "you, just now" by an indent tweak.
/// - `--no-renames` would hide file-level renames from the blame walk;
///   we specifically *want* renames followed so blame traces through
///   them. Leave the git default on.
///
/// Invoked from the blocking pool.
fn run_git_blame(root: &Utf8Path, path: &Utf8PathBuf) -> MoonResult<Option<GitFileBlame>> {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["blame", "--porcelain", "--root", "-w", "--"])
		.arg(path.as_std_path())
		.output();
	let Ok(output) = output else {
		// `git` binary not on PATH. Same outcome the caller wants:
		// no blame, no toast, no noise.
		return Ok(None);
	};
	if !output.status.success() {
		// Non-repo, untracked file, etc. all exit non-zero. The UI
		// stays silent by contract (no toast), but we log stderr at
		// debug so a developer chasing "why is blame missing for
		// this one file?" has a breadcrumb without recompiling.
		let stderr = String::from_utf8_lossy(&output.stderr);
		tracing::debug!(
			path = %path,
			code = output.status.code().unwrap_or(-1),
			stderr = %stderr.trim(),
			"git blame exited non-zero"
		);
		return Ok(None);
	}
	let mut blame = parse_blame_porcelain(&output.stdout, path.as_str().to_owned());
	blame.remote_url = remote_web_url(root).unwrap_or_default();
	// Sanity-log the parse outcome. If every line has `sha=""` the
	// parser fell off the porcelain happy path — useful to know when
	// the UI shows "no blame" despite a successful exit.
	let filled = blame.lines.iter().filter(|l| !l.sha.is_empty()).count();
	tracing::debug!(
		path = %path,
		lines = blame.lines.len(),
		filled,
		stdout_bytes = output.stdout.len(),
		"git blame parsed"
	);
	Ok(Some(blame))
}

/// Validate a rev string that's about to be passed to `git
/// show <rev>:<path>`. We accept exactly two shapes — the
/// literal `"HEAD"` (compare baseline = `Head`) and a 40-char
/// hex SHA (compare baseline = `Default`, where the frontend
/// passes the merge-base it cached). Refusing anything else
/// keeps the surface narrow: the frontend never legitimately
/// hands us a flag-shaped or path-shaped rev, so any other
/// input is either a bug or an attempt to confuse the underlying
/// git invocation.
/// Shared path-containment envelope for git helpers that take a
/// workspace-relative path: reject absolute, rooted, and
/// `..`-escaping paths (an empty path is allowed through — callers
/// that can't act on it return `Ok(None)` themselves). `op` names
/// the calling method for the error message. Trailing slashes are
/// trimmed so a directory-style path normalises to its file form.
fn reject_uncontained_path(op: &str, path: &Utf8Path) -> MoonResult<Utf8PathBuf> {
	let rel = Utf8PathBuf::from(path.as_str().trim_end_matches('/'));
	if rel.is_absolute() {
		return Err(MoonError::invalid(format!("{op} rejects absolute path: {rel}")));
	}
	let mut depth = 0i32;
	for seg in rel.components() {
		match seg {
			camino::Utf8Component::ParentDir => {
				depth -= 1;
				if depth < 0 {
					return Err(MoonError::invalid(format!("{op} rejects path escape: {rel}")));
				}
			}
			camino::Utf8Component::Normal(_) => depth += 1,
			camino::Utf8Component::CurDir => {}
			camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir => {
				return Err(MoonError::invalid(format!("{op} rejects rooted path: {rel}")));
			}
		}
	}
	Ok(rel)
}

/// `git hash-object <path>` — the blob SHA of the file's *current*
/// on-disk bytes, regardless of index/HEAD state. This is what a
/// reviewed-file mark is pinned to (Phase 5.7). Returns `Ok(None)`
/// when the file is missing or `git` isn't on PATH; the caller
/// treats both as "leave the row unticked". Invoked from the
/// blocking pool.
fn run_git_blob_sha(root: &Utf8Path, path: &Utf8PathBuf) -> Option<String> {
	use std::process::Command;

	let abs = root.join(path);
	if !abs.as_std_path().is_file() {
		return None;
	}
	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.arg("hash-object")
		.arg("--")
		.arg(abs.as_std_path())
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	let sha = String::from_utf8(output.stdout).ok()?.trim().to_owned();
	// A blob SHA is 40 hex chars; anything else is git surprising us.
	if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
		Some(sha)
	} else {
		None
	}
}

fn is_safe_rev(rev: &str) -> bool {
	if rev == "HEAD" {
		return true;
	}
	rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `git show <rev>:<path>`. Returns `Ok(None)` for the "no diff to
/// show" states the UI treats silently: not a repo, path isn't in
/// the rev's tree (freshly added / untracked), or `git` itself is
/// missing. Binary contents at the rev collapse to `None` too —
/// the diff view only renders text. UTF-8 decode failures on
/// something we *thought* was text are the one real error path
/// and bubble up.
///
/// Invoked from the blocking pool. `rev` has already been
/// validated by [`is_safe_rev`].
fn run_git_ref_content(root: &Utf8Path, rev: &str, path: &Utf8PathBuf) -> MoonResult<Option<String>> {
	use std::process::Command;

	// `<rev>:<path>` uses forward slashes regardless of host OS —
	// git's pathspec grammar isn't the platform's. The path is
	// already workspace-relative + UTF-8 so the conversion is
	// lossless; Windows paths with backslashes would confuse git
	// silently otherwise.
	// `git show <rev>:<path>` is the stable way to pull a committed
	// blob by path. `--` isn't used here: `git show` treats args
	// after `--` as pathspecs rather than as rev-parse inputs, and
	// the blob would come back empty.
	let spec = format!("{}:{}", rev, path.as_str().replace('\\', "/"));
	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.arg("show")
		.arg(&spec)
		.output();
	let Ok(output) = output else {
		return Ok(None);
	};
	if !output.status.success() {
		// Two common shapes collapse to `None` here:
		// - "fatal: not a git repository" → outside a repo.
		// - "fatal: path 'foo' exists on disk, but not in '<rev>'"
		//   → untracked / newly-added. The diff for those is
		//   "everything is new", which the frontend renders by
		//   passing an empty "before" side itself; we don't need
		//   to fake a success here.
		let stderr = String::from_utf8_lossy(&output.stderr);
		tracing::debug!(
			path = %path,
			rev = %rev,
			code = output.status.code().unwrap_or(-1),
			stderr = %stderr.trim(),
			"git show <rev>:<path> exited non-zero"
		);
		return Ok(None);
	}
	if looks_binary(&output.stdout) {
		return Ok(None);
	}
	String::from_utf8(output.stdout)
		.map(Some)
		.map_err(|e| MoonError::IoError(format!("git show <rev>:<path> produced non-UTF-8 text: {e}")))
}

/// Resolve the merge-base with the default branch and emit the
/// file-level diff between the working tree (committed +
/// uncommitted) and that base. Returns `None` for the cases the
/// SCM panel silently downgrades to `Head` for — see the trait
/// method's doc for the full list.
///
/// Invoked from the blocking pool.
fn run_git_default_branch_diff(root: &Utf8Path) -> Option<BranchDiffStatus> {
	use std::process::Command;

	// Resolve the default-branch remote ref the same way the
	// existing `git_branch` does, so the toggle's enabled-state
	// in the SCM panel matches the data we'd actually return.
	let default_ref = resolve_default_remote_ref(root)?;

	// Bail early when HEAD is detached — there's no meaningful
	// "branch" to compare and the merge-base call would just
	// hand us HEAD itself.
	let head_branch = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["symbolic-ref", "--quiet", "--short", "HEAD"])
		.output()
		.ok()?;
	if !head_branch.status.success() {
		return None;
	}
	let head_name = String::from_utf8_lossy(&head_branch.stdout).trim().to_owned();
	if head_name.is_empty() {
		return None;
	}
	// On the default branch itself (e.g. `main`) the merge-base is
	// HEAD and the diff is empty — and the toggle would be
	// confusing rather than useful. Suppress.
	if default_ref.split_once('/').map(|(_, b)| b) == Some(head_name.as_str()) {
		return None;
	}

	let merge_base = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["merge-base", "HEAD", &default_ref])
		.output()
		.ok()?;
	if !merge_base.status.success() {
		return None;
	}
	let merge_base = String::from_utf8_lossy(&merge_base.stdout).trim().to_owned();
	if merge_base.is_empty() {
		return None;
	}

	// `git diff --name-status -z --no-renames <merge-base>`
	// compares the working tree (committed + uncommitted) against
	// `merge-base`. Untracked files are absent from `git diff`
	// against a tree-ish — that matches the user's "modified /
	// added / deleted vs main" mental model so we don't need to
	// merge in porcelain output here.
	//
	// `--no-renames` keeps the parser flat: a rename comes through
	// as `D <old>\0A <new>` instead of the two-path `R<NN>` record,
	// the same discipline the regular porcelain pipeline uses.
	let diff = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["diff", "--name-status", "-z", "--no-renames", &merge_base])
		.output()
		.ok()?;
	if !diff.status.success() {
		tracing::debug!(
			%merge_base,
			code = diff.status.code().unwrap_or(-1),
			stderr = %String::from_utf8_lossy(&diff.stderr).trim(),
			"git diff --name-status against merge-base failed"
		);
		return None;
	}
	let entries = parse_diff_name_status_z(&diff.stdout);
	Some(BranchDiffStatus {
		merge_base,
		default_branch_ref: default_ref,
		entries,
	})
}

/// `git diff --name-status -z` records are
/// `<status>\0<path>\0`-shaped (the `-z` flag swaps the regular
/// `<status>\t<path>\n` for NUL-separated fields *and* records).
/// Map the single status byte to the existing `GitFileStatus`
/// vocabulary; unknown bytes are dropped silently — we'd rather
/// skip a row than paint an arbitrary status.
fn parse_diff_name_status_z(buf: &[u8]) -> Vec<GitStatusEntry> {
	let mut out = Vec::new();
	let mut cursor = 0;
	while cursor < buf.len() {
		// Status field — one byte under `--no-renames`. With
		// renames enabled this would be `R<NN>` / `C<NN>` which
		// our caller doesn't ask for, but the loop is still safe:
		// it'd just hit the `_ => continue` arm and move on.
		let Some(status_end) = buf[cursor..].iter().position(|&b| b == 0) else {
			break;
		};
		if status_end == 0 {
			// Empty status field — malformed, bail.
			break;
		}
		let status_byte = buf[cursor];
		cursor += status_end + 1;
		let Some(path_end) = buf[cursor..].iter().position(|&b| b == 0) else {
			break;
		};
		let raw_path = &buf[cursor..cursor + path_end];
		cursor += path_end + 1;
		let Ok(path) = std::str::from_utf8(raw_path) else {
			continue;
		};
		let status = match status_byte {
			b'A' => GitFileStatus::Added,
			b'D' => GitFileStatus::Deleted,
			b'M' | b'T' => GitFileStatus::Modified,
			_ => continue,
		};
		out.push(GitStatusEntry {
			path: path.to_owned(),
			status,
		});
	}
	out
}

/// Read the configured upstream for a local branch: the
/// `branch.<name>.remote` and `branch.<name>.merge` pair. Returns
/// `None` when either key is missing (no upstream configured, e.g.
/// a freshly-created local branch not yet pushed).
///
/// `remote` is either a named remote (`"origin"`) or a git URL
/// (`gh pr checkout` on a fork PR points `branch.<name>.remote`
/// straight at the fork URL rather than adding a named remote).
/// `merge` is the full ref on the remote (`"refs/heads/master"`).
/// Both forms are valid inputs to `git push <remote> <local>:<merge>`,
/// which is what `git_push` uses to sidestep the `push.default = simple`
/// failure when the local branch name differs from the merge target
/// name (the typical fork-PR layout, where gh names the local branch
/// `<owner>/<head>` to avoid colliding with the default branch).
fn read_branch_upstream(root: &Utf8Path, branch: &str) -> Option<(String, String)> {
	use std::process::Command;

	let read = |key: &str| -> Option<String> {
		Command::new("git")
			.arg("-C")
			.arg(root.as_std_path())
			.args(["config", "--get", &format!("branch.{branch}.{key}")])
			.output()
			.ok()
			.filter(|o| o.status.success())
			.and_then(|o| String::from_utf8(o.stdout).ok())
			.map(|s| s.trim().to_owned())
			.filter(|s| !s.is_empty())
	};
	let remote = read("remote")?;
	let merge = read("merge")?;
	Some((remote, merge))
}

/// Resolve the primary remote's web URL, normalised for link-
/// building. Returns `None` when no remote is configured, the
/// configured remote uses an unrecognised host, or the command fails
/// for any other reason.
///
/// Preference order for which remote to pick:
///
/// 1. `origin` — the near-universal default set by `git clone`.
/// 2. `upstream` — the second-most-common convention on forks.
/// 3. First remote from `git remote` output — last-resort fallback.
///
/// Normalisation handles the three common URL shapes:
///
/// - `git@github.com:owner/repo.git` → `https://github.com/owner/repo`
/// - `https://github.com/owner/repo.git` → `https://github.com/owner/repo`
/// - `ssh://git@github.com/owner/repo` → `https://github.com/owner/repo`
///
/// Only `github.com` is recognised for now — GitLab, Bitbucket, and
/// self-hosted hosts get `None` until someone wires their PR-URL
/// convention. Matches the scope discipline in AGENTS.md: add the
/// other platforms when a user asks.
/// Build a GitHub permalink (URL + Markdown) for `rel` spanning
/// `start_line..=end_line`. Returns `None` when the remote isn't a
/// recognised host, the repo has no `HEAD` commit, or `git` is
/// missing — every "nothing to link to" case the editor menu
/// collapses to a disabled item.
fn run_git_permalink(root: &Utf8Path, rel: &Utf8Path, start_line: u32, end_line: u32) -> Option<GitPermalink> {
	use std::process::Command;

	let web = remote_web_url(root)?;
	let sha = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["rev-parse", "HEAD"])
		.output()
		.ok()
		.filter(|o| o.status.success())
		.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
		.filter(|s| !s.is_empty())?;

	// Normalise the line range: clamp to >= 1 and order start <= end
	// so a backwards drag still produces a sane fragment.
	let lo = start_line.max(1);
	let hi = end_line.max(lo);
	let fragment = if lo == hi {
		format!("L{lo}")
	} else {
		format!("L{lo}-L{hi}")
	};

	// Percent-encode the path so spaces / `#` / `?` in a filename
	// don't break the URL. `encode_branch_segment` already leaves the
	// `/` separators literal, which is exactly what we want for a
	// nested path.
	let encoded_path = encode_branch_segment(rel.as_str());

	let url = format!("{web}/blob/{sha}/{encoded_path}#{fragment}");
	// Link text uses the un-encoded relative path + line range: it's
	// display text in Markdown, so readability beats URL-safety.
	let markdown = format!("[{rel}#{fragment}]({url})");
	Some(GitPermalink { url, markdown })
}

fn remote_web_url(root: &Utf8Path) -> Option<String> {
	use std::process::Command;

	for candidate in ["origin", "upstream"] {
		let raw = Command::new("git")
			.arg("-C")
			.arg(root.as_std_path())
			.args(["config", "--get"])
			.arg(format!("remote.{candidate}.url"))
			.output()
			.ok()
			.filter(|o| o.status.success())
			.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
			.filter(|s| !s.is_empty());
		if let Some(url) = raw {
			if let Some(web) = normalize_remote_url(&url) {
				return Some(web);
			}
			// Remote exists but isn't a supported host; keep looking
			// — the repo may have a GitHub upstream behind a custom
			// origin.
		}
	}
	None
}

/// URL-normalising half of `remote_web_url`, broken out for unit
/// tests. Returns `None` for any URL we can't confidently map to a
/// web base.
fn normalize_remote_url(raw: &str) -> Option<String> {
	// `git@github.com:owner/repo(.git)?` — SCP-style SSH.
	if let Some(rest) = raw.strip_prefix("git@") {
		if let Some((host, path)) = rest.split_once(':') {
			if host == "github.com" {
				return Some(github_web_url(path));
			}
		}
	}
	// `ssh://git@github.com/owner/repo(.git)?`
	if let Some(rest) = raw.strip_prefix("ssh://") {
		let rest = rest.strip_prefix("git@").unwrap_or(rest);
		if let Some((host, path)) = rest.split_once('/') {
			if host == "github.com" {
				return Some(github_web_url(path));
			}
		}
	}
	// `https://github.com/owner/repo(.git)?` — already HTTPS.
	if let Some(rest) = raw.strip_prefix("https://").or_else(|| raw.strip_prefix("http://")) {
		if let Some((host, path)) = rest.split_once('/') {
			if host == "github.com" {
				return Some(github_web_url(path));
			}
		}
	}
	None
}

fn github_web_url(owner_repo: &str) -> String {
	// Strip any trailing slash and the conventional `.git` suffix
	// that both HTTPS and SSH shapes carry.
	let trimmed = owner_repo.trim_end_matches('/').trim_end_matches(".git");
	format!("https://github.com/{trimmed}")
}

/// Parse `git blame --porcelain` output into line-indexed records.
///
/// The format, stripped to what we consume:
///
/// ```text
/// <40-char-sha> <orig-line> <final-line> [<group-lines>]
/// author Some Name
/// author-mail <email@example.com>
/// author-time 1712345678
/// …
/// summary subject of the commit
/// …
/// \tthe actual line of source
/// ```
///
/// The first occurrence of a commit carries the full header; later
/// lines from the same commit skip it (just the header line + `\t<src>`).
/// We cache per-sha metadata so each emitted `GitLineBlame` carries
/// the full set.
///
/// `message` is the commit's full subject+body. `git blame --porcelain`
/// only gives us `summary` (the first line); to get the full message
/// we'd need a second call per unique sha (`git show --no-patch
/// --format=%B <sha>`). That's expensive in the hover path but cheap
/// in the blame path if we batch after parsing — but right now we
/// punt and set `message = summary`. The hover tooltip still wins
/// (author + date + subject + hash is already a big step up from a
/// plain "edited 5 minutes ago" badge); a future pass can upgrade to
/// full messages once we decide how to batch the lookup.
fn parse_blame_porcelain(stdout: &[u8], path: String) -> GitFileBlame {
	use std::collections::HashMap;

	let text = String::from_utf8_lossy(stdout);
	let mut commits: HashMap<String, CommitMeta> = HashMap::new();
	let mut lines: Vec<Option<GitLineBlame>> = Vec::new();

	// `current_sha`: the commit the next `\t<src>` line belongs to.
	// We set it every time we see a header line and read it back
	// when the `\t` line arrives.
	let mut current_sha: Option<String> = None;
	let mut final_line: usize = 0;
	// Mutable draft for the commit we're currently accumulating
	// header fields for. Dumped into `commits` on the `\t<src>` line.
	let mut draft = CommitMeta::default();

	for line in text.lines() {
		if let Some(src) = line.strip_prefix('\t') {
			// Source line: finalise this record. `src` itself is
			// unused (we don't re-display the file content), just a
			// delimiter that a block is closing out.
			let _ = src;
			let sha = current_sha.clone().unwrap_or_default();
			if !commits.contains_key(&sha) && !draft.author.is_empty() {
				commits.insert(sha.clone(), draft.clone());
			}
			let meta = commits.get(&sha).cloned().unwrap_or_default();
			let is_uncommitted = sha.chars().all(|c| c == '0');
			let entry = GitLineBlame {
				sha: sha.clone(),
				is_uncommitted,
				author: meta.author,
				author_email: meta.author_email,
				author_time: meta.author_time,
				summary: meta.summary.clone(),
				message: meta.summary,
			};
			// `final_line` is 1-indexed in the porcelain; we store
			// 0-indexed to match CM's line addressing. Grow the
			// vector with empty slots if a block skipped ahead
			// (shouldn't happen, but survive malformed input).
			let idx = final_line.saturating_sub(1);
			if lines.len() <= idx {
				lines.resize_with(idx + 1, || None);
			}
			lines[idx] = Some(entry);
			draft = CommitMeta::default();
			continue;
		}
		// Header / metadata lines. The sha-headed one starts every
		// block; the rest are key/value pairs.
		let mut parts = line.splitn(2, ' ');
		let Some(key) = parts.next() else {
			continue;
		};
		let rest = parts.next().unwrap_or("");
		if key.len() == 40 && key.chars().all(|c| c.is_ascii_hexdigit() || c == '0') {
			// Block-header line: `<sha> <orig> <final> [<group>]`.
			let mut header = rest.split_whitespace();
			let _orig = header.next();
			let fin = header.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
			final_line = fin;
			current_sha = Some(key.to_owned());
			// If we've seen this commit before, its header is
			// abbreviated: no `author` / `author-time` / etc. lines
			// follow, just the `\t<src>`. Prime `draft` from the
			// cached metadata so the finaliser sees the right fields.
			if let Some(cached) = commits.get(key) {
				draft = cached.clone();
			} else {
				draft = CommitMeta::default();
			}
			continue;
		}
		match key {
			"author" => draft.author = rest.to_owned(),
			"author-mail" => {
				draft.author_email = rest.trim_start_matches('<').trim_end_matches('>').to_owned();
			}
			"author-time" => {
				draft.author_time = rest.parse::<i64>().unwrap_or(0);
			}
			"summary" => draft.summary = rest.to_owned(),
			// Everything else (`committer`, `committer-time`,
			// `previous`, `filename`, `boundary`) is information we
			// don't surface in the UI. Cheap to ignore; the format
			// is stable enough that we won't need them unless we
			// add a richer blame view later.
			_ => {}
		}
	}

	GitFileBlame {
		path,
		remote_url: String::new(),
		lines: lines.into_iter().map(|opt| opt.unwrap_or_default()).collect(),
	}
}

#[derive(Clone, Default)]
struct CommitMeta {
	author: String,
	author_email: String,
	author_time: i64,
	summary: String,
}

/// Depth-first recursion into the workspace root. Order mirrors the
/// previous frontend walker (dirs push-and-recurse immediately, files
/// push inline) so Pierre's tree diff stays stable across the
/// per-dir-IPC → single-call migration.
///
/// Errors are swallowed per-entry rather than bubbled up: a single
/// unreadable symlink or permission-denied directory shouldn't blow
/// up a whole refresh. The entries we _can_ read still make the cut.
fn walk_paths(
	root: &Utf8Path,
	rel: &str,
	out: &mut Vec<String>,
	depth_capped: &mut Vec<String>,
	depth: u32,
	max_depth: u32,
	skip_dirs: &std::collections::BTreeSet<String>,
) {
	let dir_path = if rel.is_empty() {
		root.as_std_path().to_path_buf()
	} else {
		root.as_std_path().join(rel)
	};
	let iter = match std::fs::read_dir(&dir_path) {
		Ok(i) => i,
		Err(_) => return,
	};
	for entry in iter.flatten() {
		let Ok(file_type) = entry.file_type() else {
			continue;
		};
		let name = entry.file_name().to_string_lossy().into_owned();
		let child_rel = if rel.is_empty() {
			name.clone()
		} else {
			format!("{rel}/{name}")
		};
		if file_type.is_dir() {
			// `.git/` hides on purpose; ignored-directory pruning
			// goes through the explicit skip set instead of
			// hard-coded names so a project that chooses to track
			// `node_modules/` (rare but legal) keeps its contents
			// visible.
			if name == ".git" {
				continue;
			}
			let dir_path_rel = format!("{child_rel}/");
			if skip_dirs.contains(&dir_path_rel) {
				// Emit the directory itself so the user still
				// sees it in the tree (and the git overlay can
				// tint it with the ignored colour), but don't
				// enumerate its descendants. For a repo whose
				// gitignore covers `node_modules/`, this saves
				// the path store from carrying tens of thousands
				// of entries the user has no way to reach.
				out.push(dir_path_rel);
				continue;
			}
			out.push(dir_path_rel.clone());
			if depth < max_depth {
				walk_paths(root, &child_rel, out, depth_capped, depth + 1, max_depth, skip_dirs);
			} else if dir_has_any_entry(&dir_path.join(&name)) {
				// Hit the depth cap with a directory that has
				// children we won't enumerate. Surface it as a
				// lazy frontier so the file tree can fetch its
				// contents on expansion. Empty leaf directories
				// are NOT marked — they don't need lazy loading.
				depth_capped.push(dir_path_rel);
			}
		} else if file_type.is_file() || file_type.is_symlink() {
			out.push(child_rel);
		}
	}
}

/// Cheap "does this directory contain anything visible to the
/// walker?" probe. Used by [`walk_paths`] at the depth cap so we
/// only mark a directory as lazy when there's actually something
/// for the lazy load to fetch — empty leaves stay non-lazy and
/// don't trigger an IPC roundtrip on expansion.
///
/// Skips `.git/` for the same reason the walker does. Returns
/// `false` on `read_dir` errors so we don't mark unreadable
/// directories as lazy (the lazy load would fail again anyway).
fn dir_has_any_entry(path: &std::path::Path) -> bool {
	let Ok(iter) = std::fs::read_dir(path) else {
		return false;
	};
	for entry in iter.flatten() {
		let name = entry.file_name();
		if name == ".git" {
			continue;
		}
		return true;
	}
	false
}

/// Set of repo-relative directory paths (each ending in `/`) that
/// `git status --ignored=matching` collapses to a single ignored
/// row. The walker treats these as "don't recurse" so Pierre
/// never sees their descendants. Returns an empty set for non-repo
/// folders, git failures, or repos with no ignored directories.
///
/// `--porcelain=v1 -z --ignored=matching`. We **don't** pass
/// `--untracked-files=no` — git refuses
/// `--ignored=matching --untracked-files=no` with `Combinaison non
/// supportée…` because untracked enumeration is the mechanism that
/// surfaces ignored entries in the first place. The default
/// (`--untracked-files=normal`) keeps untracked directories
/// collapsed to `dir/` records, which we filter out below.
fn collapsed_ignored_dirs(root: &Utf8Path) -> std::collections::BTreeSet<String> {
	use std::process::Command;

	let mut out = std::collections::BTreeSet::new();
	let Ok(output) = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["status", "--porcelain=v1", "-z", "--ignored=matching"])
		.output()
	else {
		return out;
	};
	if !output.status.success() {
		return out;
	}
	let mut cursor = 0;
	let buf = &output.stdout;
	while cursor < buf.len() {
		if buf.len() - cursor < 3 {
			break;
		}
		let x = buf[cursor];
		let y = buf[cursor + 1];
		cursor += 3;
		let path_start = cursor;
		while cursor < buf.len() && buf[cursor] != 0 {
			cursor += 1;
		}
		let raw = &buf[path_start..cursor];
		if cursor < buf.len() {
			cursor += 1;
		}
		// Only `!!` records (both X and Y are `!`) describe
		// ignored entries. Anything else (`R` renames double-
		// records for example) is irrelevant here.
		if x != b'!' || y != b'!' {
			continue;
		}
		let Ok(path) = std::str::from_utf8(raw) else {
			continue;
		};
		// `--ignored=matching` collapses an ignored directory to
		// `name/` (trailing slash). An ignored *file* comes
		// through without a trailing slash; we ignore those —
		// the walker would visit it anyway and pierre's git
		// overlay tints it from `git_status_entries`.
		if !path.ends_with('/') {
			continue;
		}
		out.insert(path.replace('\\', "/"));
	}
	out
}

/// Per-path git status for every interesting entry in the tree —
/// changed tracked files, untracked files, collapsed-ignored
/// directories, and deletions the frontend hasn't enumerated. See
/// the trait docs for what `paths` is for.
///
/// Primary strategy inside a git repo: `git status --porcelain=v1`.
/// That path respects the index, so a `.gitignore`-matching file
/// that's already tracked is reported as clean (not faded) — the
/// walker below can't make that distinction on its own because
/// `ignore::WalkBuilder` has no view of the index.
///
/// Fallback (no git repo / `git` binary missing): walk the tree
/// with the standard gitignore filters and flag anything the walker
/// doesn't yield as `Ignored`. Good enough for pre-`git init`
/// folders; loses the rest of the state machine (no add / modify /
/// delete / untracked), which is fine because without git those
/// concepts don't exist.
fn classify_git_status(target: &ShellTarget, root: &Utf8Path, paths: &[String]) -> MoonResult<Vec<GitStatusEntry>> {
	// Host-side first: fast, and correct for ordinary folders (their
	// `.git` is host-native through the bind mount). A worktree-backed
	// folder's `.git` points into the container (ADR 0028 W.4.1), so
	// host git fails on it — fall back to running git inside the
	// container, where the worktree's metadata resolves. Ordinary
	// folders never reach the fallback, so they keep the host fast path.
	if let Some(entries) = classify_via_git_status(&ShellTarget::Host, root) {
		return Ok(entries);
	}
	if matches!(target, ShellTarget::Container { .. }) {
		if let Some(entries) = classify_via_git_status(target, root) {
			return Ok(entries);
		}
	}
	classify_ignored_via_walker(root, paths)
}

/// Read `git status --porcelain=v1 -z --ignored=matching --untracked-files=all --no-renames`
/// and turn the XY status bytes into our flat enum.
///
/// Flags, one more time because they're load-bearing:
/// - `-z` uses `\0` as the record separator; no filenames get
///   mangled by spaces, tabs, or quoting.
/// - `--porcelain=v1` pins the format; the default porcelain is
///   defined to be stable forever, but pinning is free.
/// - `--ignored=matching` reports only entries that directly hit
///   an ignore rule — meaning an ignored *directory* comes through
///   as a single collapsed `target/` row. The default
///   (`=traditional`) collapses the same way on its own, but
///   _combined_ with `--untracked-files=all` it reverts to listing
///   every file inside an ignored dir (git's own docs spell this
///   out). `=matching` is the one mode that keeps ignored dirs
///   collapsed while still expanding untracked dirs.
/// - `--untracked-files=all` lists individual files inside new
///   untracked directories rather than collapsing them to `foo/` —
///   users expect every new file to appear in the tree.
/// - `--no-renames` has git report renames as an atomic
///   `delete(old) + add(new)`. Matches the tree's rendering
///   contract and sidesteps the two-path `RN` porcelain record
///   that'd otherwise complicate parsing.
///
/// Returns `None` if git fails to start or exits non-zero — the
/// "not a git repository" complaint is expected on pre-init folders
/// and triggers the walker fallback. Stderr is swallowed on purpose.
fn classify_via_git_status(target: &ShellTarget, root: &Utf8Path) -> Option<Vec<GitStatusEntry>> {
	let output = git_command(target, root)
		.args([
			"status",
			"--porcelain=v1",
			"-z",
			"--ignored=matching",
			"--untracked-files=all",
			"--no-renames",
		])
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	Some(parse_porcelain_v1(&output.stdout))
}

/// Splits a `-z` porcelain v1 buffer into `GitStatusEntry`s.
///
/// Each record is `XY<space>path\0` where `X` is the index status
/// and `Y` the worktree status. We map that to a single label with
/// this priority — **conflict codes dominate** so an unmerged row
/// can't get masked by the file's other (modified / added) face:
///
/// 1. Unmerged combinations (`UU` / `AU` / `UA` / `DD` / `AA` /
///    `UD` / `DU`) → `Conflicted`. Catches both content conflicts
///    (`UU`) and the rarer add/add, modify/delete, delete/modify,
///    and both-deleted variants. The SCM panel shifts into
///    merge-in-progress mode based on `.git/MERGE_HEAD`, but
///    `git ls-files --unmerged` agrees with these codes — the
///    porcelain status is the canonical per-row signal.
/// 2. Either column is `D` → `Deleted` (dominates so stage-then-
///    revert doesn't mask the on-disk state).
/// 3. Either column is `A` → `Added` (staged-for-commit new file).
/// 4. Either column is `M` / `T` → `Modified`.
/// 5. `??` → `Untracked`.
/// 6. `!!` → `Ignored`.
///
/// Anything else (`C` copies we didn't disable, or any future code
/// git introduces) is silently dropped — we'd rather skip a row
/// than paint an arbitrary status on it.
fn parse_porcelain_v1(buf: &[u8]) -> Vec<GitStatusEntry> {
	let mut out = Vec::new();
	// Porcelain records are `\0`-terminated but we can't just
	// `split(b'\0')` because the `R` rename record emits _two_
	// zero-separated paths; a scan keeps the grammar local in case
	// we ever drop `--no-renames`.
	let mut cursor = 0;
	while cursor < buf.len() {
		// Need at least `XY<space>` before a path can start.
		if buf.len() - cursor < 3 {
			break;
		}
		let x = buf[cursor] as char;
		let y = buf[cursor + 1] as char;
		cursor += 3;
		let path_start = cursor;
		while cursor < buf.len() && buf[cursor] != 0 {
			cursor += 1;
		}
		let raw = &buf[path_start..cursor];
		if cursor < buf.len() {
			cursor += 1;
		}
		let Ok(path) = std::str::from_utf8(raw) else {
			continue;
		};
		if path.is_empty() {
			continue;
		}
		let Some(status) = map_porcelain_status(x, y) else {
			continue;
		};
		// Git writes ignored dirs with a trailing `/`; every other
		// status refers to a file and doesn't. Don't massage the
		// path — the frontend's path convention already expects
		// this.
		out.push(GitStatusEntry {
			path: path.replace('\\', "/"),
			status,
		});
	}
	out
}

fn map_porcelain_status(x: char, y: char) -> Option<GitFileStatus> {
	// Unmerged codes from `git status --porcelain` (see
	// `gitstatus(5)` "Short Format" — the unmerged section):
	//   D D  unmerged, both deleted
	//   A U  unmerged, added by us
	//   U D  unmerged, deleted by them
	//   U A  unmerged, added by them
	//   D U  unmerged, deleted by us
	//   A A  unmerged, both added
	//   U U  unmerged, both modified
	// Match exhaustively rather than `x == 'U' || y == 'U'` so a
	// hypothetical future code with a `U` on one side and an
	// unrelated letter on the other doesn't silently get mapped
	// here.
	if matches!(
		(x, y),
		('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
	) {
		return Some(GitFileStatus::Conflicted);
	}
	if x == 'D' || y == 'D' {
		return Some(GitFileStatus::Deleted);
	}
	if x == 'A' || y == 'A' {
		return Some(GitFileStatus::Added);
	}
	if x == 'M' || y == 'M' || x == 'T' || y == 'T' {
		return Some(GitFileStatus::Modified);
	}
	if x == '?' && y == '?' {
		return Some(GitFileStatus::Untracked);
	}
	if x == '!' && y == '!' {
		return Some(GitFileStatus::Ignored);
	}
	None
}

/// Walker fallback for folders without a usable `.git/`. The walker
/// can only tell us "would git ignore this?" — it has no tracked /
/// modified / untracked axis — so every entry we tag comes out
/// `Ignored`. That's the honest answer: the other statuses require
/// an index, which doesn't exist in this codepath.
fn classify_ignored_via_walker(root: &Utf8Path, paths: &[String]) -> MoonResult<Vec<GitStatusEntry>> {
	use ignore::WalkBuilder;
	use std::collections::HashSet;

	let mut visible: HashSet<String> = HashSet::new();
	// `hidden(false)` keeps dotfiles like `.gitignore` itself in the
	// visible set; `git_ignore` / `git_exclude` / `git_global` turn
	// on the three ignore sources users expect. `ignore(true)` also
	// respects `.ignore` files, which is the ripgrep convention and
	// aligns with our own `search_files` command. `require_git(false)`
	// applies `.gitignore` even before `git init` — a folder with a
	// `.gitignore` at its root still expects those patterns to fade.
	let walker = WalkBuilder::new(root.as_std_path())
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.git_global(true)
		.require_git(false)
		.ignore(true)
		.parents(true)
		.build();
	for entry in walker.flatten() {
		let Ok(rel) = entry.path().strip_prefix(root.as_std_path()) else {
			continue;
		};
		let Some(rel_str) = rel.to_str() else {
			continue;
		};
		if rel_str.is_empty() {
			continue;
		}
		let is_dir = entry.file_type().map(|f| f.is_dir()).unwrap_or(false);
		let normalised = rel_str.replace('\\', "/");
		if is_dir {
			visible.insert(format!("{normalised}/"));
		} else {
			visible.insert(normalised);
		}
	}

	let mut out = Vec::new();
	for path in paths {
		let trimmed = path.trim_end_matches('/');
		if trimmed.is_empty() {
			continue;
		}
		if !visible.contains(path.as_str()) && !visible.contains(trimmed) {
			out.push(GitStatusEntry {
				path: path.clone(),
				status: GitFileStatus::Ignored,
			});
		}
	}
	Ok(out)
}

fn system_time_to_ms(t: SystemTime) -> Option<i64> {
	t.duration_since(SystemTime::UNIX_EPOCH)
		.ok()
		.map(|d| d.as_millis() as i64)
}

/// Cheap heuristic — null byte in the first 8 KB indicates binary.
fn looks_binary(bytes: &[u8]) -> bool {
	let head = &bytes[..bytes.len().min(8000)];
	head.contains(&0)
}

/// Read a file directly from the host filesystem, bypassing every
/// `WorkspaceHost` (and therefore every workspace-root check). Used by the
/// "Open File…" affordance to load files that live outside any bound folder
/// — and, in the Phase 2 container world, to reach files outside the bind
/// mount that the in-container host can't see at all. Same `ReadFileResult`
/// shape as [`WorkspaceHost::read_file`] (binary detection + mtime), so the
/// frontend handles the two paths interchangeably. The caller is responsible
/// for whatever boundary makes sense at the UI layer.
pub async fn read_host_file(path: &Utf8Path) -> MoonResult<ReadFileResult> {
	let bytes = tokio::fs::read(path.as_std_path()).await.map_err(MoonError::from)?;
	let metadata = tokio::fs::metadata(path.as_std_path()).await.map_err(MoonError::from)?;
	let mtime_ms = metadata.modified().ok().and_then(system_time_to_ms);

	if looks_binary(&bytes) {
		return Ok(ReadFileResult {
			text: String::new(),
			mtime_ms,
			is_binary: true,
		});
	}

	let text = String::from_utf8(bytes).map_err(|e| MoonError::IoError(e.to_string()))?;
	Ok(ReadFileResult {
		text,
		mtime_ms,
		is_binary: false,
	})
}

/// Process-wide dedup for lint-staged parse-time warnings: returns
/// `true` the first time a given warning string is seen, `false`
/// afterwards. Lets the caller emit each distinct warning exactly
/// once on the format-on-save panel even though the rules object
/// is re-matched on every save.
fn warn_lint_staged_config_once(warning: &str) -> bool {
	use std::collections::HashSet;
	use std::sync::{Mutex, OnceLock};
	static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
	let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
	let mut guard = seen.lock().expect("lint-staged config warn cache poisoned");
	guard.insert(warning.to_owned())
}

/// Write `text` straight to the host path. Counterpart to
/// [`read_host_file`] — bypasses the editorconfig + lint-staged save pipeline
/// because external files don't belong to any workspace root and there's no
/// `.editorconfig` cascade or lint-staged config to consult. Equivalent of
/// `tokio::fs::write` plus the `WriteFileResult` shape the frontend already
/// understands.
pub async fn write_host_file(path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult> {
	let bytes = text.as_bytes();
	tokio::fs::write(path.as_std_path(), bytes)
		.await
		.map_err(MoonError::from)?;
	let metadata = tokio::fs::metadata(path.as_std_path()).await.map_err(MoonError::from)?;
	let mtime_ms = metadata.modified().ok().and_then(system_time_to_ms);
	Ok(WriteFileResult {
		mtime_ms,
		bytes_written: bytes.len() as u64,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempfile::TempDir;

	fn host(dir: &TempDir) -> LocalHost {
		let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
		LocalHost::new(root)
	}

	#[tokio::test]
	async fn read_dir_lists_files() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
		std::fs::create_dir(dir.path().join("sub")).unwrap();

		let entries = host(&dir).read_dir(Utf8Path::new(".")).await.unwrap();
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].name, "sub");
		assert_eq!(entries[0].kind, EntryKind::Dir);
		assert_eq!(entries[1].name, "a.txt");
		assert_eq!(entries[1].kind, EntryKind::File);
	}

	#[tokio::test]
	async fn read_and_write_roundtrip() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);
		std::fs::write(dir.path().join("a.txt"), "initial").unwrap();

		let r = h.read_file(Utf8Path::new("a.txt")).await.unwrap();
		assert_eq!(r.text, "initial");
		assert!(!r.is_binary);

		h.write_file(Utf8Path::new("a.txt"), "updated").await.unwrap();
		let r2 = h.read_file(Utf8Path::new("a.txt")).await.unwrap();
		assert_eq!(r2.text, "updated");
	}

	#[tokio::test]
	async fn write_file_creates_missing_parents() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		h.write_file(Utf8Path::new("a/b/c/hello.txt"), "hi").await.unwrap();

		assert!(dir.path().join("a/b/c").is_dir());
		let r = h.read_file(Utf8Path::new("a/b/c/hello.txt")).await.unwrap();
		assert_eq!(r.text, "hi");
	}

	#[tokio::test]
	async fn write_file_mkdir_p_stays_sandboxed() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		// A `..` tail that would climb out of the root must be
		// rejected before any directory is created.
		let err = h
			.write_file(Utf8Path::new("../escape/hello.txt"), "x")
			.await
			.unwrap_err();
		assert!(
			matches!(err, MoonError::InvalidArgument(_) | MoonError::PermissionDenied(_)),
			"expected a sandbox rejection, got {err:?}"
		);
		assert!(!dir.path().parent().unwrap().join("escape").exists());
	}

	#[tokio::test]
	async fn delete_removes_file() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);
		std::fs::write(dir.path().join("a.txt"), "x").unwrap();

		h.delete_path(Utf8Path::new("a.txt")).await.unwrap();
		assert!(!dir.path().join("a.txt").exists());
	}

	#[tokio::test]
	async fn delete_removes_directory_recursively() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);
		std::fs::create_dir_all(dir.path().join("sub/nested")).unwrap();
		std::fs::write(dir.path().join("sub/nested/x.txt"), "x").unwrap();

		h.delete_path(Utf8Path::new("sub")).await.unwrap();
		assert!(!dir.path().join("sub").exists());
	}

	#[tokio::test]
	async fn delete_refuses_workspace_root() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		let result = h.delete_path(Utf8Path::new(".")).await;
		assert!(matches!(result, Err(MoonError::InvalidArgument(_))));
		assert!(dir.path().exists());
	}

	#[tokio::test]
	async fn delete_rejects_path_traversal() {
		let dir = TempDir::new().unwrap();
		let outside = dir.path().parent().unwrap().join("escape-delete.txt");
		std::fs::write(&outside, "x").unwrap();

		let h = host(&dir);
		let result = h.delete_path(Utf8Path::new("../escape-delete.txt")).await;
		assert!(matches!(result, Err(MoonError::PermissionDenied(_))));
		assert!(outside.exists(), "outside file must be untouched");
		std::fs::remove_file(&outside).ok();
	}

	#[tokio::test]
	async fn rejects_path_traversal() {
		let dir = TempDir::new().unwrap();
		let outside = dir.path().parent().unwrap().join("escape.txt");
		std::fs::write(&outside, "x").unwrap();

		let h = host(&dir);
		let result = h.read_file(Utf8Path::new("../escape.txt")).await;
		assert!(matches!(result, Err(MoonError::PermissionDenied(_))));
	}

	#[tokio::test]
	async fn collect_paths_walks_recursively_and_skips_dotgit() {
		let dir = TempDir::new().unwrap();
		std::fs::create_dir(dir.path().join("src")).unwrap();
		std::fs::create_dir(dir.path().join("src").join("nested")).unwrap();
		std::fs::create_dir(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".git").join("HEAD"), "ref").unwrap();
		std::fs::write(dir.path().join("README.md"), "hi").unwrap();
		std::fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();
		std::fs::write(dir.path().join("src").join("nested").join("deep.rs"), "").unwrap();

		let result = host(&dir).collect_paths(6).await.unwrap();
		let set: std::collections::HashSet<_> = result.paths.into_iter().collect();
		assert!(set.contains("README.md"), "got {set:?}");
		assert!(set.contains("src/"), "got {set:?}");
		assert!(set.contains("src/lib.rs"), "got {set:?}");
		assert!(set.contains("src/nested/"), "got {set:?}");
		assert!(set.contains("src/nested/deep.rs"), "got {set:?}");
		// `.git/` and everything inside it stays off the tree.
		assert!(!set.iter().any(|p| p.starts_with(".git")), "got {set:?}");
		assert!(result.depth_capped.is_empty(), "got {:?}", result.depth_capped);
	}

	#[tokio::test]
	async fn collect_paths_skips_into_git_ignored_dirs() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping gitignore-aware collect_paths test");
			return;
		};
		let dir = TempDir::new().unwrap();
		// A `node_modules/`-style nuisance directory plus a real
		// source directory. After `git init` + the `.gitignore`,
		// `git status --ignored=matching` reports `node_modules/`
		// as a single collapsed `!!` record; `collect_paths` should
		// emit that one entry and skip its descendants entirely.
		std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
		std::fs::create_dir_all(dir.path().join("node_modules").join("deep").join("nested")).unwrap();
		std::fs::write(
			dir
				.path()
				.join("node_modules")
				.join("deep")
				.join("nested")
				.join("file.js"),
			"",
		)
		.unwrap();
		std::fs::write(dir.path().join("node_modules").join("top.js"), "").unwrap();
		std::fs::create_dir_all(dir.path().join("src")).unwrap();
		std::fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();
		std::fs::write(dir.path().join("README.md"), "").unwrap();

		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		run_git(&git, dir.path(), &["add", ".gitignore", "src", "README.md"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		let result = host(&dir).collect_paths(6).await.unwrap();
		let set: std::collections::HashSet<_> = result.paths.into_iter().collect();
		assert!(set.contains("README.md"), "got {set:?}");
		assert!(set.contains("src/"), "got {set:?}");
		assert!(set.contains("src/lib.rs"), "got {set:?}");
		// The collapsed `node_modules/` row stays so the user can
		// see it in the tree and the git overlay can tint it.
		assert!(set.contains("node_modules/"), "got {set:?}");
		// Every descendant of `node_modules/` is skipped.
		assert!(
			!set
				.iter()
				.any(|p| p.starts_with("node_modules/") && p != "node_modules/"),
			"node_modules/ contents leaked into the path list, got {set:?}",
		);
	}

	#[tokio::test]
	async fn collect_paths_does_not_skip_in_non_git_folder() {
		// Same shape as the gitignore-aware test, but no `git init`
		// so `git status` errors and the skip set is empty. The
		// walk must enumerate everything — non-repo folders don't
		// have an authoritative ignore source we can consult, so
		// the safe default is "show all paths and let the user
		// decide".
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
		std::fs::create_dir_all(dir.path().join("node_modules").join("deep")).unwrap();
		std::fs::write(dir.path().join("node_modules").join("deep").join("file.js"), "").unwrap();

		let result = host(&dir).collect_paths(6).await.unwrap();
		let set: std::collections::HashSet<_> = result.paths.into_iter().collect();
		assert!(set.contains("node_modules/"), "got {set:?}");
		assert!(set.contains("node_modules/deep/"), "got {set:?}");
		assert!(set.contains("node_modules/deep/file.js"), "got {set:?}");
	}

	#[tokio::test]
	async fn collect_paths_under_walks_one_subtree() {
		// Lazy-load entry point: ignores the gitignore-collapse
		// filter (the caller already decided this subtree is
		// worth fetching) and only walks below the named
		// directory. `max_depth=0` matches the file tree's
		// "one level at a time" lazy load — direct children
		// only, no descent into sub-directories.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("node_modules").join("foo").join("bar")).unwrap();
		std::fs::write(dir.path().join("node_modules").join("top.js"), "").unwrap();
		std::fs::write(dir.path().join("node_modules").join("foo").join("a.js"), "").unwrap();
		std::fs::write(dir.path().join("node_modules").join("foo").join("bar").join("b.js"), "").unwrap();
		std::fs::write(dir.path().join("README.md"), "").unwrap();

		let result = host(&dir)
			.collect_paths_under(Utf8Path::new("node_modules/"), 0)
			.await
			.unwrap();
		let set: std::collections::HashSet<_> = result.paths.into_iter().collect();
		assert!(set.contains("node_modules/foo/"), "got {set:?}");
		assert!(set.contains("node_modules/top.js"), "got {set:?}");
		// `max_depth=0` pushes direct children but never
		// recurses, so `foo/a.js` and `bar/` (let alone deeper)
		// stay out of the result.
		assert!(!set.contains("node_modules/foo/a.js"), "got {set:?}");
		assert!(!set.contains("node_modules/foo/bar/"), "got {set:?}");
		assert!(!set.contains("node_modules/foo/bar/b.js"), "got {set:?}");
		// Sibling subtrees outside `rel` aren't touched.
		assert!(!set.contains("README.md"), "got {set:?}");
		// `node_modules/foo/` stops short of its own children at
		// the cap, so it surfaces as a lazy frontier for the next
		// expansion.
		let capped: std::collections::HashSet<_> = result.depth_capped.into_iter().collect();
		assert!(capped.contains("node_modules/foo/"), "got {capped:?}");
	}

	#[tokio::test]
	async fn collect_paths_under_rejects_empty_rel() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir).collect_paths_under(Utf8Path::new(""), 1).await;
		assert!(matches!(err, Err(MoonError::InvalidArgument(_))));
	}

	#[tokio::test]
	async fn collect_paths_under_rejects_escaping_rel() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir).collect_paths_under(Utf8Path::new("../escape"), 1).await;
		assert!(err.is_err());
	}

	#[tokio::test]
	async fn collect_paths_respects_max_depth() {
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("a").join("b").join("c")).unwrap();
		std::fs::write(dir.path().join("a").join("b").join("c").join("deep.txt"), "").unwrap();

		// depth=0 → only the immediate children are enumerated;
		// `a/` is listed but `a/b/` isn't recursed. `a/` carries
		// hidden descendants so it surfaces as a depth-capped
		// lazy frontier.
		let result = host(&dir).collect_paths(0).await.unwrap();
		let set: std::collections::HashSet<_> = result.paths.into_iter().collect();
		assert!(set.contains("a/"), "got {set:?}");
		assert!(!set.contains("a/b/"), "got {set:?}");
		assert!(!set.contains("a/b/c/deep.txt"), "got {set:?}");
		let capped: std::collections::HashSet<_> = result.depth_capped.into_iter().collect();
		assert!(capped.contains("a/"), "got {capped:?}");
	}

	#[tokio::test]
	async fn collect_paths_skips_lazy_marker_for_empty_dir() {
		// Empty leaf directories at the depth cap are NOT marked
		// as lazy: there's nothing to fetch on expansion, so the
		// frontend should just show them empty rather than fire a
		// pointless IPC roundtrip.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("empty_leaf")).unwrap();
		std::fs::create_dir_all(dir.path().join("populated").join("child")).unwrap();
		std::fs::write(dir.path().join("populated").join("child").join("file.txt"), "").unwrap();

		let result = host(&dir).collect_paths(0).await.unwrap();
		let capped: std::collections::HashSet<_> = result.depth_capped.into_iter().collect();
		assert!(!capped.contains("empty_leaf/"), "got {capped:?}");
		assert!(capped.contains("populated/"), "got {capped:?}");
	}

	#[tokio::test]
	async fn git_status_entries_walker_fallback_flags_ignored() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join(".gitignore"), "target/\nsecrets.txt\n").unwrap();
		std::fs::create_dir(dir.path().join("target")).unwrap();
		std::fs::write(dir.path().join("target").join("binary"), "").unwrap();
		std::fs::write(dir.path().join("secrets.txt"), "shh").unwrap();
		std::fs::write(dir.path().join("README.md"), "").unwrap();
		std::fs::create_dir(dir.path().join("src")).unwrap();
		std::fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();

		let input = vec![
			"README.md".to_string(),
			"secrets.txt".to_string(),
			"src/".to_string(),
			"src/lib.rs".to_string(),
			"target/".to_string(),
			"target/binary".to_string(),
		];
		let entries = host(&dir).git_status_entries(&input).await.unwrap();
		let ignored: std::collections::HashSet<_> = entries
			.iter()
			.filter(|e| matches!(e.status, GitFileStatus::Ignored))
			.map(|e| e.path.clone())
			.collect();
		assert!(ignored.contains("secrets.txt"), "got {ignored:?}");
		assert!(ignored.contains("target/"), "got {ignored:?}");
		assert!(ignored.contains("target/binary"), "got {ignored:?}");
		assert!(!ignored.contains("README.md"), "got {ignored:?}");
		assert!(!ignored.contains("src/"), "got {ignored:?}");
		assert!(!ignored.contains("src/lib.rs"), "got {ignored:?}");
		// No git repo → nothing else should come back with a non-
		// ignored status; the walker can't synthesize add/modify.
		let non_ignored: Vec<_> = entries
			.iter()
			.filter(|e| !matches!(e.status, GitFileStatus::Ignored))
			.collect();
		assert!(
			non_ignored.is_empty(),
			"walker should only report ignored, got {non_ignored:?}"
		);
	}

	#[tokio::test]
	async fn git_status_entries_reports_all_porcelain_states() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping porcelain-driven git status test");
			return;
		};
		let dir = TempDir::new().unwrap();
		// Initial repo: a tracked file we'll modify, a tracked file
		// we'll delete, and a `.gitignore`.
		std::fs::write(dir.path().join(".gitignore"), "target/\n.env\n").unwrap();
		std::fs::write(dir.path().join("tracked.txt"), "one\n").unwrap();
		std::fs::write(dir.path().join("to_delete.txt"), "gone\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();

		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		run_git(
			&git,
			dir.path(),
			&["add", ".gitignore", "tracked.txt", "to_delete.txt", "README.md"],
		);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		// Now make each porcelain state happen exactly once:
		//   - tracked.txt: modify worktree → Modified
		//   - to_delete.txt: `rm` worktree → Deleted
		//   - staged.txt: create + `git add` → Added
		//   - new.txt: create, no `git add` → Untracked
		//   - target/: ignored dir with content → Ignored
		//   - .env: ignored file → Ignored
		std::fs::write(dir.path().join("tracked.txt"), "two\n").unwrap();
		std::fs::remove_file(dir.path().join("to_delete.txt")).unwrap();
		std::fs::write(dir.path().join("staged.txt"), "staged\n").unwrap();
		run_git(&git, dir.path(), &["add", "staged.txt"]);
		std::fs::write(dir.path().join("new.txt"), "new\n").unwrap();
		std::fs::create_dir(dir.path().join("target")).unwrap();
		std::fs::write(dir.path().join("target").join("out"), "").unwrap();
		std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();

		let entries = host(&dir).git_status_entries(&[]).await.unwrap();
		let by_path: std::collections::HashMap<String, GitFileStatus> =
			entries.iter().map(|e| (e.path.clone(), e.status)).collect();

		assert_eq!(
			by_path.get("tracked.txt"),
			Some(&GitFileStatus::Modified),
			"got {by_path:?}"
		);
		assert_eq!(
			by_path.get("to_delete.txt"),
			Some(&GitFileStatus::Deleted),
			"got {by_path:?}"
		);
		assert_eq!(
			by_path.get("staged.txt"),
			Some(&GitFileStatus::Added),
			"got {by_path:?}"
		);
		assert_eq!(
			by_path.get("new.txt"),
			Some(&GitFileStatus::Untracked),
			"got {by_path:?}"
		);
		// Ignored directory collapses to a single `target/` entry;
		// individual children aren't reported separately.
		assert_eq!(by_path.get("target/"), Some(&GitFileStatus::Ignored), "got {by_path:?}");
		assert_eq!(by_path.get(".env"), Some(&GitFileStatus::Ignored), "got {by_path:?}");
		// Clean tracked files don't show up at all — keeps the
		// payload small (big repos can have tens of thousands of
		// clean entries).
		assert!(!by_path.contains_key("README.md"), "got {by_path:?}");
		assert!(!by_path.contains_key(".gitignore"), "got {by_path:?}");
	}

	// A file that matches a `.gitignore` pattern but is tracked must
	// not render as ignored. `.env.example` under a `.env*` rule is
	// the canonical real-world case. Needs `git` on PATH (CI always
	// does).
	#[tokio::test]
	async fn git_status_entries_respects_index() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping index-aware git status test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join(".gitignore"), ".env*\n").unwrap();
		std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
		std::fs::write(dir.path().join(".env.example"), "SECRET=\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "").unwrap();

		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		run_git(&git, dir.path(), &["add", ".gitignore", "README.md"]);
		run_git(&git, dir.path(), &["add", "-f", ".env.example"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		let entries = host(&dir).git_status_entries(&[]).await.unwrap();
		let by_path: std::collections::HashMap<String, GitFileStatus> =
			entries.iter().map(|e| (e.path.clone(), e.status)).collect();
		assert_eq!(by_path.get(".env"), Some(&GitFileStatus::Ignored), "got {by_path:?}");
		// Tracked — not ignored, not dirty → absent from the map.
		assert!(!by_path.contains_key(".env.example"), "got {by_path:?}");
		assert!(!by_path.contains_key("README.md"), "got {by_path:?}");
		assert!(!by_path.contains_key(".gitignore"), "got {by_path:?}");
	}

	#[tokio::test]
	async fn git_restore_paths_reverts_modified_and_deleted_files() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git restore test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("README.md"), "original\n").unwrap();
		std::fs::write(dir.path().join("keep.md"), "keep\n").unwrap();

		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		run_git(&git, dir.path(), &["add", "README.md", "keep.md"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		// Modify one file and delete another (worktree only, not
		// staged). After `git_restore_paths` both should be back to
		// the committed content.
		std::fs::write(dir.path().join("README.md"), "edited\n").unwrap();
		std::fs::remove_file(dir.path().join("keep.md")).unwrap();

		host(&dir)
			.git_restore_paths(&["README.md".to_string(), "keep.md".to_string()])
			.await
			.unwrap();

		assert_eq!(
			std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
			"original\n"
		);
		assert_eq!(std::fs::read_to_string(dir.path().join("keep.md")).unwrap(), "keep\n");

		let entries = host(&dir).git_status_entries(&[]).await.unwrap();
		let dirty: Vec<_> = entries.iter().filter(|e| e.status != GitFileStatus::Ignored).collect();
		assert!(dirty.is_empty(), "expected clean worktree, got {dirty:?}");
	}

	#[tokio::test]
	async fn git_restore_paths_rejects_path_escapes() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir)
			.git_restore_paths(&["../secret.txt".to_string()])
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
	}

	#[tokio::test]
	async fn git_blame_reports_commit_for_each_line() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git blame test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();

		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "alice@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "Alice"]);
		run_git(&git, dir.path(), &["add", "lib.rs"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial: two funcs"]);

		// Second commit touches only line 2. We expect blame to
		// attribute line 1 to the initial commit and line 2 to the
		// amendment, with distinct shas.
		std::fs::write(dir.path().join("lib.rs"), "fn a() {}\nfn b() { todo!() }\n").unwrap();
		run_git(&git, dir.path(), &["config", "user.email", "bob@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "Bob"]);
		run_git(&git, dir.path(), &["commit", "-aq", "-m", "stub out b"]);

		let blame = host(&dir)
			.git_blame(Utf8Path::new("lib.rs"))
			.await
			.unwrap()
			.expect("blame should be Some inside a repo");
		assert_eq!(blame.lines.len(), 2);
		assert_eq!(blame.lines[0].author, "Alice");
		assert_eq!(blame.lines[0].author_email, "alice@example.com");
		assert_eq!(blame.lines[0].summary, "initial: two funcs");
		assert!(!blame.lines[0].is_uncommitted);
		assert_eq!(blame.lines[1].author, "Bob");
		assert_eq!(blame.lines[1].summary, "stub out b");
		assert_ne!(blame.lines[0].sha, blame.lines[1].sha);
	}

	#[tokio::test]
	async fn git_blame_marks_uncommitted_lines() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git blame uncommitted test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("lib.rs"), "first\nsecond\n").unwrap();

		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "lib.rs"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		std::fs::write(dir.path().join("lib.rs"), "first\nsecond\nthird\n").unwrap();

		let blame = host(&dir).git_blame(Utf8Path::new("lib.rs")).await.unwrap().unwrap();
		assert_eq!(blame.lines.len(), 3);
		assert!(!blame.lines[0].is_uncommitted);
		assert!(!blame.lines[1].is_uncommitted);
		assert!(blame.lines[2].is_uncommitted, "new line must be uncommitted");
	}

	#[tokio::test]
	async fn git_blame_returns_none_outside_repo() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("lone.txt"), "no repo\n").unwrap();
		let got = host(&dir).git_blame(Utf8Path::new("lone.txt")).await.unwrap();
		assert!(got.is_none());
	}

	#[tokio::test]
	async fn git_blame_rejects_path_escapes() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir).git_blame(Utf8Path::new("../secret.txt")).await.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
	}

	#[tokio::test]
	async fn git_blame_resolves_github_remote_to_web_url() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping remote URL test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("x.txt"), "one\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "x.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "add x"]);
		run_git(
			&git,
			dir.path(),
			&["remote", "add", "origin", "git@github.com:moon/ide.git"],
		);

		let blame = host(&dir).git_blame(Utf8Path::new("x.txt")).await.unwrap().unwrap();
		assert_eq!(blame.remote_url, "https://github.com/moon/ide");
	}

	#[tokio::test]
	async fn git_permalink_builds_pinned_github_link() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping permalink test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::create_dir(dir.path().join("src")).unwrap();
		std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);
		run_git(
			&git,
			dir.path(),
			&["remote", "add", "origin", "git@github.com:moon/ide.git"],
		);

		// Resolve the expected HEAD SHA so we can assert the link is
		// pinned to the commit, not a branch ref.
		let sha = String::from_utf8(
			std::process::Command::new(&git)
				.arg("-C")
				.arg(dir.path())
				.args(["rev-parse", "HEAD"])
				.output()
				.unwrap()
				.stdout,
		)
		.unwrap()
		.trim()
		.to_owned();

		// Multi-line range.
		let link = host(&dir)
			.git_permalink(Utf8Path::new("src/lib.rs"), 1, 3)
			.await
			.unwrap()
			.expect("permalink should be Some for a GitHub repo");
		assert_eq!(
			link.url,
			format!("https://github.com/moon/ide/blob/{sha}/src/lib.rs#L1-L3")
		);
		assert_eq!(
			link.markdown,
			format!("[src/lib.rs#L1-L3](https://github.com/moon/ide/blob/{sha}/src/lib.rs#L1-L3)")
		);

		// Single-line range drops the `-L<end>` suffix.
		let single = host(&dir)
			.git_permalink(Utf8Path::new("src/lib.rs"), 2, 2)
			.await
			.unwrap()
			.unwrap();
		assert_eq!(
			single.url,
			format!("https://github.com/moon/ide/blob/{sha}/src/lib.rs#L2")
		);
	}

	#[tokio::test]
	async fn git_permalink_returns_none_without_github_remote() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping permalink no-remote test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("x.txt"), "one\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "x.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);
		// No remote at all → None.
		assert!(host(&dir)
			.git_permalink(Utf8Path::new("x.txt"), 1, 1)
			.await
			.unwrap()
			.is_none());
		// A non-GitHub remote → still None.
		run_git(
			&git,
			dir.path(),
			&["remote", "add", "origin", "https://gitlab.com/moon/ide.git"],
		);
		assert!(host(&dir)
			.git_permalink(Utf8Path::new("x.txt"), 1, 1)
			.await
			.unwrap()
			.is_none());
	}

	#[tokio::test]
	async fn git_permalink_rejects_path_escapes() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir)
			.git_permalink(Utf8Path::new("../secret.txt"), 1, 1)
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
	}

	#[tokio::test]
	async fn git_blob_sha_tracks_working_tree_content() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping blob-sha test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);

		// SHA matches `git hash-object` of the working-tree bytes,
		// regardless of index/commit state — the file is untracked
		// here and still resolves.
		let expected = String::from_utf8(
			std::process::Command::new(&git)
				.arg("-C")
				.arg(dir.path())
				.args(["hash-object", "a.txt"])
				.output()
				.unwrap()
				.stdout,
		)
		.unwrap()
		.trim()
		.to_owned();
		let got = host(&dir).git_blob_sha(Utf8Path::new("a.txt")).await.unwrap();
		assert_eq!(got.as_deref(), Some(expected.as_str()));

		// Editing the file changes the SHA — this is what makes a
		// reviewed-file mark auto-clear when the file moves.
		std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
		let after = host(&dir).git_blob_sha(Utf8Path::new("a.txt")).await.unwrap();
		assert!(after.is_some());
		assert_ne!(after, got);

		// A missing file yields `None`, not an error — the caller
		// leaves the row unticked.
		let missing = host(&dir).git_blob_sha(Utf8Path::new("nope.txt")).await.unwrap();
		assert_eq!(missing, None);
	}

	#[tokio::test]
	async fn git_blob_sha_rejects_path_escapes() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir)
			.git_blob_sha(Utf8Path::new("../secret.txt"))
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
	}

	#[test]
	fn review_fingerprint_matches_frontend() {
		// Reference values produced by the TS `reviewFingerprint`
		// (FNV-1a over trimmed-joined lines). Locks cross-language
		// parity so a comment's fingerprint re-resolves at publish.
		assert_eq!(super::review_fingerprint("fn b() {}"), "a0c2d750");
		// Indentation is trimmed before hashing.
		assert_eq!(
			super::review_fingerprint("\tlet x = 1;"),
			super::review_fingerprint("    let x = 1;  ")
		);
		assert_eq!(super::review_fingerprint("\tlet x = 1;"), "91ad1ca7");
		assert_eq!(super::review_fingerprint("first\nsecond"), "9a3d2a39");
	}

	#[test]
	fn resolve_anchor_in_finds_drifted_content() {
		let lines = ["// new", "// new", "fn a() {}", "fn b() {}", "fn c() {}"];
		// `fn b()` was on line 2 originally; the fingerprint relocates
		// it to line 4.
		let fp = super::review_fingerprint("fn b() {}");
		assert_eq!(super::resolve_anchor_in(&lines, 2, 2, &fp), Some((4, 4)));
	}

	#[test]
	fn resolve_anchor_in_reports_lost_when_gone() {
		let lines = ["fn a() {}", "fn totally_different() {}", "fn c() {}"];
		let fp = super::review_fingerprint("fn b() {}");
		assert_eq!(super::resolve_anchor_in(&lines, 2, 2, &fp), None);
	}

	#[test]
	fn resolve_anchor_in_handles_multiline() {
		let lines = ["head", "head", "first", "second", "tail"];
		let fp = super::review_fingerprint("first\nsecond");
		assert_eq!(super::resolve_anchor_in(&lines, 1, 2, &fp), Some((3, 4)));
	}

	#[test]
	fn normalize_remote_url_handles_all_shapes() {
		// SCP-style SSH is what `git clone git@github.com:...` leaves
		// behind.
		assert_eq!(
			super::normalize_remote_url("git@github.com:moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		assert_eq!(
			super::normalize_remote_url("git@github.com:moon/ide"),
			Some("https://github.com/moon/ide".into()),
		);
		// Explicit SSH URL with and without the `git@` user.
		assert_eq!(
			super::normalize_remote_url("ssh://git@github.com/moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		assert_eq!(
			super::normalize_remote_url("ssh://github.com/moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		// HTTPS is already close to right, we just trim `.git`.
		assert_eq!(
			super::normalize_remote_url("https://github.com/moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		assert_eq!(
			super::normalize_remote_url("https://github.com/moon/ide"),
			Some("https://github.com/moon/ide".into()),
		);
		// Unknown hosts are rejected until we add mapping for them —
		// better to leave the frontend un-linkified than to guess at
		// a URL convention.
		assert_eq!(super::normalize_remote_url("https://gitlab.com/moon/ide.git"), None);
		assert_eq!(super::normalize_remote_url("git@bitbucket.org:moon/ide.git"), None);
		assert_eq!(super::normalize_remote_url(""), None);
	}

	#[test]
	fn encode_branch_segment_preserves_safe_chars_and_slashes() {
		// Plain alphanumerics + `-_.~/` pass through verbatim — the
		// "happy path" for nearly every branch name we'll see.
		assert_eq!(super::encode_branch_segment("main"), "main");
		assert_eq!(super::encode_branch_segment("feat/scm-publish"), "feat/scm-publish");
		assert_eq!(super::encode_branch_segment("release-1.2.3"), "release-1.2.3");
		// Anything outside that allow-list percent-encodes, including
		// the rare branch names with `#` / `?` / spaces.
		assert_eq!(super::encode_branch_segment("hot fix"), "hot%20fix");
		assert_eq!(super::encode_branch_segment("ticket#42"), "ticket%2342");
		// Multibyte UTF-8 — each byte percent-encodes individually,
		// which is what GitHub's path encoder does too.
		assert_eq!(super::encode_branch_segment("café"), "caf%C3%A9");
	}

	#[tokio::test]
	async fn git_head_content_returns_committed_text() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping head content test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("greet.txt"), "hello\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "greet.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "add greet"]);
		std::fs::write(dir.path().join("greet.txt"), "hello there\n").unwrap();

		let got = host(&dir).git_head_content(Utf8Path::new("greet.txt")).await.unwrap();
		assert_eq!(got, Some("hello\n".to_string()));
	}

	#[tokio::test]
	async fn git_head_content_returns_text_for_deleted_file() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping deleted-head test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("gone.txt"), "was here\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "gone.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "add gone"]);
		std::fs::remove_file(dir.path().join("gone.txt")).unwrap();

		let got = host(&dir).git_head_content(Utf8Path::new("gone.txt")).await.unwrap();
		assert_eq!(got, Some("was here\n".to_string()));
	}

	#[tokio::test]
	async fn git_head_content_returns_none_for_untracked_file() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping untracked-head test");
			return;
		};
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("keep.txt"), "keep\n").unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		run_git(&git, dir.path(), &["add", "keep.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);
		std::fs::write(dir.path().join("fresh.txt"), "new\n").unwrap();

		let got = host(&dir).git_head_content(Utf8Path::new("fresh.txt")).await.unwrap();
		assert!(got.is_none());
	}

	#[tokio::test]
	async fn git_head_content_returns_none_outside_repo() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("lone.txt"), "no repo\n").unwrap();
		let got = host(&dir).git_head_content(Utf8Path::new("lone.txt")).await.unwrap();
		assert!(got.is_none());
	}

	#[tokio::test]
	async fn git_head_content_rejects_path_escapes() {
		let dir = TempDir::new().unwrap();
		let err = host(&dir)
			.git_head_content(Utf8Path::new("../secret.txt"))
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
	}

	#[tokio::test]
	async fn git_commit_on_new_branch_creates_branch_and_commits() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping new-branch commit test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		std::fs::write(dir.path().join("CHANGES.md"), "wip\n").unwrap();
		let result = host(&dir)
			.git_commit_on_new_branch("feature/wip", "Add CHANGES.md")
			.await
			.unwrap();
		assert!(!result.short_sha.is_empty());
		assert_eq!(result.summary, "Add CHANGES.md");

		let head_branch = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["symbolic-ref", "--short", "HEAD"])
			.output()
			.unwrap();
		assert!(head_branch.status.success());
		assert_eq!(String::from_utf8_lossy(&head_branch.stdout).trim(), "feature/wip");
	}

	#[tokio::test]
	async fn git_worktree_add_list_remove_round_trip() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping worktree round-trip test");
			return;
		};
		if !relative_worktrees_supported() {
			eprintln!("git < 2.48 (no relative worktrees) — skipping worktree round-trip test");
			return;
		}
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// The worktree lives *inside* the parent repo (ADR 0029:
		// `<parent>/.worktrees/<slug>`) so it rides the parent's bind
		// mount and resolves the same on host and container.
		let wt_path = Utf8PathBuf::from_path_buf(dir.path().join(".worktrees").join("agent-wt")).unwrap();

		let created = host(&dir)
			.git_worktree_add(&wt_path, WorktreeBranch::New("moon/agent-1".into()))
			.await
			.unwrap();
		assert_eq!(created.branch.as_deref(), Some("moon/agent-1"));
		assert!(!created.is_main);
		assert!(!created.head.is_empty());
		// Locked at creation so a `git worktree prune` can't sever it.
		assert!(created.is_locked, "fresh worktree should be locked");
		assert!(wt_path.join("README.md").exists(), "worktree was checked out");

		// Links are relative (`--relative-paths`) so they resolve under
		// either mount — the whole point of ADR 0029.
		let dot_git = std::fs::read_to_string(wt_path.join(".git")).unwrap();
		let link = dot_git
			.trim()
			.strip_prefix("gitdir:")
			.expect("a gitdir link")
			.trim()
			.to_string();
		assert!(
			link.starts_with('.'),
			"worktree .git link should be relative, got {link:?}"
		);

		// `.worktrees/` is excluded so it never dirties the parent status.
		let exclude = std::fs::read_to_string(dir.path().join(".git").join("info").join("exclude")).unwrap();
		assert!(
			exclude.lines().any(|l| l.trim() == "/.worktrees/"),
			"parent info/exclude should hide .worktrees, got {exclude:?}"
		);

		let list = host(&dir).git_worktree_list().await.unwrap();
		assert_eq!(list.len(), 2, "main tree plus the one we added");
		assert!(list[0].is_main);
		let linked = list.iter().find(|w| !w.is_main).expect("linked worktree present");
		assert_eq!(linked.branch.as_deref(), Some("moon/agent-1"));
		assert!(linked.is_locked, "list should report the worktree as locked");

		// Remove unlocks first, so a locked worktree still removes
		// cleanly without the caller having to unlock by hand.
		host(&dir).git_worktree_remove(&wt_path, false).await.unwrap();
		assert!(!wt_path.join("README.md").exists(), "worktree was pruned");
		let after = host(&dir).git_worktree_list().await.unwrap();
		assert_eq!(after.len(), 1);
		assert!(after[0].is_main);
	}

	#[tokio::test]
	async fn git_worktree_add_checks_out_existing_branch() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping worktree existing-branch test");
			return;
		};
		if !relative_worktrees_supported() {
			eprintln!("git < 2.48 (no relative worktrees) — skipping worktree existing-branch test");
			return;
		}
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);
		// A pre-existing branch (e.g. a colleague's) we want an agent
		// to pick up without the parent checking it out.
		run_git(&git, dir.path(), &["branch", "feature-x"]);

		let wt_parent = TempDir::new().unwrap();
		let wt_path = Utf8PathBuf::from_path_buf(wt_parent.path().join("agent-wt")).unwrap();

		let created = host(&dir)
			.git_worktree_add(&wt_path, WorktreeBranch::Existing("feature-x".into()))
			.await
			.unwrap();
		assert_eq!(
			created.branch.as_deref(),
			Some("feature-x"),
			"checks out the existing branch"
		);
		assert!(created.is_locked);
		// Parent stays on main — the whole point is not disturbing it.
		let parent_head = std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
		assert_eq!(parent_head.trim(), "ref: refs/heads/main");

		host(&dir).git_worktree_remove(&wt_path, false).await.unwrap();
	}

	#[tokio::test]
	async fn git_worktree_add_moving_resets_main_and_checks_out_branch() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping worktree move test");
			return;
		};
		if !relative_worktrees_supported() {
			eprintln!("git < 2.48 (no relative worktrees) — skipping worktree move test");
			return;
		}
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);
		// Work on a feature branch (the main tree is currently on it).
		run_git(&git, dir.path(), &["switch", "-qc", "feature-x"]);
		std::fs::write(dir.path().join("feat.txt"), "feat\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "feature work"]);

		let wt_parent = TempDir::new().unwrap();
		let wt_path = Utf8PathBuf::from_path_buf(wt_parent.path().join("agent-wt")).unwrap();

		let created = host(&dir)
			.git_worktree_add_moving(
				&wt_path,
				WorktreeBranch::Existing("feature-x".into()),
				Some("main".into()),
			)
			.await
			.unwrap();
		assert_eq!(created.branch.as_deref(), Some("feature-x"));
		assert!(
			wt_path.join("feat.txt").exists(),
			"worktree carries the branch's content"
		);
		// Main tree reset to the default branch, freeing feature-x.
		let main_head = std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
		assert_eq!(main_head.trim(), "ref: refs/heads/main");

		host(&dir).git_worktree_remove(&wt_path, false).await.unwrap();
	}

	#[tokio::test]
	async fn git_worktree_add_moving_errors_on_dirty_tree() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping worktree move dirty test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);
		run_git(&git, dir.path(), &["switch", "-qc", "feature-x"]);
		// Uncommitted change — the move must refuse rather than risk
		// carrying it to the wrong branch.
		std::fs::write(dir.path().join("dirty.txt"), "wip\n").unwrap();

		let wt_parent = TempDir::new().unwrap();
		let wt_path = Utf8PathBuf::from_path_buf(wt_parent.path().join("agent-wt")).unwrap();
		let err = host(&dir)
			.git_worktree_add_moving(
				&wt_path,
				WorktreeBranch::Existing("feature-x".into()),
				Some("main".into()),
			)
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
		// Main tree untouched — still on feature-x.
		let head = std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
		assert_eq!(head.trim(), "ref: refs/heads/feature-x");
	}

	#[tokio::test]
	async fn git_worktree_add_rejects_invalid_branch() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping worktree invalid-branch test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		let wt_parent = TempDir::new().unwrap();
		let wt_path = Utf8PathBuf::from_path_buf(wt_parent.path().join("bad")).unwrap();
		// Spaces are illegal in git ref names; we fail before creating
		// anything on disk.
		let err = host(&dir)
			.git_worktree_add(&wt_path, WorktreeBranch::New("not a branch".into()))
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");
		assert!(!wt_path.exists(), "no worktree dir left behind");
	}

	#[test]
	fn parse_worktree_porcelain_handles_branch_detached_locked() {
		let sample = "worktree /repo/main\nHEAD aaaa1111\nbranch refs/heads/main\n\n\
			worktree /wt/feat\nHEAD bbbb2222\nbranch refs/heads/feat/x\nlocked\n\n\
			worktree /wt/detached\nHEAD cccc3333\ndetached\n";
		let got = super::parse_worktree_porcelain(sample);
		assert_eq!(got.len(), 3);
		assert!(got[0].is_main);
		assert_eq!(got[0].branch.as_deref(), Some("main"));
		assert_eq!(got[0].head, "aaaa1111");
		assert!(!got[1].is_main);
		assert_eq!(got[1].branch.as_deref(), Some("feat/x"));
		assert!(got[1].is_locked);
		assert_eq!(got[2].branch, None);
		assert_eq!(got[2].head, "cccc3333");
	}

	#[tokio::test]
	async fn git_commit_on_new_branch_rejects_invalid_name() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping invalid-branch test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Spaces are illegal in git ref names; we should fail before
		// touching the index.
		let err = host(&dir)
			.git_commit_on_new_branch("not a branch", "msg")
			.await
			.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)), "got {err:?}");

		// HEAD should still be on `main`, not on a half-created branch.
		let head_branch = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["symbolic-ref", "--short", "HEAD"])
			.output()
			.unwrap();
		assert_eq!(String::from_utf8_lossy(&head_branch.stdout).trim(), "main");
	}

	#[test]
	fn git_command_host_runs_git_directly() {
		let root = Utf8Path::new("/home/dev/code/proj");
		let cmd = git_command(&ShellTarget::Host, root);
		let std_cmd = cmd.get_program().to_owned();
		assert_eq!(std_cmd, "git");
		let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
		assert_eq!(args, vec!["-C", "/home/dev/code/proj"]);
	}

	#[test]
	fn git_command_container_wraps_docker_exec_with_translated_root() {
		// When the workspace shell container is running, the commit
		// (and therefore its pre-commit hook) must run inside the
		// container so the hook resolves the project's own
		// toolchain. The `.git` bytes are identical through the
		// bind mount, so this only changes the hook's environment.
		let target = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/proj"),
			server_root: Utf8PathBuf::from("/workspace/proj"),
		};
		let cmd = git_command(&target, Utf8Path::new("/home/dev/code/proj"));
		assert_eq!(cmd.get_program(), "docker");
		let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
		assert_eq!(
			args,
			vec![
				"exec",
				"-w",
				"/workspace/proj",
				"moon-ws-default-dev-1",
				"git",
				"-C",
				"/workspace/proj"
			]
		);
	}

	#[test]
	fn git_command_container_falls_back_to_host_outside_mount() {
		// A folder outside the bind mount can't be reached by an
		// in-container git, so we run on the host rather than spawn
		// against a path the container can't see.
		let target = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/proj"),
			server_root: Utf8PathBuf::from("/workspace/proj"),
		};
		let cmd = git_command(&target, Utf8Path::new("/etc/elsewhere"));
		assert_eq!(cmd.get_program(), "git");
		let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
		assert_eq!(args, vec!["-C", "/etc/elsewhere"]);
	}

	#[tokio::test]
	async fn git_commit_on_new_branch_rolls_back_on_empty_index() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping rollback test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Working tree is clean; the commit step should fail with
		// "nothing to commit" and the host should roll the fresh
		// branch back so HEAD lands on `main` again.
		let err = host(&dir)
			.git_commit_on_new_branch("feature/nope", "msg")
			.await
			.unwrap_err();
		let detail = format!("{err:?}");
		assert!(detail.contains("nothing to commit"), "got {detail}");

		let head_branch = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["symbolic-ref", "--short", "HEAD"])
			.output()
			.unwrap();
		assert_eq!(String::from_utf8_lossy(&head_branch.stdout).trim(), "main");

		// And the branch we tried to create should be gone.
		let branch_list = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["branch", "--list", "feature/nope"])
			.output()
			.unwrap();
		assert!(String::from_utf8_lossy(&branch_list.stdout).trim().is_empty());
	}

	#[tokio::test]
	async fn safety_snapshot_restores_after_destructive_pre_commit_hook() {
		// Pre-commit frameworks (lint-staged, pre-commit) do their
		// own stash dance during `git commit`. When that dance is
		// interrupted mid-`git stash apply`, the working tree
		// loses files. Our safety snapshot (taken right before
		// `git add -A`) is the last-resort restore that brings
		// them back. This test simulates that exact corruption: a
		// hook that deletes the working tree and then exits
		// non-zero, mimicking a crashed lint-staged.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping safety-snapshot test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Tracked modification, plus an untracked new file.
		std::fs::write(dir.path().join("seed.txt"), "modified content\n").unwrap();
		std::fs::write(dir.path().join("brand_new.rs"), "fn main() {}\n// new file\n").unwrap();

		// Destructive pre-commit hook: blow away the staged paths
		// and exit non-zero. This is the worst-case shape of a
		// hook crash mid-flight.
		let hooks_dir = dir.path().join(".git/hooks");
		std::fs::create_dir_all(&hooks_dir).unwrap();
		let hook_path = hooks_dir.join("pre-commit");
		std::fs::write(&hook_path, "#!/bin/sh\nrm -f seed.txt brand_new.rs\nexit 1\n").unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
			perms.set_mode(0o755);
			std::fs::set_permissions(&hook_path, perms).unwrap();
		}

		let result = host(&dir).git_commit("test commit", false).await;
		assert!(result.is_err(), "commit should have failed: {result:?}");

		// Files survive thanks to the safety snapshot. Without the
		// snapshot the hook's `rm -f` leaves the working tree
		// empty (only `seed.txt` would come back via git's own
		// implicit reset, and `brand_new.rs` would be permanently
		// gone — it was untracked).
		assert!(
			dir.path().join("seed.txt").exists(),
			"tracked file lost after hook crash"
		);
		assert!(
			dir.path().join("brand_new.rs").exists(),
			"untracked file lost after hook crash"
		);
		let seed = std::fs::read_to_string(dir.path().join("seed.txt")).unwrap();
		assert_eq!(seed, "modified content\n", "tracked file content lost");
		let brand_new = std::fs::read_to_string(dir.path().join("brand_new.rs")).unwrap();
		assert_eq!(brand_new, "fn main() {}\n// new file\n", "untracked file content lost");
	}

	#[tokio::test]
	async fn safety_snapshot_restores_after_destructive_hook_on_new_branch() {
		// Same setup, but exercising `git_commit_on_new_branch`
		// since that's the path the user actually hit. The flow
		// composes (`git_commit_on_new_branch` calls into
		// `run_git_commit` which holds the snapshot), so the
		// expectation is identical: files survive on disk after
		// the hook crash.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping safety-snapshot new-branch test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		std::fs::write(dir.path().join("seed.txt"), "branch-mode change\n").unwrap();
		std::fs::write(dir.path().join("new_on_branch.rs"), "// new\n").unwrap();

		let hooks_dir = dir.path().join(".git/hooks");
		std::fs::create_dir_all(&hooks_dir).unwrap();
		let hook_path = hooks_dir.join("pre-commit");
		std::fs::write(&hook_path, "#!/bin/sh\nrm -f seed.txt new_on_branch.rs\nexit 1\n").unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
			perms.set_mode(0o755);
			std::fs::set_permissions(&hook_path, perms).unwrap();
		}

		let result = host(&dir).git_commit_on_new_branch("feature/safety", "msg").await;
		assert!(result.is_err(), "commit should have failed: {result:?}");

		assert!(dir.path().join("seed.txt").exists());
		assert!(dir.path().join("new_on_branch.rs").exists());
		assert_eq!(
			std::fs::read_to_string(dir.path().join("seed.txt")).unwrap(),
			"branch-mode change\n"
		);

		// Branch rollback still happened — HEAD is back on `main`.
		let head_branch = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["symbolic-ref", "--short", "HEAD"])
			.output()
			.unwrap();
		assert_eq!(String::from_utf8_lossy(&head_branch.stdout).trim(), "main");
	}

	#[tokio::test]
	async fn concurrent_commits_serialise_via_git_mutex() {
		// Two commits firing concurrently against the same host.
		// The pre-commit hook sleeps for 200ms, so without the
		// per-folder mutex the two `git add` / `git commit`
		// invocations would overlap and at least one would fail
		// with `Unable to create '.git/index.lock': File exists`.
		// With the mutex the second commit waits for the first
		// to finish entirely; both succeed.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping concurrent-commits test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Slow but harmless hook.
		let hooks_dir = dir.path().join(".git/hooks");
		std::fs::create_dir_all(&hooks_dir).unwrap();
		let hook_path = hooks_dir.join("pre-commit");
		std::fs::write(&hook_path, "#!/bin/sh\nsleep 0.2\nexit 0\n").unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
			perms.set_mode(0o755);
			std::fs::set_permissions(&hook_path, perms).unwrap();
		}

		let host = std::sync::Arc::new(host(&dir));

		std::fs::write(dir.path().join("a.txt"), "first\n").unwrap();
		let host1 = host.clone();
		let h1 = tokio::spawn(async move { host1.git_commit("first commit", false).await });

		// Brief jitter so both tasks are in flight; without the
		// mutex this is the window where their `.git/index.lock`
		// usage overlaps. With the mutex, h2 just waits.
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;

		std::fs::write(dir.path().join("b.txt"), "second\n").unwrap();
		let host2 = host.clone();
		let h2 = tokio::spawn(async move { host2.git_commit("second commit", false).await });

		let r1 = h1.await.unwrap();
		let r2 = h2.await.unwrap();
		assert!(r1.is_ok(), "first commit failed: {r1:?}");
		assert!(r2.is_ok(), "second commit failed: {r2:?}");

		// Both commits land in the log on top of the initial one.
		let log = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["log", "--oneline"])
			.output()
			.unwrap();
		let lines: Vec<_> = String::from_utf8_lossy(&log.stdout)
			.lines()
			.filter(|l| !l.is_empty())
			.map(str::to_owned)
			.collect();
		assert_eq!(lines.len(), 3, "expected initial + 2 commits, got: {lines:?}");

		// `.git/index.lock` is not lingering.
		assert!(
			!dir.path().join(".git/index.lock").exists(),
			"index.lock leaked across commits"
		);
	}

	#[tokio::test]
	async fn git_diff_summary_lists_changed_files() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping diff-summary test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);
		std::fs::write(dir.path().join("a.txt"), "alpha changed\n").unwrap();

		let summary = host(&dir).git_diff_summary().await.unwrap();
		assert!(summary.contains("a.txt"), "got {summary:?}");
		assert!(summary.contains("file changed"), "got {summary:?}");
	}

	#[tokio::test]
	async fn git_diff_summary_includes_untracked_files() {
		// Same rationale as the patch path: `git add -A` will pick
		// up untracked files at commit time, so the summary the
		// branch-name suggester sees has to include them too.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping diff-summary untracked test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Tracked modification + a brand-new file + an ignored file.
		std::fs::write(dir.path().join("seed.txt"), "seed changed\n").unwrap();
		std::fs::write(
			dir.path().join("brand_new.rs"),
			"fn one() {}\nfn two() {}\nfn three() {}\n",
		)
		.unwrap();
		std::fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
		std::fs::write(dir.path().join("ignored.log"), "noise\n").unwrap();

		let summary = host(&dir).git_diff_summary().await.unwrap();
		assert!(summary.contains("seed.txt"), "tracked entry missing: {summary:?}");
		assert!(summary.contains("brand_new.rs"), "untracked entry missing: {summary:?}");
		// Three lines + bar marker for the new file.
		assert!(summary.contains("brand_new.rs | 3"), "line count missing: {summary:?}");
		// The .gitignore file itself is untracked → does surface.
		assert!(summary.contains(".gitignore"), "gitignore should surface: {summary:?}");
		// But the file matched by .gitignore must not.
		assert!(!summary.contains("ignored.log"), "ignored file leaked: {summary:?}");

		// Single reconciled totals line covering tracked + untracked.
		let totals_count = summary
			.lines()
			.filter(|l| l.contains("files changed") || l.contains("file changed"))
			.count();
		assert_eq!(totals_count, 1, "expected exactly one totals line, got {summary:?}");
		// 1 tracked + 2 untracked (brand_new.rs, .gitignore) = 3 files.
		assert!(summary.contains("3 files changed"), "totals miscounted: {summary:?}");
	}

	#[tokio::test]
	async fn git_diff_summary_marks_untracked_binary() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping diff-summary binary test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		let mut bytes = vec![0u8; 16];
		bytes[3] = 0;
		bytes.extend_from_slice(b"payload");
		std::fs::write(dir.path().join("blob.bin"), &bytes).unwrap();

		let summary = host(&dir).git_diff_summary().await.unwrap();
		assert!(summary.contains("blob.bin | Bin"), "binary marker missing: {summary:?}");
	}

	#[test]
	fn merge_diff_summary_handles_empty_tracked() {
		// Fresh repo with no commits: tracked summary is empty
		// because `git diff HEAD` has no HEAD to diff against. We
		// still want a coherent summary built purely from untracked
		// files so the "first commit ever" branch-name suggestion
		// has something to chew on.
		let untracked = vec![
			UntrackedSummary {
				path: "src/lib.rs".to_string(),
				lines: Some(42),
			},
			UntrackedSummary {
				path: "README.md".to_string(),
				lines: Some(1),
			},
		];
		let merged = merge_diff_summary("", &untracked);
		assert!(merged.contains("src/lib.rs | 42"), "got {merged:?}");
		assert!(merged.contains("README.md | 1"), "got {merged:?}");
		assert!(merged.contains("2 files changed"), "got {merged:?}");
		assert!(merged.contains("43 insertions(+)"), "got {merged:?}");
	}

	#[test]
	fn merge_diff_summary_reconciles_totals() {
		// Mock tracked stat: 1 file, 5 insertions, 2 deletions.
		// Adding one untracked text file with 10 lines should yield
		// 2 files / 15 insertions / 2 deletions.
		let tracked = " a.txt | 7 +++++--\n 1 file changed, 5 insertions(+), 2 deletions(-)\n";
		let untracked = vec![UntrackedSummary {
			path: "b.txt".to_string(),
			lines: Some(10),
		}];
		let merged = merge_diff_summary(tracked, &untracked);
		assert!(merged.contains("a.txt | 7"), "tracked entry dropped: {merged:?}");
		assert!(merged.contains("b.txt | 10"), "untracked entry missing: {merged:?}");
		// Old totals line is gone, replaced with the reconciled one.
		assert_eq!(
			merged.matches("file changed").count() + merged.matches("files changed").count(),
			1
		);
		assert!(merged.contains("2 files changed"), "got {merged:?}");
		assert!(merged.contains("15 insertions(+)"), "got {merged:?}");
		assert!(merged.contains("2 deletions(-)"), "got {merged:?}");
	}

	#[test]
	fn merge_diff_summary_handles_empty_input() {
		// Clean tree, no untracked files: short-circuit to empty
		// string so callers can keep their "nothing to summarise"
		// path.
		let merged = merge_diff_summary("", &[]);
		assert!(merged.is_empty(), "got {merged:?}");
	}

	#[tokio::test]
	async fn git_head_commit_message_returns_subject_and_body() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping head_commit_message test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(
			&git,
			dir.path(),
			&[
				"commit",
				"-q",
				"-m",
				"Add bucket sync",
				"-m",
				"Body line one.\nBody line two.",
			],
		);
		let msg = host(&dir).git_head_commit_message().await.unwrap();
		assert!(msg.starts_with("Add bucket sync"), "got {msg:?}");
		assert!(msg.contains("Body line one."), "got {msg:?}");
		// Subject + body separator survives; trailing newline does not.
		assert!(!msg.ends_with('\n'), "got {msg:?}");
	}

	#[tokio::test]
	async fn git_head_commit_message_empty_when_no_commits() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping no-commits head_commit_message test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		// Fresh repo, no commits yet — `git log -1` exits non-zero;
		// host returns empty string, callers treat as "nothing to
		// prefill".
		assert_eq!(host(&dir).git_head_commit_message().await.unwrap(), "");
	}

	#[tokio::test]
	async fn git_log_returns_recent_commits() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_log test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "Alice"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "First commit"]);
		std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "Second commit"]);

		let entries = host(&dir).git_log(10).await.unwrap();
		assert_eq!(entries.len(), 2, "got {entries:?}");
		// Newest first.
		assert_eq!(entries[0].subject, "Second commit");
		assert_eq!(entries[0].author, "Alice");
		assert!(entries[0].sha.len() == 40, "full SHA: {}", entries[0].sha);
		assert!(entries[0].short_sha.len() == 7, "short SHA: {}", entries[0].short_sha);
		assert!(!entries[0].date_relative.is_empty());
		assert_eq!(entries[1].subject, "First commit");
	}

	#[tokio::test]
	async fn git_log_empty_when_no_commits() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping no-commits git_log test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		assert_eq!(host(&dir).git_log(10).await.unwrap(), Vec::new());
	}

	#[tokio::test]
	async fn git_commit_diff_lists_changed_files() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_commit_diff test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "Alice"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "First"]);
		std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
		std::fs::write(dir.path().join("a.txt"), "alpha\nchanged\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "Second"]);

		// Get the HEAD SHA.
		let head = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["rev-parse", "HEAD"])
			.output()
			.unwrap();
		let sha = String::from_utf8_lossy(&head.stdout).trim().to_owned();

		let diff = host(&dir).git_commit_diff(&sha).await.unwrap();
		let diff = diff.expect("commit diff should resolve");
		assert_eq!(diff.commit_sha, sha);
		assert!(!diff.parent_sha.is_empty(), "parent SHA should resolve");
		assert_eq!(diff.subject, "Second");
		// a.txt modified, b.txt added.
		assert_eq!(diff.entries.len(), 2, "got {diff:?}");
	}

	#[tokio::test]
	async fn git_commit_diff_root_commit_has_empty_parent() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_commit_diff root test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "Alice"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "Root"]);

		let head = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["rev-parse", "HEAD"])
			.output()
			.unwrap();
		let sha = String::from_utf8_lossy(&head.stdout).trim().to_owned();

		let diff = host(&dir).git_commit_diff(&sha).await.unwrap();
		let diff = diff.expect("root commit diff should resolve");
		assert_eq!(diff.parent_sha, "", "root commit has no parent");
		assert_eq!(diff.entries.len(), 1, "one file added");
		assert_eq!(diff.entries[0].status, GitFileStatus::Added);
	}

	#[tokio::test]
	async fn git_diff_patch_returns_patch_and_truncates_above_cap() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping diff_patch test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		std::fs::write(dir.path().join("a.txt"), "alpha changed\n").unwrap();
		let patch = host(&dir).git_diff_patch().await.unwrap();
		assert!(patch.contains("diff --git"), "got {patch:?}");
		assert!(patch.contains("alpha changed"), "got {patch:?}");
		assert!(
			!patch.contains("(diff truncated)"),
			"small diff was unexpectedly truncated: {patch:?}"
		);

		// Now blow past the 64 KB cap with a long file change.
		let huge: String = (0..8000).map(|i| format!("line {i}\n")).collect();
		std::fs::write(dir.path().join("a.txt"), huge).unwrap();
		let patch = host(&dir).git_diff_patch().await.unwrap();
		assert!(patch.len() <= 65_000, "patch={} should be capped near 64k", patch.len());
		assert!(patch.contains("(diff truncated)"), "missing truncation sentinel");
		// Truncation cuts at a newline; we'd see a hanging partial
		// line otherwise (everything before the sentinel ends in `\n`).
		let body = patch.trim_end_matches("... (diff truncated)\n");
		assert!(body.ends_with('\n'), "truncation should land on a newline boundary");
	}

	#[tokio::test]
	async fn git_diff_patch_includes_untracked_files() {
		// `commit` runs `git add -A` first, so untracked files are
		// part of the commit. The patch surface for the AI commit
		// suggester therefore has to include them too — otherwise
		// the model writes a message that ignores brand-new files.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping diff_patch untracked test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Tracked modification + a brand-new file + an ignored
		// file (which must NOT show up).
		std::fs::write(dir.path().join("a.txt"), "alpha changed\n").unwrap();
		std::fs::write(dir.path().join("new.txt"), "fresh line one\nfresh line two\n").unwrap();
		std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
		std::fs::write(dir.path().join("ignored.txt"), "should not appear\n").unwrap();

		let patch = host(&dir).git_diff_patch().await.unwrap();
		// Tracked modification still surfaces.
		assert!(patch.contains("alpha changed"), "tracked diff missing: {patch:?}");
		// Untracked file shows up as a "new file mode" entry, just
		// like git would emit once it's been added.
		assert!(
			patch.contains("diff --git a/new.txt b/new.txt"),
			"untracked header missing: {patch:?}"
		);
		assert!(
			patch.contains("new file mode 100644"),
			"new file marker missing: {patch:?}"
		);
		assert!(patch.contains("--- /dev/null"), "/dev/null marker missing: {patch:?}");
		assert!(patch.contains("+fresh line one"), "first line missing: {patch:?}");
		assert!(patch.contains("+fresh line two"), "second line missing: {patch:?}");
		// Ignored file doesn't leak in. (The string "ignored.txt"
		// itself does appear — the new `.gitignore` is untracked
		// and therefore part of the commit, so its contents are in
		// the patch. We check for the ignored file's body and
		// header instead.)
		assert!(!patch.contains("should not appear"), "ignored file leaked: {patch:?}");
		assert!(
			!patch.contains("b/ignored.txt"),
			"ignored file's diff header leaked: {patch:?}"
		);
	}

	#[tokio::test]
	async fn git_diff_patch_marks_untracked_binary_files() {
		// Untracked binaries surface as the same `Binary files ...
		// differ` line real `git diff` emits, so the model knows
		// the file is part of the commit without us shovelling raw
		// bytes into the prompt.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping diff_patch binary test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
		run_git(&git, dir.path(), &["add", "."]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "initial"]);

		// Null byte in the first 8 KB → binary heuristic trips.
		let mut bytes = vec![0u8; 16];
		bytes[3] = 0;
		bytes.extend_from_slice(b"some payload");
		std::fs::write(dir.path().join("blob.bin"), &bytes).unwrap();

		let patch = host(&dir).git_diff_patch().await.unwrap();
		assert!(
			patch.contains("Binary files /dev/null and b/blob.bin differ"),
			"binary marker missing: {patch:?}"
		);
		// And the raw payload doesn't end up in the prompt.
		assert!(!patch.contains("some payload"), "binary contents leaked: {patch:?}");
	}

	fn which_git() -> Option<std::path::PathBuf> {
		std::process::Command::new("git")
			.arg("--version")
			.output()
			.ok()
			.filter(|o| o.status.success())
			.map(|_| std::path::PathBuf::from("git"))
	}

	/// Worktree-add version-gates on git >= 2.48 (relative links), so
	/// the real-`git worktree add` tests skip on older git.
	fn relative_worktrees_supported() -> bool {
		git_major_minor(&ShellTarget::Host, Utf8Path::new(".")).is_some_and(|v| v >= MIN_GIT_FOR_RELATIVE_WORKTREES)
	}

	fn run_git(git: &std::path::Path, cwd: &std::path::Path, args: &[&str]) {
		// Scrub the ambient identity. Devs (and CI containers) often
		// have `GIT_AUTHOR_NAME` / `GIT_COMMITTER_NAME` exported in
		// their shell — those override `git config user.name` and
		// make the blame / log tests assert against the wrong name.
		// Removing them here pins every test commit to the repo-local
		// `user.name` / `user.email` the test sets up.
		let out = std::process::Command::new(git)
			.arg("-C")
			.arg(cwd)
			.args(args)
			.env_remove("GIT_AUTHOR_NAME")
			.env_remove("GIT_AUTHOR_EMAIL")
			.env_remove("GIT_AUTHOR_DATE")
			.env_remove("GIT_COMMITTER_NAME")
			.env_remove("GIT_COMMITTER_EMAIL")
			.env_remove("GIT_COMMITTER_DATE")
			.output()
			.expect("spawn git");
		assert!(
			out.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
	}

	/// SHA a ref points at in a (bare) repo, or `None` when the ref
	/// doesn't exist. Used to assert a push did / didn't move a
	/// remote branch.
	fn remote_ref_sha(git: &std::path::Path, repo: &std::path::Path, ref_name: &str) -> Option<String> {
		let out = std::process::Command::new(git)
			.arg("-C")
			.arg(repo)
			.args(["rev-parse", "--verify", "--quiet", ref_name])
			.output()
			.expect("spawn git rev-parse");
		if !out.status.success() {
			return None;
		}
		let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
		if sha.is_empty() {
			None
		} else {
			Some(sha)
		}
	}

	#[tokio::test]
	async fn git_fetch_advances_remote_tracking_ref() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_fetch test");
			return;
		};

		// Three repos: a bare `remote.git` to push against, `local`
		// is the workspace under test (we run `git_fetch` here), and
		// `pusher` is an unrelated clone used to land a new commit
		// on `remote.git` *behind* `local`'s back. After fetch,
		// `local`'s `refs/remotes/origin/main` should point at
		// `pusher`'s last commit.
		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let pusher = root.path().join("pusher");
		let local = root.path().join("local");

		// Bare repo doesn't have any branches yet — `pusher` (a
		// fresh non-clone) creates `main` and pushes it so subsequent
		// `clone` commands have a default branch to land on.
		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		std::fs::create_dir_all(&pusher).unwrap();
		run_git(&git, &pusher, &["init", "-q", "-b", "main"]);
		run_git(&git, &pusher, &["config", "user.email", "p@example.com"]);
		run_git(&git, &pusher, &["config", "user.name", "Pusher"]);
		run_git(&git, &pusher, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(pusher.join("README.md"), "v1\n").unwrap();
		run_git(&git, &pusher, &["add", "."]);
		run_git(&git, &pusher, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &pusher, &["push", "-q", "-u", "origin", "main"]);

		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);

		// Land a new commit upstream that `local` knows nothing about.
		std::fs::write(pusher.join("README.md"), "v2\n").unwrap();
		run_git(&git, &pusher, &["commit", "-q", "-am", "second"]);
		run_git(&git, &pusher, &["push", "-q", "origin", "main"]);

		// Pre-fetch: `local`'s `origin/main` is still on the first commit.
		let pre = std::process::Command::new(&git)
			.arg("-C")
			.arg(&local)
			.args(["rev-parse", "refs/remotes/origin/main"])
			.output()
			.unwrap();
		let pre_sha = String::from_utf8_lossy(&pre.stdout).trim().to_string();

		// Run the function under test.
		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		LocalHost::new(local_root).git_fetch().await.unwrap();

		// Post-fetch: `origin/main` advanced to the second commit.
		let post = std::process::Command::new(&git)
			.arg("-C")
			.arg(&local)
			.args(["rev-parse", "refs/remotes/origin/main"])
			.output()
			.unwrap();
		let post_sha = String::from_utf8_lossy(&post.stdout).trim().to_string();
		assert_ne!(pre_sha, post_sha, "refs/remotes/origin/main did not advance");

		// And `git_branch` now reports `behind = 1` against the
		// upstream — exactly the signal the SCM panel reads to
		// surface "Sync Changes".
		let branch = LocalHost::new(Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap())
			.git_branch()
			.await
			.unwrap();
		assert_eq!(branch.behind, 1, "expected behind=1 after fetch, got {branch:?}");
	}

	#[tokio::test]
	async fn git_fetch_fails_fast_on_unreachable_remote() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_fetch unreachable test");
			return;
		};

		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "a@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "A"]);
		// `file://` URL pointing at a path that doesn't exist —
		// git fails synchronously with "does not appear to be a git
		// repository", no network involved, no auth prompt risk.
		// Validates the error-propagation path without exercising
		// the 30s timeout (which would slow the test down for no
		// reason).
		run_git(
			&git,
			dir.path(),
			&["remote", "add", "origin", "file:///definitely/not/a/repo"],
		);

		let started = std::time::Instant::now();
		let err = host(&dir).git_fetch().await.unwrap_err();
		let elapsed = started.elapsed();
		assert!(matches!(err, MoonError::IoError(_)), "expected IoError, got {err:?}");
		assert!(
			elapsed < std::time::Duration::from_secs(10),
			"git_fetch took {elapsed:?} — should fail fast, not approach the 30s timeout"
		);
	}

	/// `git_branch` exposes `default_branch_remote_ref` +
	/// `default_branch_behind` so the SCM panel can render an
	/// "Update from main" affordance. Validate the happy path:
	/// a feature branch sitting on the same commit as `main`, then
	/// a third commit pushed to `origin/main` from a sibling clone.
	/// After fetch we expect `default_branch_remote_ref ==
	/// "origin/main"` and `default_branch_behind == 1`.
	#[tokio::test]
	async fn git_branch_reports_default_branch_behind_after_remote_advances() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping default-branch-behind test");
			return;
		};

		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let pusher = root.path().join("pusher");
		let local = root.path().join("local");

		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		std::fs::create_dir_all(&pusher).unwrap();
		run_git(&git, &pusher, &["init", "-q", "-b", "main"]);
		run_git(&git, &pusher, &["config", "user.email", "p@example.com"]);
		run_git(&git, &pusher, &["config", "user.name", "Pusher"]);
		run_git(&git, &pusher, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(pusher.join("README.md"), "v1\n").unwrap();
		run_git(&git, &pusher, &["add", "."]);
		run_git(&git, &pusher, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &pusher, &["push", "-q", "-u", "origin", "main"]);

		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);
		run_git(&git, &local, &["checkout", "-q", "-b", "feature"]);

		std::fs::write(pusher.join("README.md"), "v2\n").unwrap();
		run_git(&git, &pusher, &["commit", "-q", "-am", "second"]);
		run_git(&git, &pusher, &["push", "-q", "origin", "main"]);

		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		LocalHost::new(local_root.clone()).git_fetch().await.unwrap();

		let branch = LocalHost::new(local_root).git_branch().await.unwrap();
		assert_eq!(
			branch.default_branch_remote_ref.as_deref(),
			Some("origin/main"),
			"expected origin/main as the resolved default ref, got {branch:?}"
		);
		assert_eq!(
			branch.default_branch_behind, 1,
			"expected default_branch_behind=1 after fetch, got {branch:?}"
		);
		assert_eq!(branch.name.as_deref(), Some("feature"));
	}

	/// On the default branch itself the "Update from main" button
	/// must hide — `behind` (the upstream-tracking count) already
	/// surfaces the same commits via the regular Sync Changes
	/// button. We assert `default_branch_behind == 0` even though
	/// `origin/main` is a commit ahead, since reading `branch.name
	/// == "main"` strips the affordance.
	#[tokio::test]
	async fn git_branch_default_branch_behind_is_zero_when_on_default_branch() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping on-default test");
			return;
		};

		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let pusher = root.path().join("pusher");
		let local = root.path().join("local");

		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		std::fs::create_dir_all(&pusher).unwrap();
		run_git(&git, &pusher, &["init", "-q", "-b", "main"]);
		run_git(&git, &pusher, &["config", "user.email", "p@example.com"]);
		run_git(&git, &pusher, &["config", "user.name", "Pusher"]);
		run_git(&git, &pusher, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(pusher.join("README.md"), "v1\n").unwrap();
		run_git(&git, &pusher, &["add", "."]);
		run_git(&git, &pusher, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &pusher, &["push", "-q", "-u", "origin", "main"]);

		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);

		std::fs::write(pusher.join("README.md"), "v2\n").unwrap();
		run_git(&git, &pusher, &["commit", "-q", "-am", "second"]);
		run_git(&git, &pusher, &["push", "-q", "origin", "main"]);

		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		LocalHost::new(local_root.clone()).git_fetch().await.unwrap();

		let branch = LocalHost::new(local_root).git_branch().await.unwrap();
		assert_eq!(branch.name.as_deref(), Some("main"));
		assert_eq!(
			branch.default_branch_behind, 0,
			"on the default branch, Sync Changes covers the new commits — Update-from-main should hide"
		);
		assert_eq!(
			branch.behind, 1,
			"Sync Changes should still surface the upstream commit"
		);
	}

	/// `git_merge_default_branch` lands the remote default branch's
	/// commits on the current feature branch via a fast-forward (or
	/// merge commit when histories diverge). Happy path: branch
	/// off, push a commit on `origin/main`, fetch, run merge,
	/// confirm `default_branch_behind` drops to 0.
	#[tokio::test]
	async fn git_merge_default_branch_fast_forwards_local_branch() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping merge-default test");
			return;
		};

		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let pusher = root.path().join("pusher");
		let local = root.path().join("local");

		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		std::fs::create_dir_all(&pusher).unwrap();
		run_git(&git, &pusher, &["init", "-q", "-b", "main"]);
		run_git(&git, &pusher, &["config", "user.email", "p@example.com"]);
		run_git(&git, &pusher, &["config", "user.name", "Pusher"]);
		run_git(&git, &pusher, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(pusher.join("README.md"), "v1\n").unwrap();
		run_git(&git, &pusher, &["add", "."]);
		run_git(&git, &pusher, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &pusher, &["push", "-q", "-u", "origin", "main"]);

		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);
		run_git(&git, &local, &["checkout", "-q", "-b", "feature"]);

		std::fs::write(pusher.join("README.md"), "v2\n").unwrap();
		run_git(&git, &pusher, &["commit", "-q", "-am", "second"]);
		run_git(&git, &pusher, &["push", "-q", "origin", "main"]);

		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		LocalHost::new(local_root.clone()).git_fetch().await.unwrap();
		let pre = LocalHost::new(local_root.clone()).git_branch().await.unwrap();
		assert_eq!(pre.default_branch_behind, 1);

		LocalHost::new(local_root.clone())
			.git_merge_default_branch("origin/main")
			.await
			.unwrap();

		let post = LocalHost::new(local_root).git_branch().await.unwrap();
		assert_eq!(
			post.default_branch_behind, 0,
			"expected default_branch_behind to drop to 0 after merge, got {post:?}"
		);
		assert_eq!(post.name.as_deref(), Some("feature"));
	}

	/// Reproduces the `gh pr checkout <N>` shape for a fork PR
	/// whose `maintainerCanModify` is `true`: the local branch's
	/// `remote` / `pushRemote` are the fork's git URL (not a
	/// named remote) and `merge` is `refs/heads/<head>`. The
	/// local branch is named `<owner>/<head>` to avoid colliding
	/// with the upstream's default branch.
	///
	/// Asserts:
	///   - `has_upstream == true` — we read `branch.<name>.merge`
	///     directly, not via `@{u}`, so URL upstreams count.
	///   - `upstream_tracked == false` — `@{u}` doesn't resolve
	///     because no remote-tracking ref exists.
	///   - `git_push` succeeds — it uses `git push <remote-url>
	///     HEAD:<merge>`, sidestepping `push.default = simple`'s
	///     local-name ≠ merge-target-name complaint. A plain
	///     `git push` on this config would fail with `fatal: The
	///     upstream branch of your current branch does not match
	///     the name of your current branch`.
	#[tokio::test]
	async fn fork_pr_upstream_detected_and_push_uses_explicit_refspec() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping fork-PR-upstream test");
			return;
		};

		let root = TempDir::new().unwrap();
		let fork = root.path().join("fork.git");
		let local = root.path().join("local");

		// Bare fork remote with an initial commit on `master` so
		// the explicit-refspec push has a target ref to update.
		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "master", "fork.git"]);

		let seed = root.path().join("seed");
		std::fs::create_dir_all(&seed).unwrap();
		run_git(&git, &seed, &["init", "-q", "-b", "master"]);
		run_git(&git, &seed, &["config", "user.email", "s@example.com"]);
		run_git(&git, &seed, &["config", "user.name", "Seed"]);
		run_git(&git, &seed, &["remote", "add", "origin", fork.to_str().unwrap()]);
		std::fs::write(seed.join("README.md"), "v1\n").unwrap();
		run_git(&git, &seed, &["add", "."]);
		run_git(&git, &seed, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &seed, &["push", "-q", "-u", "origin", "master"]);

		// Clone the fork — stand-in for the upstream repo the
		// maintainer is reviewing the PR from. The local-side
		// clone here doesn't matter; we just need a working tree
		// with a `branch.<name>.{remote,merge,pushRemote}` config
		// that points at a URL.
		run_git(&git, root.path(), &["clone", "-q", fork.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);

		// `gh pr checkout` for a fork PR with `maintainerCanModify`:
		// fetch the head ref into a local branch named after the
		// PR author (to avoid clobbering the default branch), then
		// set the per-branch remote / pushRemote to the fork URL
		// and merge to the head ref. We skip the fetch step here
		// (the clone already has `master` checked out at the same
		// commit) and just stamp the config.
		run_git(&git, &local, &["checkout", "-q", "-b", "mmunson99/master"]);
		let fork_url = fork.to_str().unwrap();
		run_git(&git, &local, &["config", "branch.mmunson99/master.remote", fork_url]);
		run_git(
			&git,
			&local,
			&["config", "branch.mmunson99/master.pushRemote", fork_url],
		);
		run_git(
			&git,
			&local,
			&["config", "branch.mmunson99/master.merge", "refs/heads/master"],
		);

		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		let host = LocalHost::new(local_root.clone());
		let branch = host.git_branch().await.unwrap();

		assert_eq!(branch.name.as_deref(), Some("mmunson99/master"));
		assert!(
			branch.has_upstream,
			"URL-based upstream from `gh pr checkout` must count as configured, got {branch:?}"
		);
		assert!(
			!branch.upstream_tracked,
			"`@{{u}}` can't resolve a URL remote — upstream_tracked must be false, got {branch:?}"
		);
		assert_eq!(branch.ahead, 0, "no remote-tracking ref → no count, got {branch:?}");
		assert_eq!(branch.behind, 0, "no remote-tracking ref → no count, got {branch:?}");

		// Land a local commit, then push. Bare `git push` would
		// fail here with the `push.default = simple` complaint
		// because the local branch is `mmunson99/master` while
		// the merge target is `refs/heads/master`. Our `git_push`
		// uses the explicit refspec so this succeeds.
		std::fs::write(local.join("CHANGE.md"), "fork-pr edit\n").unwrap();
		run_git(&git, &local, &["add", "."]);
		run_git(&git, &local, &["commit", "-q", "-m", "fork PR edit"]);
		host
			.git_push()
			.await
			.expect("git_push must succeed for fork-PR upstream");

		// Round-trip the fork to make sure the commit actually
		// landed on `refs/heads/master` (the merge target), not
		// `refs/heads/mmunson99/master`.
		let verify = root.path().join("verify");
		run_git(
			&git,
			root.path(),
			&["clone", "-q", "-b", "master", fork.to_str().unwrap(), "verify"],
		);
		assert!(
			verify.join("CHANGE.md").exists(),
			"fork's `master` should carry the new commit after push"
		);
	}

	/// Normal `git clone` + branch-with-upstream: `@{u}` resolves
	/// to a `refs/remotes/origin/<branch>` ref, so we expose both
	/// `has_upstream == true` and `upstream_tracked == true`, and
	/// ahead / behind populate from `rev-list --left-right`.
	#[tokio::test]
	async fn named_remote_upstream_is_tracked_and_counts_work() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping named-remote-upstream test");
			return;
		};

		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let local = root.path().join("local");

		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		let seed = root.path().join("seed");
		std::fs::create_dir_all(&seed).unwrap();
		run_git(&git, &seed, &["init", "-q", "-b", "main"]);
		run_git(&git, &seed, &["config", "user.email", "s@example.com"]);
		run_git(&git, &seed, &["config", "user.name", "Seed"]);
		run_git(&git, &seed, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(seed.join("README.md"), "v1\n").unwrap();
		run_git(&git, &seed, &["add", "."]);
		run_git(&git, &seed, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &seed, &["push", "-q", "-u", "origin", "main"]);

		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);

		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		let branch = LocalHost::new(local_root).git_branch().await.unwrap();

		assert_eq!(branch.name.as_deref(), Some("main"));
		assert!(branch.has_upstream, "cloned branch has origin/main as upstream");
		assert!(
			branch.upstream_tracked,
			"named-remote upstream must be tracked, got {branch:?}"
		);
		assert!(
			!branch.upstream_foreign,
			"same-name upstream is not foreign, got {branch:?}"
		);
	}

	/// `git checkout -b feature origin/main` leaves the new branch
	/// tracking `refs/heads/main` (remote `origin`, tracked) while
	/// its own name differs. That is the *foreign* upstream shape:
	/// `git_push` must never fast-forward the shared `main`, and the
	/// SCM panel publishes the branch instead. Regression test for
	/// "push a coder-created branch and it lands on remote main".
	#[tokio::test]
	async fn branch_off_default_is_foreign_and_push_spares_main() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping foreign-upstream test");
			return;
		};

		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let local = root.path().join("local");

		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		let seed = root.path().join("seed");
		std::fs::create_dir_all(&seed).unwrap();
		run_git(&git, &seed, &["init", "-q", "-b", "main"]);
		run_git(&git, &seed, &["config", "user.email", "s@example.com"]);
		run_git(&git, &seed, &["config", "user.name", "Seed"]);
		run_git(&git, &seed, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(seed.join("README.md"), "v1\n").unwrap();
		run_git(&git, &seed, &["add", "."]);
		run_git(&git, &seed, &["commit", "-q", "-m", "initial"]);
		run_git(&git, &seed, &["push", "-q", "-u", "origin", "main"]);

		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &local, &["config", "user.email", "l@example.com"]);
		run_git(&git, &local, &["config", "user.name", "Local"]);
		// Belt-and-braces: pin the default push policy so the
		// fallback path's safety doesn't hinge on the host's git
		// global config.
		run_git(&git, &local, &["config", "push.default", "simple"]);

		// The coder's branch-creation shape: branch directly off the
		// remote-tracking default ref.
		run_git(&git, &local, &["checkout", "-q", "-b", "feature/foo", "origin/main"]);

		let local_root = Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		let host = LocalHost::new(local_root.clone());
		let branch = host.git_branch().await.unwrap();

		assert_eq!(branch.name.as_deref(), Some("feature/foo"));
		assert!(branch.has_upstream, "tracks origin/main, got {branch:?}");
		assert!(branch.upstream_tracked, "origin/main is a tracked ref, got {branch:?}");
		assert!(
			branch.upstream_foreign,
			"branch name differs from tracked upstream `main` → foreign, got {branch:?}"
		);

		// Land a local commit and capture the remote's `main` tip so
		// we can prove a push attempt never moves it.
		std::fs::write(local.join("CHANGE.md"), "feature edit\n").unwrap();
		run_git(&git, &local, &["add", "."]);
		run_git(&git, &local, &["commit", "-q", "-m", "feature work"]);
		let main_before = remote_ref_sha(&git, &remote, "refs/heads/main");

		// `git_push` must NOT update `main`. With the foreign upstream
		// it falls back to plain `git push`, which `push.default =
		// simple` rejects — so this errors, and crucially `main` is
		// untouched.
		let push = host.git_push().await;
		assert!(
			push.is_err(),
			"plain push of a foreign-upstream branch should be refused"
		);
		assert_eq!(
			remote_ref_sha(&git, &remote, "refs/heads/main"),
			main_before,
			"the feature commit must never land on remote main"
		);

		// Publishing is the right move: it creates `origin/feature/foo`
		// and re-points tracking, after which the branch is no longer
		// foreign and a normal sync works.
		host.git_publish_branch().await.expect("publish must succeed");
		assert!(
			remote_ref_sha(&git, &remote, "refs/heads/feature/foo").is_some(),
			"publish creates origin/feature/foo"
		);
		assert_eq!(
			remote_ref_sha(&git, &remote, "refs/heads/main"),
			main_before,
			"publish must not touch remote main either"
		);
		let after = host.git_branch().await.unwrap();
		assert!(
			!after.upstream_foreign,
			"after publishing, tracking points at origin/feature/foo, got {after:?}"
		);
	}

	#[test]
	fn parse_iso8601_utc_round_trips_known_timestamps() {
		assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00Z"), Some(0));
		assert_eq!(parse_iso8601_utc("2026-05-07T22:00:00Z"), Some(1_778_191_200));
		// Rejects non-Z suffixes / wrong separators / short strings.
		assert_eq!(parse_iso8601_utc(""), None);
		assert_eq!(parse_iso8601_utc("2026-05-07 22:00:00"), None);
		assert_eq!(parse_iso8601_utc("2026/05/07T22:00:00Z"), None);
	}

	#[test]
	fn format_iso8601_relative_buckets_diff_into_human_strings() {
		let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
		let from_secs_ago = |secs: u64| -> String {
			let then = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000 - secs);
			let iso = iso8601_from_unix(then.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64);
			format_iso8601_relative(&iso, now).expect("relative")
		};
		assert_eq!(from_secs_ago(0), "just now");
		assert_eq!(from_secs_ago(45), "just now");
		assert_eq!(from_secs_ago(60), "1 minute ago");
		assert_eq!(from_secs_ago(120), "2 minutes ago");
		assert_eq!(from_secs_ago(3600), "1 hour ago");
		assert_eq!(from_secs_ago(7200), "2 hours ago");
		assert_eq!(from_secs_ago(60 * 60 * 25), "yesterday");
		assert_eq!(from_secs_ago(60 * 60 * 24 * 3), "3 days ago");
		assert_eq!(from_secs_ago(60 * 60 * 24 * 8), "1 week ago");
		assert_eq!(from_secs_ago(60 * 60 * 24 * 35), "1 month ago");
		assert_eq!(from_secs_ago(60 * 60 * 24 * 400), "1 year ago");
		// Future timestamps reject — we don't render "in 3 hours" and
		// the caller treats `None` as "no relative form".
		let future_iso = iso8601_from_unix(1_780_001_000);
		assert_eq!(format_iso8601_relative(&future_iso, now), None);
	}

	fn iso8601_from_unix(secs: i64) -> String {
		// Tiny inverse of `parse_iso8601_utc` for test fixtures.
		// Uses chrono-free arithmetic via `time` crate API would be
		// nicer but we don't pull a crate just for tests; the
		// roundtrip below is checked against the real parser.
		let days = secs.div_euclid(86_400);
		let time = secs.rem_euclid(86_400);
		let (y, m, d) = civil_from_days(days);
		let hour = time / 3600;
		let min = (time % 3600) / 60;
		let sec = time % 60;
		format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
	}

	fn civil_from_days(z: i64) -> (i64, u32, u32) {
		let z = z + 719_468;
		let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
		let doe = (z - era * 146_097) as u32;
		let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
		let y = yoe as i64 + era * 400;
		let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
		let mp = (5 * doy + 2) / 153;
		let d = doy - (153 * mp + 2) / 5 + 1;
		let m = if mp < 10 { mp + 3 } else { mp - 9 };
		let y = if m <= 2 { y + 1 } else { y };
		(y, m, d)
	}

	#[test]
	fn parse_gh_pr_list_extracts_rows_and_skips_malformed() {
		let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
		let json = br#"[
			{
				"number": 42,
				"title": "Add cool feature",
				"headRefName": "feat/cool",
				"isDraft": false,
				"updatedAt": "2026-05-07T22:00:00Z",
				"author": { "login": "ada" }
			},
			{
				"number": 7,
				"title": "WIP: shave yak",
				"headRefName": "wip/yak",
				"isDraft": true,
				"updatedAt": "2026-05-06T22:00:00Z",
				"author": { "login": "lovelace" }
			},
			{
				"title": "missing number - skipped",
				"headRefName": "x"
			}
		]"#;

		let rows = parse_gh_pr_list(json, now);
		assert_eq!(rows.len(), 2, "expected the two well-formed rows");
		match &rows[0].0 {
			BranchListEntry::Pr {
				number,
				title,
				author,
				head_ref,
				is_draft,
				..
			} => {
				assert_eq!(*number, 42);
				assert_eq!(title, "Add cool feature");
				assert_eq!(author, "ada");
				assert_eq!(head_ref, "feat/cool");
				assert!(!is_draft);
			}
			_ => panic!("expected Pr entry"),
		}
		match &rows[1].0 {
			BranchListEntry::Pr { number, is_draft, .. } => {
				assert_eq!(*number, 7);
				assert!(*is_draft);
			}
			_ => panic!("expected Pr entry"),
		}
		// updatedAt timestamps come back parsed for the merging
		// path in `Participating` to sort by — the well-formed
		// row should be a real unix-second value.
		assert!(rows[0].1.is_some());
	}

	#[test]
	fn is_safe_rev_accepts_head_and_40_char_hex() {
		assert!(is_safe_rev("HEAD"));
		// Lowercase hex (the shape `git rev-parse` emits).
		assert!(is_safe_rev("0123456789abcdef0123456789abcdef01234567"));
		// Uppercase hex — `--` etc. on the path are still safe;
		// callers stick to lowercase but we accept both.
		assert!(is_safe_rev("0123456789ABCDEF0123456789ABCDEF01234567"));
		// Wrong length, non-hex, flag-shaped, branch name.
		assert!(!is_safe_rev(""));
		assert!(!is_safe_rev("head"));
		assert!(!is_safe_rev("main"));
		assert!(!is_safe_rev("origin/main"));
		assert!(!is_safe_rev("--upload-pack=evil"));
		assert!(!is_safe_rev("0123456789abcdef")); // too short
		assert!(!is_safe_rev("0123456789abcdef0123456789abcdef0123456g")); // non-hex
	}

	#[test]
	fn parse_diff_name_status_z_maps_status_bytes_and_skips_unknowns() {
		// `git diff --name-status -z --no-renames` shape: each
		// field (status, path) is NUL-terminated, so a record is
		// `<status>\0<path>\0`. Mix in an unknown byte (`X`) to
		// confirm the parser drops it instead of poisoning the
		// row.
		let raw: &[u8] = b"M\0src/lib.rs\0A\0src/new.rs\0D\0src/gone.rs\0T\0src/typechange.rs\0X\0noise\0";
		let entries = parse_diff_name_status_z(raw);
		assert_eq!(entries.len(), 4);
		assert_eq!(entries[0].path, "src/lib.rs");
		assert!(matches!(entries[0].status, GitFileStatus::Modified));
		assert_eq!(entries[1].path, "src/new.rs");
		assert!(matches!(entries[1].status, GitFileStatus::Added));
		assert_eq!(entries[2].path, "src/gone.rs");
		assert!(matches!(entries[2].status, GitFileStatus::Deleted));
		// Typechange folds into Modified — same surface as the
		// porcelain pipeline.
		assert_eq!(entries[3].path, "src/typechange.rs");
		assert!(matches!(entries[3].status, GitFileStatus::Modified));
	}

	#[test]
	fn parse_diff_name_status_z_handles_empty_and_malformed_input() {
		assert!(parse_diff_name_status_z(b"").is_empty());
		// Missing terminating NUL on the path — drop the trailing
		// partial record.
		assert!(parse_diff_name_status_z(b"M\0src/lib.rs").is_empty());
	}

	#[test]
	fn map_porcelain_status_recognises_every_unmerged_combination() {
		// All seven `git status --porcelain` unmerged codes (see
		// `gitstatus(5)`'s "Short Format" table — the unmerged
		// section). Each one must map to `Conflicted` so the SCM
		// panel can shift into merge-in-progress mode and the
		// row gets the conflict badge.
		for (x, y) in [
			('U', 'U'),
			('A', 'U'),
			('U', 'A'),
			('D', 'D'),
			('A', 'A'),
			('U', 'D'),
			('D', 'U'),
		] {
			assert_eq!(
				map_porcelain_status(x, y),
				Some(GitFileStatus::Conflicted),
				"unmerged code `{x}{y}` should map to Conflicted",
			);
		}
	}

	#[test]
	fn map_porcelain_status_preserves_existing_priority_for_non_conflict_codes() {
		// Regression guard: the conflict arm is intentionally
		// first, so a stale `D` / `A` code on one side of an
		// unmerged pair doesn't shadow it. But the regular
		// (non-unmerged) priorities must keep working — these
		// are the rows the file tree paints today.
		assert_eq!(map_porcelain_status('M', ' '), Some(GitFileStatus::Modified));
		assert_eq!(map_porcelain_status(' ', 'M'), Some(GitFileStatus::Modified));
		assert_eq!(map_porcelain_status('A', ' '), Some(GitFileStatus::Added));
		assert_eq!(map_porcelain_status('D', ' '), Some(GitFileStatus::Deleted));
		assert_eq!(map_porcelain_status(' ', 'D'), Some(GitFileStatus::Deleted));
		assert_eq!(map_porcelain_status('?', '?'), Some(GitFileStatus::Untracked));
		assert_eq!(map_porcelain_status('!', '!'), Some(GitFileStatus::Ignored));
		// `C` (copy) and the catch-all stay dropped — same
		// pre-conflict-feature behaviour.
		assert_eq!(map_porcelain_status('C', ' '), None);
		assert_eq!(map_porcelain_status(' ', ' '), None);
	}

	#[tokio::test]
	async fn git_merge_state_returns_default_when_no_merge_in_progress() {
		// Clean repo, no `MERGE_HEAD` — the trait method must
		// return `in_progress: false` with every other field at
		// its default. Same shape the SCM panel uses to decide
		// "merge-in-progress mode is off".
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_merge_state default test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
		run_git(&git, dir.path(), &["add", "README.md"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		let state = host(&dir).git_merge_state().await.unwrap();
		assert!(!state.in_progress, "no MERGE_HEAD → in_progress must be false");
		assert!(state.merging_ref.is_none());
		assert!(state.default_message.is_none());
		assert!(state.unmerged_paths.is_empty());
	}

	#[tokio::test]
	async fn git_merge_state_surfaces_in_progress_merge_with_unmerged_paths() {
		// End-to-end probe of the merge-state pipeline:
		//   1. Set up a repo with two branches that touch the
		//      same line of the same file. Merging one into the
		//      other produces a textbook content conflict.
		//   2. After the failed merge, `.git/MERGE_HEAD` should
		//      exist, `MERGE_MSG` should be populated, and
		//      `ls-files --unmerged` should report `conflict.txt`.
		//   3. `map_porcelain_status` should classify
		//      `conflict.txt` as `Conflicted` — confirms the
		//      status pipeline and the merge-state pipeline
		//      agree on the same row.
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_merge_state conflict test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		std::fs::write(dir.path().join("conflict.txt"), "base\n").unwrap();
		run_git(&git, dir.path(), &["add", "conflict.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "base"]);
		run_git(&git, dir.path(), &["switch", "-c", "feature"]);
		std::fs::write(dir.path().join("conflict.txt"), "feature\n").unwrap();
		run_git(&git, dir.path(), &["commit", "-q", "-am", "feature edit"]);
		run_git(&git, dir.path(), &["switch", "main"]);
		std::fs::write(dir.path().join("conflict.txt"), "mainline\n").unwrap();
		run_git(&git, dir.path(), &["commit", "-q", "-am", "main edit"]);

		// Now merge feature → main. Expected to fail with a
		// content conflict, leaving `.git/MERGE_HEAD` behind.
		let merge_failed = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["merge", "--no-edit", "feature"])
			.status()
			.unwrap();
		assert!(!merge_failed.success(), "merge should have hit a conflict");

		let state = host(&dir).git_merge_state().await.unwrap();
		assert!(state.in_progress, "MERGE_HEAD must exist after a conflicted merge");
		assert!(
			state.merging_ref.as_deref() == Some("feature") || state.merging_ref.as_deref() == Some("refs/heads/feature"),
			"merging_ref should resolve to the feature branch; got {:?}",
			state.merging_ref,
		);
		assert!(
			state.default_message.as_deref().unwrap_or("").contains("feature"),
			"MERGE_MSG should mention the feature branch; got {:?}",
			state.default_message,
		);
		assert_eq!(
			state.unmerged_paths,
			vec!["conflict.txt".to_string()],
			"the conflicted file should be the only unmerged path",
		);

		// And the porcelain pipeline should agree.
		let entries = host(&dir).git_status_entries(&[]).await.unwrap();
		let conflicted_paths: Vec<_> = entries
			.iter()
			.filter(|e| matches!(e.status, GitFileStatus::Conflicted))
			.map(|e| e.path.clone())
			.collect();
		assert_eq!(conflicted_paths, vec!["conflict.txt".to_string()]);
	}

	#[tokio::test]
	async fn git_merge_abort_clears_merge_state() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping git_merge_abort test");
			return;
		};
		let dir = TempDir::new().unwrap();
		run_git(&git, dir.path(), &["init", "-q", "-b", "main"]);
		run_git(&git, dir.path(), &["config", "user.email", "test@example.com"]);
		run_git(&git, dir.path(), &["config", "user.name", "test"]);
		std::fs::write(dir.path().join("conflict.txt"), "base\n").unwrap();
		run_git(&git, dir.path(), &["add", "conflict.txt"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "base"]);
		run_git(&git, dir.path(), &["switch", "-c", "feature"]);
		std::fs::write(dir.path().join("conflict.txt"), "feature\n").unwrap();
		run_git(&git, dir.path(), &["commit", "-q", "-am", "feature edit"]);
		run_git(&git, dir.path(), &["switch", "main"]);
		std::fs::write(dir.path().join("conflict.txt"), "mainline\n").unwrap();
		run_git(&git, dir.path(), &["commit", "-q", "-am", "main edit"]);
		let _ = std::process::Command::new(&git)
			.arg("-C")
			.arg(dir.path())
			.args(["merge", "--no-edit", "feature"])
			.status()
			.unwrap();

		host(&dir).git_merge_abort().await.unwrap();
		let state = host(&dir).git_merge_state().await.unwrap();
		assert!(
			!state.in_progress,
			"after `git merge --abort`, MERGE_HEAD should be gone"
		);
		assert!(state.unmerged_paths.is_empty());
		// Worktree was restored to `main`'s pre-merge content.
		assert_eq!(
			std::fs::read_to_string(dir.path().join("conflict.txt")).unwrap(),
			"mainline\n",
		);
	}

	#[tokio::test]
	async fn git_default_branch_diff_returns_committed_and_uncommitted_changes() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping default-branch diff test");
			return;
		};
		// Two repos: a bare "remote" that we treat as `origin`,
		// and a clone we run the diff inside.
		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let clone = root.path().join("local");
		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		// Seed `main` on the remote with one commit.
		let seeder = root.path().join("seeder");
		std::fs::create_dir_all(&seeder).unwrap();
		run_git(&git, &seeder, &["init", "-q", "-b", "main"]);
		run_git(&git, &seeder, &["config", "user.email", "s@example.com"]);
		run_git(&git, &seeder, &["config", "user.name", "Seeder"]);
		run_git(&git, &seeder, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(seeder.join("a.rs"), "fn a() {}\n").unwrap();
		std::fs::write(seeder.join("b.rs"), "fn b() {}\n").unwrap();
		run_git(&git, &seeder, &["add", "."]);
		run_git(&git, &seeder, &["commit", "-q", "-m", "main: initial"]);
		run_git(&git, &seeder, &["push", "-q", "-u", "origin", "main"]);
		// Clone the remote; that's where the test runs.
		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);
		run_git(&git, &clone, &["config", "user.email", "l@example.com"]);
		run_git(&git, &clone, &["config", "user.name", "Local"]);
		// On a feature branch:
		// - commit an addition (`new.rs`)
		// - commit a deletion (`b.rs`)
		// Then leave one uncommitted modification (`a.rs`) in
		// the working tree. All three should appear in the diff.
		run_git(&git, &clone, &["checkout", "-q", "-b", "feat/branch-diff"]);
		std::fs::write(clone.join("new.rs"), "fn new() {}\n").unwrap();
		run_git(&git, &clone, &["add", "."]);
		run_git(&git, &clone, &["commit", "-q", "-m", "add new.rs"]);
		std::fs::remove_file(clone.join("b.rs")).unwrap();
		run_git(&git, &clone, &["add", "-A"]);
		run_git(&git, &clone, &["commit", "-q", "-m", "rm b.rs"]);
		std::fs::write(clone.join("a.rs"), "fn a() { todo!() }\n").unwrap();

		let utf8 = Utf8PathBuf::from_path_buf(clone.canonicalize().unwrap()).unwrap();
		let result = LocalHost::new(utf8).git_default_branch_diff().await.unwrap();
		let Some(diff) = result else {
			panic!("expected Some(BranchDiffStatus); got None");
		};
		assert_eq!(diff.default_branch_ref, "origin/main");
		assert_eq!(diff.merge_base.len(), 40, "merge_base must be a 40-char SHA");
		// Map by path so order doesn't matter.
		let by_path: std::collections::HashMap<&str, GitFileStatus> =
			diff.entries.iter().map(|e| (e.path.as_str(), e.status)).collect();
		assert_eq!(by_path.get("new.rs"), Some(&GitFileStatus::Added));
		assert_eq!(by_path.get("b.rs"), Some(&GitFileStatus::Deleted));
		assert_eq!(by_path.get("a.rs"), Some(&GitFileStatus::Modified));
		assert_eq!(by_path.len(), 3, "no untracked / unrelated rows expected: {by_path:?}");
	}

	#[tokio::test]
	async fn git_default_branch_diff_returns_none_when_on_default_branch() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping default-branch on-main test");
			return;
		};
		let root = TempDir::new().unwrap();
		let remote = root.path().join("remote.git");
		let clone = root.path().join("local");
		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "remote.git"]);
		let seeder = root.path().join("seeder");
		std::fs::create_dir_all(&seeder).unwrap();
		run_git(&git, &seeder, &["init", "-q", "-b", "main"]);
		run_git(&git, &seeder, &["config", "user.email", "s@example.com"]);
		run_git(&git, &seeder, &["config", "user.name", "Seeder"]);
		run_git(&git, &seeder, &["remote", "add", "origin", remote.to_str().unwrap()]);
		std::fs::write(seeder.join("a"), "1").unwrap();
		run_git(&git, &seeder, &["add", "."]);
		run_git(&git, &seeder, &["commit", "-q", "-m", "first"]);
		run_git(&git, &seeder, &["push", "-q", "-u", "origin", "main"]);
		run_git(&git, root.path(), &["clone", "-q", remote.to_str().unwrap(), "local"]);

		let utf8 = Utf8PathBuf::from_path_buf(clone.canonicalize().unwrap()).unwrap();
		// Active branch is `main` (the default). The toggle would
		// be confusing here, so the host returns `None` and the
		// frontend silently keeps `Head` mode.
		let result = LocalHost::new(utf8).git_default_branch_diff().await.unwrap();
		assert!(result.is_none(), "expected None when HEAD is on the default branch");
	}

	#[test]
	fn parse_gh_pr_list_returns_empty_on_garbage() {
		let now = SystemTime::UNIX_EPOCH;
		assert!(parse_gh_pr_list(b"", now).is_empty());
		assert!(parse_gh_pr_list(b"not json", now).is_empty());
		// JSON but not an array — gh shouldn't ever produce this,
		// but we tolerate without panicking.
		assert!(parse_gh_pr_list(br#"{"oops": true}"#, now).is_empty());
	}

	#[test]
	fn parse_gh_pr_url_extracts_first_url_or_returns_none() {
		// Empty array (no PR for this head) — the `Ok(None)` path.
		assert_eq!(parse_gh_pr_url(b"[]"), None);
		// Single hit — what `--limit 1` produces when a PR exists.
		assert_eq!(
			parse_gh_pr_url(br#"[{"url": "https://github.com/owner/repo/pull/42"}]"#),
			Some("https://github.com/owner/repo/pull/42".to_owned()),
		);
		// Multi-element (defensive, gh shouldn't return >1 with
		// `--limit 1`) — first wins.
		assert_eq!(
			parse_gh_pr_url(br#"[{"url": "https://a/1"}, {"url": "https://b/2"}]"#),
			Some("https://a/1".to_owned()),
		);
		// Empty `url` string collapses to `None` rather than
		// returning an unusable href.
		assert_eq!(parse_gh_pr_url(br#"[{"url": ""}]"#), None);
		// Garbage / missing fields stay `None`.
		assert_eq!(parse_gh_pr_url(b""), None);
		assert_eq!(parse_gh_pr_url(b"not json"), None);
		assert_eq!(parse_gh_pr_url(br#"[{"notUrl": "x"}]"#), None);
		assert_eq!(parse_gh_pr_url(br#"{"oops": true}"#), None);
	}

	#[tokio::test]
	async fn branch_list_local_orders_by_committer_date_with_current_marker() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping branch_list local test");
			return;
		};

		let root = TempDir::new().unwrap();
		let repo = root.path();
		run_git(&git, repo, &["init", "-q", "-b", "main"]);
		run_git(&git, repo, &["config", "user.email", "t@example.com"]);
		run_git(&git, repo, &["config", "user.name", "Tester"]);
		std::fs::write(repo.join("README.md"), "v1\n").unwrap();
		run_git(&git, repo, &["add", "."]);
		run_git(&git, repo, &["commit", "-q", "-m", "first commit"]);

		// Seed two extra branches at separate commits so the
		// `--sort=-committerdate` order is observable.
		run_git(&git, repo, &["checkout", "-q", "-b", "feat/older"]);
		std::fs::write(repo.join("a"), "1").unwrap();
		run_git(&git, repo, &["add", "."]);
		run_git(&git, repo, &["commit", "-q", "-m", "older work"]);
		run_git(&git, repo, &["checkout", "-q", "main"]);
		run_git(&git, repo, &["checkout", "-q", "-b", "feat/newer"]);
		std::fs::write(repo.join("b"), "2").unwrap();
		run_git(&git, repo, &["add", "."]);
		run_git(&git, repo, &["commit", "-q", "-m", "newer work"]);

		let utf8 = Utf8PathBuf::from_path_buf(repo.canonicalize().unwrap()).unwrap();
		let result = LocalHost::new(utf8).branch_list(PrListScope::All).await.unwrap();

		// Newer first, older last. Current branch is `feat/newer`
		// (we never switched away after the second checkout).
		let names: Vec<&str> = result
			.local
			.iter()
			.map(|e| match e {
				BranchListEntry::Local { name, .. } => name.as_str(),
				_ => panic!("expected local entries"),
			})
			.collect();
		assert_eq!(names, vec!["feat/newer", "feat/older", "main"]);

		let current_count = result
			.local
			.iter()
			.filter(|e| matches!(e, BranchListEntry::Local { is_current: true, .. }))
			.count();
		assert_eq!(current_count, 1);
		assert!(matches!(
			&result.local[0],
			BranchListEntry::Local { is_current: true, name, .. } if name == "feat/newer"
		));

		// No remote configured → not a GitHub repo, so the PR
		// section is suppressed without contacting gh.
		assert!(matches!(result.pr_status, PrListStatus::NotGithub));
		assert!(result.prs.is_empty());
	}

	#[tokio::test]
	async fn branch_list_local_always_includes_default_branch_past_cap() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping default-branch cap test");
			return;
		};

		let root = TempDir::new().unwrap();
		let repo = root.path();
		run_git(&git, repo, &["init", "-q", "-b", "main"]);
		run_git(&git, repo, &["config", "user.email", "t@example.com"]);
		run_git(&git, repo, &["config", "user.name", "Tester"]);
		std::fs::write(repo.join("README.md"), "v1\n").unwrap();
		run_git(&git, repo, &["add", "."]);
		run_git(&git, repo, &["commit", "-q", "-m", "first commit"]);

		// Point `origin/HEAD` at `main` without a real remote: a
		// symbolic ref under `refs/remotes/origin/HEAD` is exactly
		// what `git remote set-head` writes, and it's all
		// `resolve_default_remote_ref` reads.
		run_git(&git, repo, &["update-ref", "refs/remotes/origin/main", "main"]);
		run_git(
			&git,
			repo,
			&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"],
		);

		// Create more than the cap of fresher branches so `main`
		// (committed first, hence oldest) falls off the recency
		// window. Each gets its own commit so committer dates differ.
		let extra = BRANCH_LIST_LOCAL_CAP + 5;
		for i in 0..extra {
			let name = format!("feat/{i:03}");
			run_git(&git, repo, &["checkout", "-q", "-b", &name, "main"]);
			std::fs::write(repo.join(format!("f{i}")), "x").unwrap();
			run_git(&git, repo, &["add", "."]);
			run_git(&git, repo, &["commit", "-q", "-m", &format!("work {i}")]);
		}
		// Land on a feature branch so `main` is neither current nor
		// near the top of the recency sort.
		run_git(&git, repo, &["checkout", "-q", "feat/000"]);

		let utf8 = Utf8PathBuf::from_path_buf(repo.canonicalize().unwrap()).unwrap();
		let result = LocalHost::new(utf8).branch_list(PrListScope::All).await.unwrap();

		// `main` survived the cap and carries the default marker.
		let main_row = result
			.local
			.iter()
			.find(|e| matches!(e, BranchListEntry::Local { name, .. } if name == "main"));
		assert!(
			matches!(main_row, Some(BranchListEntry::Local { is_default: true, .. })),
			"main should be present and flagged is_default, got {main_row:?}",
		);
		// Exactly one row is the default.
		let default_count = result
			.local
			.iter()
			.filter(|e| matches!(e, BranchListEntry::Local { is_default: true, .. }))
			.count();
		assert_eq!(default_count, 1);
	}

	#[tokio::test]
	async fn branch_switch_local_moves_head_to_target() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping branch_switch test");
			return;
		};

		let root = TempDir::new().unwrap();
		let repo = root.path();
		run_git(&git, repo, &["init", "-q", "-b", "main"]);
		run_git(&git, repo, &["config", "user.email", "t@example.com"]);
		run_git(&git, repo, &["config", "user.name", "Tester"]);
		std::fs::write(repo.join("README.md"), "v1\n").unwrap();
		run_git(&git, repo, &["add", "."]);
		run_git(&git, repo, &["commit", "-q", "-m", "first commit"]);
		run_git(&git, repo, &["branch", "feat/two"]);

		let utf8 = Utf8PathBuf::from_path_buf(repo.canonicalize().unwrap()).unwrap();
		LocalHost::new(utf8.clone())
			.branch_switch(&BranchSwitchTarget::Local {
				name: "feat/two".into(),
			})
			.await
			.unwrap();

		let info = LocalHost::new(utf8.clone()).git_branch().await.unwrap();
		assert_eq!(info.name.as_deref(), Some("feat/two"));

		// Empty name fails fast with a clear message — no `git
		// switch ` ever fires.
		let err = LocalHost::new(utf8)
			.branch_switch(&BranchSwitchTarget::Local { name: "  ".into() })
			.await
			.unwrap_err();
		match err {
			MoonError::InvalidArgument(msg) => assert!(msg.contains("branch name")),
			other => panic!("expected InvalidArgument, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn branch_list_pr_section_signals_not_github_for_non_github_remote() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping non-github remote test");
			return;
		};
		let root = TempDir::new().unwrap();
		let repo = root.path();
		run_git(&git, repo, &["init", "-q", "-b", "main"]);
		run_git(&git, repo, &["config", "user.email", "t@example.com"]);
		run_git(&git, repo, &["config", "user.name", "Tester"]);
		std::fs::write(repo.join("README.md"), "v1\n").unwrap();
		run_git(&git, repo, &["add", "."]);
		run_git(&git, repo, &["commit", "-q", "-m", "first"]);
		run_git(
			&git,
			repo,
			&["remote", "add", "origin", "git@gitlab.com:owner/repo.git"],
		);

		let utf8 = Utf8PathBuf::from_path_buf(repo.canonicalize().unwrap()).unwrap();
		let result = LocalHost::new(utf8).branch_list(PrListScope::All).await.unwrap();
		// A non-GitHub remote short-circuits before we ever spawn
		// `gh`, regardless of whether gh is installed.
		assert!(
			matches!(result.pr_status, PrListStatus::NotGithub),
			"expected NotGithub, got {:?}",
			result.pr_status
		);
		assert!(result.prs.is_empty());
	}

	#[cfg(unix)]
	fn write_executable_script(path: &std::path::Path, body: &str) {
		use std::os::unix::fs::PermissionsExt;
		std::fs::write(path, body).unwrap();
		std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
	}

	/// Whole save pipeline against an arbitrary lint-staged command —
	/// validates that `save_file` runs commands lint-staged-style (file
	/// path appended, command mutates file in place) for tools that
	/// were never in the old `KnownTool` allow-list.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_runs_arbitrary_lint_staged_command() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		write_executable_script(
			&dir.path().join("uppercase.sh"),
			"#!/bin/sh\ntr '[:lower:]' '[:upper:]' < \"$1\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
		);
		std::fs::write(
			dir.path().join(".lintstagedrc.json"),
			r#"{ "*.txt": "./uppercase.sh" }"#,
		)
		.unwrap();

		let result = h.save_file(Utf8Path::new("a.txt"), "hello world").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
		assert_eq!(on_disk.trim_end(), "HELLO WORLD");
		assert_eq!(result.bytes_written, on_disk.len() as u64);
	}

	/// Every command in a chain runs in order, each seeing the previous
	/// one's on-disk output. Verified by chaining a "prepend marker"
	/// script and an "uppercase" script — only the combined output
	/// (marker + uppercase'd input, all in upper case) is possible if
	/// both ran in sequence.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_runs_every_command_in_chain_in_order() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		write_executable_script(
			&dir.path().join("first.sh"),
			"#!/bin/sh\nprintf 'prefix:' > \"$1.tmp\" && cat \"$1\" >> \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
		);
		write_executable_script(
			&dir.path().join("upper.sh"),
			"#!/bin/sh\ntr '[:lower:]' '[:upper:]' < \"$1\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
		);
		std::fs::write(
			dir.path().join(".lintstagedrc.json"),
			r#"{ "*.txt": ["./first.sh", "./upper.sh"] }"#,
		)
		.unwrap();

		h.save_file(Utf8Path::new("a.txt"), "hello world").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
		// `first.sh` prepended `prefix:`, then `upper.sh` ran on the
		// combined output and upper-cased everything. The presence of
		// `PREFIX:` proves the first command ran; `HELLO WORLD` proves
		// the second ran after it on the prefixed bytes.
		assert!(
			on_disk.contains("PREFIX:HELLO WORLD"),
			"expected both scripts to run in order, got: {on_disk:?}"
		);
	}

	/// A failing command mid-chain doesn't abort the rest: the trailing
	/// formatter still runs on whatever bytes are on disk. Verified by
	/// chaining a failing script (which leaves the file untouched) and
	/// an "uppercase" formatter; the file should land upper-cased even
	/// though the first step exited non-zero.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_chain_continues_past_failing_command() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		write_executable_script(&dir.path().join("fail.sh"), "#!/bin/sh\nexit 1\n");
		write_executable_script(
			&dir.path().join("upper.sh"),
			"#!/bin/sh\ntr '[:lower:]' '[:upper:]' < \"$1\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
		);
		std::fs::write(
			dir.path().join(".lintstagedrc.json"),
			r#"{ "*.txt": ["./fail.sh", "./upper.sh"] }"#,
		)
		.unwrap();

		h.save_file(Utf8Path::new("a.txt"), "hello world").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
		// `upper.sh` ran after `fail.sh` exited 1, so the bytes are
		// the editorconfig-normalised input upper-cased.
		assert!(
			on_disk.contains("HELLO WORLD"),
			"expected upper.sh to run after fail.sh, got: {on_disk:?}"
		);
	}

	/// Failure of a single (non-chained) command is non-fatal: the
	/// editorconfig-normalised bytes stay on disk and save still
	/// returns success.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_keeps_normalised_text_when_command_fails() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		write_executable_script(&dir.path().join("fail.sh"), "#!/bin/sh\nexit 1\n");
		std::fs::write(dir.path().join(".lintstagedrc.json"), r#"{ "*.txt": "./fail.sh" }"#).unwrap();

		h.save_file(Utf8Path::new("a.txt"), "input  ").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
		// editorconfig stripped the trailing whitespace and added the
		// final newline; the failing formatter didn't get to mutate
		// further.
		assert_eq!(on_disk, "input\n");
	}

	/// No matching lint-staged rule → editorconfig pipeline still runs
	/// (final newline ensured, trailing whitespace stripped).
	#[tokio::test]
	async fn save_file_falls_back_to_editorconfig_when_no_rule() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		// No `.lintstagedrc.json`, so `match_commands` returns None.
		h.save_file(Utf8Path::new("a.txt"), "hello   \nworld\t").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
		assert_eq!(on_disk, "hello\nworld\n");
	}

	/// No matching lint-staged rule but the file extension has a
	/// language-default formatter entry → the fallback runs. Validates
	/// that `~/code/workloads`-style projects (pure Rust, no
	/// `.lintstagedrc.json`) get format-on-save without needing a
	/// per-repo lint-staged config.
	///
	/// `rustfmt` itself can't be assumed in CI, so we drop a fake one
	/// in `node_modules/.bin/` — `build_path_env` prepends that
	/// directory to the formatter subprocess's `PATH`, so spawning
	/// `rustfmt` resolves to this script before any system install.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_falls_back_to_default_formatter_when_extension_has_one() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		let bin = dir.path().join("node_modules").join(".bin");
		std::fs::create_dir_all(&bin).unwrap();
		// The fallback now emits `rustfmt --edition <e> <path>`,
		// so the file path is the last positional. POSIX `for`
		// loop walks all args and the trailing `:` keeps the
		// last one in `$f`.
		write_executable_script(
			&bin.join("rustfmt"),
			"#!/bin/sh\nfor f in \"$@\"; do :; done\ntr '[:lower:]' '[:upper:]' < \"$f\" > \"$f.tmp\" && mv \"$f.tmp\" \"$f\"\n",
		);

		let result = h.save_file(Utf8Path::new("a.rs"), "hello\n").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.rs")).unwrap();
		assert_eq!(on_disk.trim_end(), "HELLO");
		assert_eq!(result.bytes_written, on_disk.len() as u64);
	}

	/// Container routing: when a `ShellResolver` returns
	/// `ShellTarget::Container`, format-on-save spawns
	/// `docker exec -w <translated_cwd> <name> <bin> <translated_abs>`
	/// instead of running the binary on the host. We can't ask CI
	/// for a real container, so we point the resolver at a fake
	/// `docker` script (dropped on the formatter subprocess's
	/// `PATH` via `node_modules/.bin/` prepend) that records its
	/// argv to a sidecar and exits 0 without mutating the file.
	/// This validates path translation + invocation shape
	/// end-to-end without bringing docker into the test bus.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_routes_format_through_docker_exec_in_container_target() {
		use crate::shell::{ShellResolver, ShellResolverHandle, ShellTarget};

		let dir = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();

		let argv_log = root.join("docker.argv");
		let bin = dir.path().join("node_modules").join(".bin");
		std::fs::create_dir_all(&bin).unwrap();
		write_executable_script(
			&bin.join("docker"),
			&format!("#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > {argv_log}\n"),
		);

		// Stub resolver: always container, with the bind mount
		// rooted at this temp dir → /workspace/<basename>.
		struct StubResolver {
			host_root: Utf8PathBuf,
			server_root: Utf8PathBuf,
		}
		#[async_trait::async_trait]
		impl ShellResolver for StubResolver {
			async fn resolve(&self, _host_root: &Utf8Path) -> ShellTarget {
				ShellTarget::Container {
					container_name: "moon-ws-test-dev-1".into(),
					host_root: self.host_root.clone(),
					server_root: self.server_root.clone(),
				}
			}
		}
		let basename = root.file_name().unwrap_or("workspace").to_string();
		let resolver = ShellResolverHandle::new(std::sync::Arc::new(StubResolver {
			host_root: root.clone(),
			server_root: Utf8PathBuf::from(format!("/workspace/{basename}")),
		}));

		// Drop a Cargo.toml so the rustfmt fallback emits the
		// detected `--edition` flag — same wire shape the user
		// will see in `~/code/workloads`.
		std::fs::write(
			dir.path().join("Cargo.toml"),
			"[package]\nname = \"x\"\nedition = \"2024\"\n",
		)
		.unwrap();

		let host = LocalHost::new(root.clone()).with_shell_resolver(resolver);
		host.save_file(Utf8Path::new("a.rs"), "hello\n").await.unwrap();

		// Argv verifies the wire shape: `exec -w <translated_cwd>
		// <name> sh -c '<wrap>' sh rustfmt --edition 2024
		// <translated_abs>`, where the `sh -c` wrapper prepends
		// the bind-mount-translated `node_modules/.bin` chain to
		// `$PATH` so project-local binaries resolve. No `-it`
		// (captured I/O). See ADR 0013.
		let argv = std::fs::read_to_string(argv_log.as_std_path()).unwrap();
		let lines: Vec<&str> = argv.lines().collect();
		assert_eq!(
			lines,
			vec![
				"exec",
				"-w",
				format!("/workspace/{basename}").as_str(),
				"moon-ws-test-dev-1",
				"sh",
				"-c",
				format!(r#"PATH="/workspace/{basename}/node_modules/.bin:$PATH" exec "$@""#).as_str(),
				"sh",
				"rustfmt",
				"--edition",
				"2024",
				format!("/workspace/{basename}/a.rs").as_str(),
			]
		);
	}

	/// Lint-staged still wins over the language-default fallback. With
	/// a `.lintstagedrc.json` that maps `*.rs` to a marker script and
	/// a fake `rustfmt` on PATH, only the marker should run — proving
	/// that adding a default-formatter row never overrides an explicit
	/// team config.
	#[cfg(unix)]
	#[tokio::test]
	async fn save_file_lint_staged_wins_over_default_formatter() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		write_executable_script(
			&dir.path().join("marker.sh"),
			"#!/bin/sh\nprintf 'lint-staged-ran' > \"$1\"\n",
		);
		std::fs::write(dir.path().join(".lintstagedrc.json"), r#"{ "*.rs": "./marker.sh" }"#).unwrap();

		// Fake rustfmt that would clobber the file with a different
		// marker — must NOT run because lint-staged matched first.
		// `--edition <e>` lands in `$1..$2`, so reach for the
		// last positional with the same `for` trick the
		// fallback test uses.
		let bin = dir.path().join("node_modules").join(".bin");
		std::fs::create_dir_all(&bin).unwrap();
		write_executable_script(
			&bin.join("rustfmt"),
			"#!/bin/sh\nfor f in \"$@\"; do :; done\nprintf 'rustfmt-ran' > \"$f\"\n",
		);

		h.save_file(Utf8Path::new("a.rs"), "hello\n").await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.rs")).unwrap();
		assert!(
			on_disk.contains("lint-staged-ran"),
			"lint-staged should have won; got: {on_disk:?}"
		);
		assert!(
			!on_disk.contains("rustfmt-ran"),
			"rustfmt fallback should not have fired; got: {on_disk:?}"
		);
	}

	/// `format_file` is the coder's turn-end flush entry point: bytes
	/// are already on disk (the agent's `write_file` / `edit_file`
	/// tool landed them raw, with `host.write_file`), and we want
	/// the same shape `save_file` would land if the user had typed
	/// the file and hit `Ctrl+S`. Verify the chain runs and the
	/// editorconfig pre-save normalises trailing whitespace on the
	/// disk bytes.
	#[cfg(unix)]
	#[tokio::test]
	async fn format_file_runs_editorconfig_and_lint_staged_chain_against_disk_bytes() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);

		write_executable_script(
			&dir.path().join("upper.sh"),
			"#!/bin/sh\ntr '[:lower:]' '[:upper:]' < \"$1\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\"\n",
		);
		std::fs::write(dir.path().join(".lintstagedrc.json"), r#"{ "*.txt": "./upper.sh" }"#).unwrap();
		// Pre-seed the file with un-normalised bytes the way the
		// coder's `write_file` tool would: raw, trailing whitespace
		// intact, no final newline.
		std::fs::write(dir.path().join("a.txt"), "hello world   ").unwrap();

		let result = h.format_file(Utf8Path::new("a.txt")).await.unwrap();
		let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
		// Editorconfig stripped the trailing run of spaces and added
		// a final newline; lint-staged then uppercased the lot.
		assert_eq!(on_disk, "HELLO WORLD\n");
		assert_eq!(result.bytes_written, on_disk.len() as u64);
	}

	/// When the lint-staged chain doesn't match and editorconfig
	/// is a no-op (bytes already canonical), `format_file` returns
	/// the current stat without writing.
	#[tokio::test]
	async fn format_file_is_a_no_op_when_already_canonical_and_no_rule_matches() {
		let dir = TempDir::new().unwrap();
		let h = host(&dir);
		std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
		let mtime_before = std::fs::metadata(dir.path().join("a.txt")).unwrap().modified().unwrap();
		// Sleep briefly so a re-write would be detectable by mtime.
		std::thread::sleep(std::time::Duration::from_millis(20));

		let result = h.format_file(Utf8Path::new("a.txt")).await.unwrap();
		let mtime_after = std::fs::metadata(dir.path().join("a.txt")).unwrap().modified().unwrap();
		assert_eq!(mtime_before, mtime_after, "no-op format_file should not have written");
		assert_eq!(result.bytes_written, 6);
	}
}
