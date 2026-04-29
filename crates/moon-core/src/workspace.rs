//! Workspace registry: tracks the singleton workspace and the
//! folders bound into it.
//!
//! Phase 2.5 makes "workspace" and "folder" different things. The
//! singleton workspace ID is fixed to `"default"`; named multi-
//! workspace UI lands in Phase 7. Each folder owns its own
//! [`WorkspaceHost`] (today always [`LocalHost`]) — fs and search
//! commands route through the active folder's host, never the
//! workspace's, because hosts are per-folder by construction.

use camino::Utf8PathBuf;
use moon_protocol::workspace::{HostKind, Workspace as WorkspaceRecord, WorkspaceFolder, WorkspaceId};
use moon_protocol::{MoonError, MoonResult};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::host::{LocalHost, WorkspaceHost};

/// Fixed ID of the singleton workspace until Phase 7 introduces named
/// workspaces. Surfaced as a constant so downstream code (container
/// project naming, future state-dir layout) doesn't re-derive it.
pub const DEFAULT_WORKSPACE_ID: &str = "default";

/// One bound folder plus the host that drives reads/writes for paths
/// inside it. Held behind an `Arc` so command handlers can hang on to
/// it past the registry lock release.
pub struct WorkspaceFolderEntry {
	pub folder: WorkspaceFolder,
	pub host: Arc<dyn WorkspaceHost>,
}

impl std::fmt::Debug for WorkspaceFolderEntry {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("WorkspaceFolderEntry")
			.field("folder", &self.folder)
			.finish_non_exhaustive()
	}
}

#[derive(Default)]
pub struct WorkspaceRegistry {
	inner: RwLock<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
	folders: Vec<Arc<WorkspaceFolderEntry>>,
	active_folder_path: Option<String>,
}

impl WorkspaceRegistry {
	pub fn new() -> Self {
		Self::default()
	}

	/// Add `path` as a folder in the workspace and make it active.
	/// Idempotent on duplicate path: returns the existing entry and
	/// flips `active_folder_path` to it without inserting a second
	/// copy or rebuilding the host.
	pub async fn add_folder(&self, path: Utf8PathBuf) -> MoonResult<Arc<WorkspaceFolderEntry>> {
		if !path.exists() {
			return Err(MoonError::NotFound(path.to_string()));
		}
		if !path.is_dir() {
			return Err(MoonError::invalid(format!("{path} is not a directory")));
		}
		let canonical = std::fs::canonicalize(path.as_std_path()).map_err(MoonError::from)?;
		let canonical = Utf8PathBuf::from_path_buf(canonical)
			.map_err(|p| MoonError::Internal(format!("non-utf8 path: {}", p.display())))?;
		let canonical_str = canonical.to_string();

		let mut inner = self.inner.write().await;

		if let Some(existing) = inner.folders.iter().find(|e| e.folder.path == canonical_str) {
			let entry = existing.clone();
			inner.active_folder_path = Some(canonical_str);
			return Ok(entry);
		}

		let name = canonical.file_name().unwrap_or("workspace").to_string();
		let folder = WorkspaceFolder {
			path: canonical_str.clone(),
			name,
			host: HostKind::Local,
		};
		let entry = Arc::new(WorkspaceFolderEntry {
			folder,
			host: Arc::new(LocalHost::new(canonical)),
		});
		inner.folders.push(entry.clone());
		inner.active_folder_path = Some(canonical_str);
		Ok(entry)
	}

	/// Remove the folder at `path`. If it was active, the
	/// previous folder in insertion order takes over (or the new first,
	/// if index 0 was removed); when no folders remain the workspace
	/// is empty and `active_folder` is `None`.
	pub async fn remove_folder(&self, path: &str) -> MoonResult<()> {
		let mut inner = self.inner.write().await;
		let pos = inner
			.folders
			.iter()
			.position(|e| e.folder.path == path)
			.ok_or_else(|| MoonError::NotFound(format!("folder {path}")))?;
		inner.folders.remove(pos);
		if inner.active_folder_path.as_deref() == Some(path) {
			let new_idx = pos.saturating_sub(1);
			inner.active_folder_path = inner.folders.get(new_idx).map(|e| e.folder.path.clone());
		}
		Ok(())
	}

	/// Set the active folder. Errors if `path` isn't already in the
	/// workspace — callers should `add_folder` first if they need
	/// to bind a new path.
	pub async fn set_active_folder(&self, path: &str) -> MoonResult<()> {
		let mut inner = self.inner.write().await;
		if !inner.folders.iter().any(|e| e.folder.path == path) {
			return Err(MoonError::NotFound(format!("folder {path}")));
		}
		inner.active_folder_path = Some(path.to_string());
		Ok(())
	}

