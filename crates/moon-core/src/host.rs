//! `WorkspaceHost` is the I/O boundary. See [ADR 0002](../../../specs/decisions/0002-workspace-host.md).
//!
//! Phase 0 ships only `LocalHost`. The trait exists pre-implementation
//! so call sites in higher layers don't have to be rewritten when
//! `RemoteHost` lands in Phase 2.

use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::editorconfig::EditorConfig;
use moon_protocol::fs::{DirEntry, EntryKind, ReadFileResult, StatResult, WriteFileResult};
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

	/// Workspace-relative paths (directories carry a trailing `/`, to
	/// match `read_dir` output and the tree's path format) that match
	/// the effective gitignore rules — `.gitignore` files at any depth,
	/// `.git/info/exclude`, and the user's global excludes. The frontend
	/// feeds the result to Pierre Trees as ignored rows so they paint
	/// faded rather than vanish; hiding ignored files outright would
	/// miss the common "peek at `target/` or `node_modules/`" workflow.
	async fn git_ignored_paths(&self, paths: &[String]) -> MoonResult<Vec<String>>;
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

	async fn git_ignored_paths(&self, paths: &[String]) -> MoonResult<Vec<String>> {
		// The ignore walker is synchronous, so hop to the blocking pool
		// to keep the Tokio reactor free. `ignore` fans out its own
		// threads if we use `build_parallel`, but the single-threaded
		// walk is already fast enough for IDE-sized trees (tens of
		// thousands of files) and keeps the code straight-line.
		let root = self.root.clone();
		let paths = paths.to_vec();
		tokio::task::spawn_blocking(move || classify_git_ignored(&root, &paths))
			.await
			.map_err(|e| MoonError::Internal(format!("git_ignored_paths join error: {e}")))?
	}
}

/// Returns the subset of `paths` (workspace-relative, directories
/// optionally terminated by `/`) that git / the walker treat as
/// ignored.
///
/// Primary strategy inside a git repo: ask `git ls-files` itself.
/// That path respects the index, so a file matching a `.gitignore`
/// pattern but already tracked (committed, or `git add -f`'d) is
/// correctly reported as _not_ ignored — the walker below can't make
/// that distinction on its own because `ignore::WalkBuilder` has no
/// view of the index.
///
/// Fallback (no git repo / git binary missing): walk the tree with
/// the standard gitignore filters and treat anything the walker
/// doesn't yield as ignored. Good enough for pre-`git init` folders;
/// loses the index-aware behaviour above, which is the price of not
/// requiring git to be installed.
fn classify_git_ignored(root: &Utf8Path, paths: &[String]) -> MoonResult<Vec<String>> {
	if let Some(ignored) = classify_via_git_ls_files(root) {
		return Ok(filter_to_inputs(&ignored, paths));
	}
	classify_via_walker(root, paths)
}

/// Run `git ls-files --others --ignored --exclude-standard` in the
/// workspace root. The combination lists paths that (a) aren't in
/// the index and (b) match an exclude rule — i.e. the exact set git
/// considers ignored for a rendering pass. `--directory` collapses
/// fully-ignored directories to a single entry (`target/`) instead
/// of enumerating every file below.
///
/// Returns `None` if the command can't run or exits non-zero — the
/// caller falls back to the walker so a non-repo folder still gets a
/// reasonable answer. Stderr is intentionally swallowed; git's
/// "not a git repository" complaint is expected and not interesting.
fn classify_via_git_ls_files(root: &Utf8Path) -> Option<std::collections::HashSet<String>> {
	use std::collections::HashSet;
	use std::process::Command;

	let output = Command::new("git")
		.arg("-C")
		.arg(root.as_std_path())
		.args([
			"ls-files",
			"-z",
			"--others",
			"--ignored",
			"--exclude-standard",
			"--directory",
		])
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	let mut ignored: HashSet<String> = HashSet::new();
	for raw in output.stdout.split(|b| *b == 0) {
		if raw.is_empty() {
			continue;
		}
		let Ok(s) = std::str::from_utf8(raw) else {
			continue;
		};
		// `git ls-files --directory` emits `foo/` for directory
		// entries. Insert both the slashed and bare form so the
		// filter loop can match either convention without second-
		// guessing what the caller sent.
		let normalised = s.replace('\\', "/");
		if let Some(bare) = normalised.strip_suffix('/') {
			ignored.insert(bare.to_string());
			ignored.insert(normalised);
		} else {
			ignored.insert(normalised);
		}
	}
	Some(ignored)
}

