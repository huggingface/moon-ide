use camino::Utf8PathBuf;
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_protocol::container::ContainerState;
use moon_protocol::MoonError;

use crate::AppCore;

/// Build the container workspace handle for this process's
/// workspace from the current bound-folder set.
pub async fn workspace_handle(app: &AppCore) -> Result<ContainerWorkspace, MoonError> {
	let snapshot = app.workspaces.snapshot().await;
	// Worktree-backed session folders (ADR 0028) are host-only — their
	// tooling runs host-side, so they don't get a container bind mount.
	let bound_folders: Vec<Utf8PathBuf> = snapshot
		.folders
		.iter()
		.filter(|f| !matches!(f.origin, moon_protocol::workspace::FolderOrigin::Worktree { .. }))
		.map(|f| Utf8PathBuf::from(&f.path))
		.collect();
	let state_dir = app.workspaces_dir.join(&snapshot.id);
	let container = ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: snapshot.id,
		state_dir,
		bound_folders,
	})?;
	Ok(container)
}

/// Is this workspace's dev container currently `Running`?
/// Best-effort: any error (no compose, daemon down) reads as `false`.
/// Used by the worktree orchestration (ADR 0028 W.4.1) to decide
/// whether a worktree's git ops run host- or container-side.
pub async fn workspace_container_running(app: &AppCore) -> bool {
	let Ok(container) = workspace_handle(app).await else {
		return false;
	};
	matches!(container.status().await, Ok(s) if matches!(s.state, ContainerState::Running))
}