	/// Snapshot the workspace as the wire shape the frontend speaks.
	pub async fn snapshot(&self) -> WorkspaceRecord {
		let inner = self.inner.read().await;
		WorkspaceRecord {
			id: DEFAULT_WORKSPACE_ID.to_string(),
			folders: inner.folders.iter().map(|e| e.folder.clone()).collect(),
			active_folder: inner.active_folder_path.clone(),
		}
	}

	/// All folder entries in insertion order.
	pub async fn folders(&self) -> Vec<Arc<WorkspaceFolderEntry>> {
		self.inner.read().await.folders.clone()
	}

	/// Active folder entry. `None` when the workspace is empty or no
	/// folder has been activated.
	pub async fn active_folder(&self) -> Option<Arc<WorkspaceFolderEntry>> {
		let inner = self.inner.read().await;
		let path = inner.active_folder_path.as_ref()?;
		inner.folders.iter().find(|e| e.folder.path == *path).cloned()
	}

	/// Like [`active_folder`] but errors with a helpful message when
	/// no folder is active. Used by every fs/search/editorconfig
	/// command — they can't operate without a folder context.
	pub async fn require_active_folder(&self) -> MoonResult<Arc<WorkspaceFolderEntry>> {
		self
			.active_folder()
			.await
			.ok_or_else(|| MoonError::invalid("no active folder"))
	}

	/// Look up a folder entry by absolute path.
	pub async fn folder_for_path(&self, path: &str) -> Option<Arc<WorkspaceFolderEntry>> {
		let inner = self.inner.read().await;
		inner.folders.iter().find(|e| e.folder.path == path).cloned()
	}

	pub async fn workspace_id(&self) -> WorkspaceId {
		DEFAULT_WORKSPACE_ID.to_string()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempfile::TempDir;

	#[tokio::test]
	async fn add_folder_sets_active() {
		let dir = TempDir::new().unwrap();
		let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

		let registry = WorkspaceRegistry::new();
		let entry = registry.add_folder(path.clone()).await.unwrap();

		assert_eq!(entry.folder.host, HostKind::Local);
		let snap = registry.snapshot().await;
		assert_eq!(snap.folders.len(), 1);
		assert_eq!(snap.active_folder.as_deref(), Some(entry.folder.path.as_str()));
	}

	#[tokio::test]
	async fn add_folder_rejects_files() {
		let dir = TempDir::new().unwrap();
		let file = dir.path().join("not-a-dir");
		std::fs::write(&file, "x").unwrap();
		let path = Utf8PathBuf::from_path_buf(file).unwrap();

		let registry = WorkspaceRegistry::new();
		let err = registry.add_folder(path).await.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)));
	}

	#[tokio::test]
	async fn add_folder_is_idempotent() {
		let dir = TempDir::new().unwrap();
		let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

		let registry = WorkspaceRegistry::new();
		let first = registry.add_folder(path.clone()).await.unwrap();
		let second = registry.add_folder(path.clone()).await.unwrap();

		assert!(
			Arc::ptr_eq(&first, &second),
			"duplicate add should reuse the existing host"
		);
		let snap = registry.snapshot().await;
		assert_eq!(snap.folders.len(), 1);
	}

	#[tokio::test]
	async fn remove_folder_reassigns_active() {
		let one = TempDir::new().unwrap();
		let two = TempDir::new().unwrap();
		let one_path = Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
		let two_path = Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();

		let registry = WorkspaceRegistry::new();
		let entry_one = registry.add_folder(one_path.clone()).await.unwrap();
		let entry_two = registry.add_folder(two_path.clone()).await.unwrap();
		assert_eq!(
			registry.snapshot().await.active_folder.as_deref(),
			Some(entry_two.folder.path.as_str())
		);

		// Removing the active folder hands active to the previous one.
		registry.remove_folder(&entry_two.folder.path).await.unwrap();
		assert_eq!(
			registry.snapshot().await.active_folder.as_deref(),
			Some(entry_one.folder.path.as_str())
		);

		// Removing the last folder leaves the workspace empty.
		registry.remove_folder(&entry_one.folder.path).await.unwrap();
		let snap = registry.snapshot().await;
		assert!(snap.folders.is_empty());
		assert!(snap.active_folder.is_none());
	}

	#[tokio::test]
	async fn set_active_folder_rejects_unknown_path() {
		let registry = WorkspaceRegistry::new();
		let err = registry.set_active_folder("/nope").await.unwrap_err();
		assert!(matches!(err, MoonError::NotFound(_)));
	}
}
