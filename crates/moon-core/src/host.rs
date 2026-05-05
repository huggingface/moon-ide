//! `WorkspaceHost` is the I/O boundary. See [ADR 0002](../../../specs/decisions/0002-workspace-host.md).
//!
//! Phase 0 ships only `LocalHost`. The trait exists pre-implementation
//! so call sites in higher layers don't have to be rewritten when
//! `RemoteHost` lands in Phase 2.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::editorconfig::EditorConfig;
use moon_protocol::fs::{DirEntry, EntryKind, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{GitBranchInfo, GitCommitResult, GitFileBlame, GitFileStatus, GitLineBlame, GitStatusEntry};
use moon_protocol::{MoonError, MoonResult};
use std::time::SystemTime;

use crate::editorconfig::EditorConfigService;
use crate::format;
use crate::lint_staged::{LintStagedRules, LintStagedService};
use crate::pre_save;

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
	/// then the lint-staged formatter (oxfmt / prettier / rustfmt) for
	/// files that have one configured. Every editor save and every agent
	/// write funnels through this, so the on-disk shape matches what
	/// `bun run lint-staged` would produce regardless of who issued the
	/// write. Failures inside the formatter step never abort the save —
	/// callers are guaranteed to land at least the editorconfig-normalised
	/// bytes. See [specs/decisions/0012-format-on-save.md](../../../specs/decisions/0012-format-on-save.md).
	async fn save_file(&self, path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult>;

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
	/// the cap are included but their children aren't.
	///
	/// Exists separately from `read_dir` because the tree's walker
	/// would otherwise fire one IPC roundtrip per directory —
	/// dominating the refresh latency on anything bigger than a
	/// handful of folders. One call collapses hundreds of
	/// roundtrips into a single backend walk, which is the actual
	/// work; everything else was IPC framing.
	async fn collect_paths(&self, max_depth: u32) -> MoonResult<Vec<String>>;

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

	/// Lightweight branch / HEAD info for the SCM panel header.
	/// Returns the all-`None` default when the active folder isn't
	/// a git repo or `git` itself can't run — the UI treats that
	/// as "show no branch label" rather than a hard error so
	/// non-git workspaces still render cleanly.
	async fn git_branch(&self) -> MoonResult<GitBranchInfo>;

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

	/// `git push` with no arguments — uses the configured upstream
	/// for the current branch. Errors propagate git's own stderr
	/// verbatim so messages like "the current branch X has no
	/// upstream branch" stay actionable. We don't try to
	/// auto-set-upstream: that's a multi-remote decision the user
	/// should make explicitly (the SCM panel surfaces git's hint
	/// telling them the exact `git push -u origin <branch>` to run).
	async fn git_push(&self) -> MoonResult<()>;

	/// `git pull` with no arguments — uses the user's configured
	/// `pull.rebase` setting. Errors propagate git's stderr;
	/// conflict markers in the working tree, dirty-tree refusals,
	/// and missing-upstream messages all stay readable.
	async fn git_pull(&self) -> MoonResult<()>;
}

pub struct LocalHost {
	root: Utf8PathBuf,
	editorconfig: EditorConfigService,
	lint_staged: LintStagedService,
}

impl LocalHost {
	pub fn new(root: Utf8PathBuf) -> Self {
		Self {
			editorconfig: EditorConfigService::new(root.clone()),
			lint_staged: LintStagedService::new(root.clone()),
			root,
		}
	}

	pub fn root(&self) -> &Utf8Path {
		&self.root
	}

	/// Run the lint-staged formatter for `rel` against `text`, if one is
	/// configured. Returns `None` (caller falls back to the editorconfig-
	/// normalised text) when there's no rule for the file or when the
	/// formatter itself misses — every miss path is logged inside
	/// [`crate::format::run_formatter`].
	async fn maybe_run_formatter(&self, rel: &Utf8Path, text: &str) -> Option<String> {
		let rules = self.lint_staged.for_path(rel).await.ok()?;
		if rules.is_empty() {
			return None;
		}
		// `absolute_path` is the only way to get the host-side absolute
		// path for a workspace-relative input. On `RemoteHost` (Phase 2)
		// this would be the in-container path — exactly what the
		// in-container formatter wants on its `--stdin-filepath`.
		let abs_str = self.absolute_path(rel).await.ok()?;
		let abs = Utf8PathBuf::from(abs_str);
		let cmd = rules.match_command(abs.as_std_path())?.to_owned();
		// `config_dir` is `Some` whenever `match_command` returned a hit
		// (the rule came from a real file on disk); the workspace root
		// is just a defensive fallback the type system asks for.
		let cwd = rules.config_dir().unwrap_or(&self.root).to_path_buf();
		format::run_formatter(&self.root, &cwd, &abs, &cmd, text).await
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

		// The parent must already exist; we don't auto-mkdir here.
		let resolved_parent = self.resolve(parent)?;
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
		// Formatter wins: oxfmt / prettier / rustfmt all canonicalise
		// line endings, trim trailing whitespace, and ensure a final
		// newline themselves. Running the editorconfig pipeline first
		// is wasted work (and risks fighting a formatter that has a
		// different opinion on, say, line-ending style — formatters
		// are the canonical source of truth for files they own). The
		// editorconfig pipeline only kicks in as the fallback when no
		// formatter ran: no lint-staged rule for this file, the rule
		// pointed at an unsupported tool, or the formatter subprocess
		// failed. See specs/decisions/0012-format-on-save.md.
		if let Some(formatted) = self.maybe_run_formatter(path, text).await {
			return self.write_file(path, &formatted).await;
		}
		let ec = self.editorconfig.for_path(path).await?;
		let normalized = pre_save::apply_pipeline(text, &ec);
		self.write_file(path, &normalized).await
	}

	async fn git_status_entries(&self, paths: &[String]) -> MoonResult<Vec<GitStatusEntry>> {
		// Both the `git status` subprocess and the walker fallback
		// are blocking work, so hop onto the blocking pool. The git
		// path is dominated by git itself anyway; the walker is
		// single-threaded but fast enough for IDE-sized trees (tens
		// of thousands of files) without `build_parallel`'s wiring.
		let root = self.root.clone();
		let paths = paths.to_vec();
		tokio::task::spawn_blocking(move || classify_git_status(&root, &paths))
			.await
			.map_err(|e| MoonError::Internal(format!("git_status_entries join error: {e}")))?
	}

	async fn collect_paths(&self, max_depth: u32) -> MoonResult<Vec<String>> {
		// Pure `std::fs` walk on the blocking pool. Tried using
		// `tokio::fs::read_dir` recursively here — it kept the
		// reactor busy with tiny awaits per entry and wound up
		// slower than the sync version, presumably because the
		// actual read_dir syscall is already non-blocking-ish on
		// modern kernels.
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || {
			let mut out = Vec::new();
			walk_paths(&root, "", &mut out, 0, max_depth);
			Ok(out)
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
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || run_git_restore(&root, &rels))
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
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || run_git_blame(&root, &rel))
			.await
			.map_err(|e| MoonError::Internal(format!("git_blame join error: {e}")))?
	}

	async fn git_head_content(&self, path: &Utf8Path) -> MoonResult<Option<String>> {
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
				"git_head_content rejects absolute path: {rel}"
			)));
		}
		let mut depth = 0i32;
		for seg in rel.components() {
			match seg {
				camino::Utf8Component::ParentDir => {
					depth -= 1;
					if depth < 0 {
						return Err(MoonError::invalid(format!(
							"git_head_content rejects path escape: {rel}"
						)));
					}
				}
				camino::Utf8Component::Normal(_) => depth += 1,
				camino::Utf8Component::CurDir => {}
				camino::Utf8Component::Prefix(_) | camino::Utf8Component::RootDir => {
					return Err(MoonError::invalid(format!(
						"git_head_content rejects rooted path: {rel}"
					)));
				}
			}
		}
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || run_git_head_content(&root, &rel))
			.await
			.map_err(|e| MoonError::Internal(format!("git_head_content join error: {e}")))?
	}

	async fn git_branch(&self) -> MoonResult<GitBranchInfo> {
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || Ok(run_git_branch(&root)))
			.await
			.map_err(|e| MoonError::Internal(format!("git_branch join error: {e}")))?
	}

	async fn git_commit(&self, message: &str, amend: bool) -> MoonResult<GitCommitResult> {
		let trimmed = message.trim();
		if trimmed.is_empty() && !amend {
			return Err(MoonError::invalid("commit message is empty"));
		}
		let root = self.root.clone();
		let owned = trimmed.to_owned();
		tokio::task::spawn_blocking(move || run_git_commit(&root, &owned, amend))
			.await
			.map_err(|e| MoonError::Internal(format!("git_commit join error: {e}")))?
	}

	async fn git_push(&self) -> MoonResult<()> {
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || run_git_simple(&root, &["push"], "git push"))
			.await
			.map_err(|e| MoonError::Internal(format!("git_push join error: {e}")))?
	}

	async fn git_pull(&self) -> MoonResult<()> {
		let root = self.root.clone();
		tokio::task::spawn_blocking(move || run_git_simple(&root, &["pull"], "git pull"))
			.await
			.map_err(|e| MoonError::Internal(format!("git_pull join error: {e}")))?
	}
}