fn classify_via_walker(root: &Utf8Path, paths: &[String]) -> MoonResult<Vec<String>> {
	use ignore::WalkBuilder;
	use std::collections::HashSet;

	let mut visible: HashSet<String> = HashSet::new();
	// `hidden(false)` keeps dotfiles like `.gitignore` itself in the
	// visible set; `git_ignore` / `git_exclude` / `git_global` turn on
	// the three ignore sources users expect. `ignore(true)` also
	// respects `.ignore` files, which is the ripgrep convention and
	// aligns with our own `search_files` command.
	let walker = WalkBuilder::new(root.as_std_path())
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.git_global(true)
		// Apply `.gitignore` even when `.git/` isn't present. A folder
		// with a `.gitignore` at its root is a common pre-init
		// scenario, and users still expect those patterns to fade the
		// tree — the alternative (nothing fades until `git init` runs)
		// is a surprising trap.
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

	let mut ignored = Vec::new();
	for path in paths {
		let trimmed = path.trim_end_matches('/');
		if trimmed.is_empty() {
			continue;
		}
		if !visible.contains(path.as_str()) && !visible.contains(trimmed) {
			ignored.push(path.clone());
		}
	}
	Ok(ignored)
}

/// Reduce the "everything git considers ignored" set to just the
/// paths the frontend asked about, matching either slash convention.
///
/// Directory matching is a little fuzzy on purpose: git reports
/// ignored folders as `foo/` (via `--directory`), but if the frontend
/// only asked about a file _inside_ that folder, we still want to
/// flag it. We climb each input's ancestors and flag the whole path
/// if any ancestor is in the ignored set.
fn filter_to_inputs(ignored: &std::collections::HashSet<String>, paths: &[String]) -> Vec<String> {
	let mut out = Vec::new();
	for path in paths {
		if is_path_ignored(ignored, path) {
			out.push(path.clone());
		}
	}
	out
}

fn is_path_ignored(ignored: &std::collections::HashSet<String>, path: &str) -> bool {
	let trimmed = path.trim_end_matches('/');
	if trimmed.is_empty() {
		return false;
	}
	if ignored.contains(path) || ignored.contains(trimmed) {
		return true;
	}
	// Walk ancestors: `target/debug/build` is ignored if `target/` or
	// `target/debug/` is. Stops at the workspace root (no leading
	// slash to worry about — paths are workspace-relative).
	let mut cursor = trimmed;
	while let Some(slash) = cursor.rfind('/') {
		cursor = &cursor[..slash];
		if cursor.is_empty() {
			break;
		}
		if ignored.contains(cursor) || ignored.contains(&format!("{cursor}/")) {
			return true;
		}
	}
	false
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
	async fn git_ignored_paths_matches_gitignore() {
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
		let ignored = host(&dir).git_ignored_paths(&input).await.unwrap();
		let ignored: std::collections::HashSet<_> = ignored.into_iter().collect();
		assert!(ignored.contains("secrets.txt"));
		assert!(ignored.contains("target/"));
		assert!(ignored.contains("target/binary"));
		assert!(!ignored.contains("README.md"));
		assert!(!ignored.contains("src/"));
		assert!(!ignored.contains("src/lib.rs"));
	}

	// A file that matches a `.gitignore` pattern but has already been
	// added to the index (think `.env.example` under a `.env*` rule)
	// must _not_ render as ignored — that's what makes the index-aware
	// `git ls-files` path exist in the first place. The walker fallback
	// wouldn't catch this, which is why we skip it when git isn't on
	// PATH (CI's `git` is always available, so the assertion holds).
	#[tokio::test]
	async fn git_ignored_paths_respects_index() {
		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping index-aware gitignore test");
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
		// `.env.example` matches the `.env*` rule; `-f` force-adds it.
		// Mirrors the real-world "I want the template file in git but
		// keep .env out" pattern we're trying to special-case.
		run_git(&git, dir.path(), &["add", "-f", ".env.example"]);
		run_git(&git, dir.path(), &["commit", "-q", "-m", "init"]);

		let input = vec![
			".env".to_string(),
			".env.example".to_string(),
			"README.md".to_string(),
			".gitignore".to_string(),
		];
		let ignored = host(&dir).git_ignored_paths(&input).await.unwrap();
		let ignored: std::collections::HashSet<_> = ignored.into_iter().collect();
		// `.env` still matches the rule and isn't tracked → ignored.
		assert!(ignored.contains(".env"), "got {ignored:?}");
		// `.env.example` matches the rule but is tracked → NOT ignored.
		// This is the whole reason we prefer `git ls-files` over the
		// pattern-only walker.
		assert!(!ignored.contains(".env.example"), "got {ignored:?}");
		assert!(!ignored.contains("README.md"), "got {ignored:?}");
		assert!(!ignored.contains(".gitignore"), "got {ignored:?}");
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
