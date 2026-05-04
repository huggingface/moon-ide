//! `WorkspaceHost` is the I/O boundary. See [ADR 0002](../../../specs/decisions/0002-workspace-host.md).
//!
//! Phase 0 ships only `LocalHost`. The trait exists pre-implementation
//! so call sites in higher layers don't have to be rewritten when
//! `RemoteHost` lands in Phase 2.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::editorconfig::EditorConfig;
use moon_protocol::fs::{DirEntry, EntryKind, ReadFileResult, StatResult, WriteFileResult};
use moon_protocol::git::{GitFileBlame, GitFileStatus, GitLineBlame, GitStatusEntry};
use moon_protocol::{MoonError, MoonResult};
use std::time::SystemTime;

use crate::editorconfig::EditorConfigService;

#[async_trait]
pub trait WorkspaceHost: Send + Sync {
	async fn read_dir(&self, path: &Utf8Path) -> MoonResult<Vec<DirEntry>>;
	async fn read_file(&self, path: &Utf8Path) -> MoonResult<ReadFileResult>;
	async fn write_file(&self, path: &Utf8Path, text: &str) -> MoonResult<WriteFileResult>;
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
}

pub struct LocalHost {
	root: Utf8PathBuf,
	editorconfig: EditorConfigService,
}

impl LocalHost {
	pub fn new(root: Utf8PathBuf) -> Self {
		Self {
			editorconfig: EditorConfigService::new(root.clone()),
			root,
		}
	}

	pub fn root(&self) -> &Utf8Path {
		&self.root
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

		let metadata = tokio::fs::metadata(resolved.as_std_path())
			.await
			.map_err(MoonError::from)?;

		Ok(WriteFileResult {
			mtime_ms: metadata.modified().ok().and_then(system_time_to_ms),
			bytes_written: text.len() as u64,
		})
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
		Ok(())
	}

	async fn editorconfig_for(&self, path: &Utf8Path) -> MoonResult<EditorConfig> {
		self.editorconfig.for_path(path).await
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
		// Non-repo, untracked file, etc. all exit non-zero. Swallow
		// stderr on purpose — a failed blame is UI-silent by contract.
		return Ok(None);
	}
	let mut blame = parse_blame_porcelain(&output.stdout, path.as_str().to_owned());
	blame.remote_url = remote_web_url(root).unwrap_or_default();
	Ok(Some(blame))
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
