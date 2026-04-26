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
}
