//! Workspace registry: tracks open workspaces and their hosts.
//!
//! Phase 0 supports exactly one active workspace. Multi-root lands in Phase 7;
//! the registry already returns a list to make that change source-compatible.

use camino::Utf8PathBuf;
use moon_protocol::workspace::{HostKind, Workspace as WorkspaceRecord, WorkspaceId};
use moon_protocol::{MoonError, MoonResult};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::host::{LocalHost, WorkspaceHost};

pub struct Workspace {
	pub record: WorkspaceRecord,
	pub host: Arc<dyn WorkspaceHost>,
}

impl std::fmt::Debug for Workspace {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Workspace")
			.field("record", &self.record)
			.finish_non_exhaustive()
	}
}

#[derive(Default)]
pub struct WorkspaceRegistry {
	inner: RwLock<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
	active: Option<Arc<Workspace>>,
}

impl WorkspaceRegistry {
	pub fn new() -> Self {
		Self::default()
	}

	pub async fn open_local(&self, path: Utf8PathBuf) -> MoonResult<Arc<Workspace>> {
		if !path.exists() {
			return Err(MoonError::NotFound(path.to_string()));
		}
		if !path.is_dir() {
			return Err(MoonError::invalid(format!("{path} is not a directory")));
		}

		let canonical = std::fs::canonicalize(path.as_std_path()).map_err(MoonError::from)?;
		let canonical = Utf8PathBuf::from_path_buf(canonical)
			.map_err(|p| MoonError::Internal(format!("non-utf8 path: {}", p.display())))?;

		let name = canonical.file_name().unwrap_or("workspace").to_string();

		let record = WorkspaceRecord {
			id: Uuid::new_v4().to_string(),
			name,
			root: canonical.to_string(),
			host: HostKind::Local,
		};

		let workspace = Arc::new(Workspace {
			record,
			host: Arc::new(LocalHost::new(canonical)),
		});

		let mut inner = self.inner.write().await;
		inner.active = Some(workspace.clone());
		Ok(workspace)
	}

	pub async fn active(&self) -> Option<Arc<Workspace>> {
		self.inner.read().await.active.clone()
	}

	pub async fn require_active(&self) -> MoonResult<Arc<Workspace>> {
		self
			.active()
			.await
			.ok_or_else(|| MoonError::invalid("no active workspace"))
	}

	pub async fn list(&self) -> Vec<WorkspaceRecord> {
		match self.active().await {
			Some(ws) => vec![ws.record.clone()],
			None => Vec::new(),
		}
	}

	pub async fn require_active_id(&self, id: &WorkspaceId) -> MoonResult<Arc<Workspace>> {
		let ws = self.require_active().await?;
		if ws.record.id != *id {
			return Err(MoonError::NotFound(format!("workspace {id}")));
		}
		Ok(ws)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempfile::TempDir;

	#[tokio::test]
	async fn open_local_sets_active() {
		let dir = TempDir::new().unwrap();
		let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

		let registry = WorkspaceRegistry::new();
		let ws = registry.open_local(path.clone()).await.unwrap();

		assert_eq!(ws.record.host, HostKind::Local);
		assert!(registry.active().await.is_some());
		assert_eq!(registry.list().await.len(), 1);
	}

	#[tokio::test]
	async fn open_local_rejects_files() {
		let dir = TempDir::new().unwrap();
		let file = dir.path().join("not-a-dir");
		std::fs::write(&file, "x").unwrap();
		let path = Utf8PathBuf::from_path_buf(file).unwrap();

		let registry = WorkspaceRegistry::new();
		let err = registry.open_local(path).await.unwrap_err();
		assert!(matches!(err, MoonError::InvalidArgument(_)));
	}
}