/// `git symbolic-ref --short HEAD` for the branch name plus
/// `git rev-parse --short HEAD` for the commit hash. Both can fail
/// independently — fresh `git init` with no commits has a
/// resolvable branch name but no HEAD, and a detached HEAD has the
/// reverse — so we run them separately and return whichever
/// succeeded. Any failure (including the folder not being a git
/// repo) leaves the corresponding field as `None`; the SCM panel
/// renders the all-`None` case as "no branch".
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

	GitBranchInfo { name, head_short_sha }
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
fn run_git_commit(root: &Utf8Path, message: &str, amend: bool) -> MoonResult<GitCommitResult> {
	use std::process::Command;

	let stage = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args(["add", "-A"])
		.output()
		.map_err(|e| MoonError::IoError(format!("git add failed to launch: {e}")))?;
	if !stage.status.success() {
		let stderr = String::from_utf8_lossy(&stage.stderr).trim().to_string();
		return Err(MoonError::IoError(format!(
			"git add exited {}: {stderr}",
			stage.status.code().unwrap_or(-1)
		)));
	}

	let mut commit = Command::new("git");
	commit.arg("-C").arg(root.as_std_path()).arg("commit");
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

	let short_sha = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
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
		Command::new("git")
			.arg("-C")
			.arg(root.as_std_path())
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

/// Run `git <args>` from `root` and surface stderr verbatim on
/// failure. Used by `git_push` and `git_pull` (and any future
/// "shoot a git command and see if it worked" SCM action) so
/// network / auth / merge-conflict messages reach the user
/// without us second-guessing their wording.
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

/// `git show HEAD:<path>`. Returns `Ok(None)` for the "no diff to
/// show" states the UI treats silently: not a repo, path isn't in
/// `HEAD` (freshly added / untracked), or `git` itself is missing.
/// Binary contents at `HEAD` collapse to `None` too — the diff view
/// only renders text. UTF-8 decode failures on something we *thought*
/// was text are the one real error path and bubble up.
///
/// Invoked from the blocking pool.
fn run_git_head_content(root: &Utf8Path, path: &Utf8PathBuf) -> MoonResult<Option<String>> {
	use std::process::Command;

	// `HEAD:<path>` uses forward slashes regardless of host OS —
	// git's pathspec grammar isn't the platform's. The path is
	// already workspace-relative + UTF-8 so the conversion is
	// lossless; Windows paths with backslashes would confuse git
	// silently otherwise.
	// `git show <rev>:<path>` is the stable way to pull a committed
	// blob by path. `--` isn't used here: `git show` treats args
	// after `--` as pathspecs rather than as rev-parse inputs, and
	// the blob would come back empty.
	let spec = format!("HEAD:{}", path.as_str().replace('\\', "/"));
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
		// - "fatal: path 'foo' exists on disk, but not in 'HEAD'"
		//   → untracked / newly-added. The diff for those is
		//   "everything is new", which the frontend renders by
		//   passing an empty "before" side itself; we don't need
		//   to fake a success here.
		let stderr = String::from_utf8_lossy(&output.stderr);
		tracing::debug!(
			path = %path,
			code = output.status.code().unwrap_or(-1),
			stderr = %stderr.trim(),
			"git show HEAD:<path> exited non-zero"
		);
		return Ok(None);
	}
	if looks_binary(&output.stdout) {
		return Ok(None);
	}
	String::from_utf8(output.stdout)
		.map(Some)
		.map_err(|e| MoonError::IoError(format!("git show HEAD:<path> produced non-UTF-8 text: {e}")))
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
fn walk_paths(root: &Utf8Path, rel: &str, out: &mut Vec<String>, depth: u32, max_depth: u32) {
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
			// `.git/` hides on purpose — see `read_dir`'s matching
			// skip. Once Phase 5's git layer fully lands this may
			// move to a gitignore-aware filter, but right now the
			// tree would drown in refs/ churn if we surfaced it.
			if name == ".git" {
				continue;
			}
			out.push(format!("{child_rel}/"));
			if depth < max_depth {
				walk_paths(root, &child_rel, out, depth + 1, max_depth);
			}
		} else if file_type.is_file() || file_type.is_symlink() {
			out.push(child_rel);
		}
	}
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
fn classify_git_status(root: &Utf8Path, paths: &[String]) -> MoonResult<Vec<GitStatusEntry>> {
	if let Some(entries) = classify_via_git_status(root) {
		return Ok(entries);
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
fn classify_via_git_status(root: &Utf8Path) -> Option<Vec<GitStatusEntry>> {
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
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
/// this priority:
///
/// 1. Either column is `D` → `Deleted` (dominates so stage-then-
///    revert doesn't mask the on-disk state).
/// 2. Either column is `A` → `Added` (staged-for-commit new file).
/// 3. Either column is `M` / `T` → `Modified`.
/// 4. `??` → `Untracked`.
/// 5. `!!` → `Ignored`.
///
/// Anything else (`UU` conflicts, `C` copies we didn't disable) is
/// silently dropped — conflicts surface in the SCM panel per the
/// roadmap, and we don't want a stray `copy` byte to paint an
/// arbitrary row.
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

		let paths = host(&dir).collect_paths(6).await.unwrap();
		let set: std::collections::HashSet<_> = paths.into_iter().collect();
		assert!(set.contains("README.md"), "got {set:?}");
		assert!(set.contains("src/"), "got {set:?}");
		assert!(set.contains("src/lib.rs"), "got {set:?}");
		assert!(set.contains("src/nested/"), "got {set:?}");
		assert!(set.contains("src/nested/deep.rs"), "got {set:?}");
		// `.git/` and everything inside it stays off the tree.
		assert!(!set.iter().any(|p| p.starts_with(".git")), "got {set:?}");
	}

	#[tokio::test]
	async fn collect_paths_respects_max_depth() {
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("a").join("b").join("c")).unwrap();
		std::fs::write(dir.path().join("a").join("b").join("c").join("deep.txt"), "").unwrap();

		// depth=0 → only the immediate children are enumerated;
		// `a/` is listed but `a/b/` isn't recursed.
		let paths = host(&dir).collect_paths(0).await.unwrap();
		let set: std::collections::HashSet<_> = paths.into_iter().collect();
		assert!(set.contains("a/"), "got {set:?}");
		assert!(!set.contains("a/b/"), "got {set:?}");
		assert!(!set.contains("a/b/c/deep.txt"), "got {set:?}");
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

	fn which_git() -> Option<std::path::PathBuf> {
		std::process::Command::new("git")
			.arg("--version")
			.output()
			.ok()
			.filter(|o| o.status.success())
			.map(|_| std::path::PathBuf::from("git"))
	}

	fn run_git(git: &std::path::Path, cwd: &std::path::Path, args: &[&str]) {
		let out = std::process::Command::new(git)
			.arg("-C")
			.arg(cwd)
			.args(args)
			.output()
			.expect("spawn git");
		assert!(
			out.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
	}
}
