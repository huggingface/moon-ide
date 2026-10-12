//! Commands wrapping `moon-container`.
//!
//! The container's identity is decoupled from any specific
//! folder — the compose project (`moon-ws-<id>`) survives
//! folder switches; only its bound-mount set changes when
//! folders are added or removed. The
//! [`container_apply_bound_folders`] command lets the frontend
//! re-emit `compose.yaml` after a folder add/remove and
//! transparently apply the diff if the project happens to be
//! running.
//!
//! Lifecycle-mutating commands (everything but `container_status`
//! and `container_render_compose`) emit a [`CONTAINER_STATE_EVENT`]
//! after they finish so any background subscribers stay in
//! lockstep without polling.

use camino::Utf8PathBuf;
use moon_container::{
	apply_forwards, list_forward_status, project_name_for_id, Workspace as ContainerWorkspace, WorkspaceConfig,
	DEFAULT_DEV_IMAGE,
};
use moon_core::session as core_session;
use moon_protocol::container::{ContainerState, ContainerStateChange, ContainerStatus};
use moon_protocol::MoonError;

use crate::events::{emit, EventSink};
use crate::ports::PORTS_STATE_EVENT;
use crate::AppCore;

/// Emitted after every successful lifecycle command. Payload:
/// [`ContainerStateChange`].
pub const CONTAINER_STATE_EVENT: &str = "container:state";

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

/// Snapshot status and broadcast the resulting `container:state`
/// event. Called by every mutating command after it completes.
async fn snapshot_and_emit(app: &AppCore, container: &ContainerWorkspace) -> Result<ContainerStatus, MoonError> {
	let status = container.status().await?;
	emit_container_state(app.events.as_ref(), &status);
	Ok(status)
}

/// Broadcast a `container:state` event for an already-fetched
/// status. Shared with the startup auto-resume path and the
/// docker-events watcher, which own their status query rather
/// than going through a mutating command.
pub(crate) fn emit_container_state(events: &dyn EventSink, status: &ContainerStatus) {
	emit(
		events,
		CONTAINER_STATE_EVENT,
		&ContainerStateChange { status: status.clone() },
	);
}

/// Drop any live LSP broker after a container mutation. The next
/// `lsp_open` call rebuilds against the current state (container
/// up → `DockerExec` spawner; container down → host fallback).
async fn reset_lsp_broker(app: &AppCore) {
	let handle = app.lsp.lock().await.take();
	if let Some(handle) = handle {
		handle.broker.shutdown_all().await;
	}
}

/// Re-apply the persisted port-forward set after a workspace
/// lifecycle event that may have torn the proxy sidecar down
/// (setup post-teardown, rebuild, apply_bound_folders triggering
/// a dev recreate, startup auto-resume). Best-effort: failures
/// are logged at `warn` and don't fail the underlying lifecycle
/// command — the user can retry from the Ports panel. Skipped
/// when the workspace shell isn't running (we don't want to spin
/// up a sidecar pointing at a stopped dev).
pub(crate) async fn reapply_persisted_forwards(app: &AppCore, status: &ContainerStatus) {
	if !matches!(status.state, ContainerState::Running) {
		return;
	}
	let Some(id) = app.workspace_id() else {
		return;
	};
	let session = match core_session::load(&app.workspaces_dir, id).await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "could not load session for forward reapply");
			return;
		}
	};
	if session.forwarded_ports.is_empty() {
		return;
	}
	let project = match project_name_for_id(id) {
		Ok(p) => p,
		Err(err) => {
			tracing::warn!(error = %err, "could not resolve project name for forward reapply");
			return;
		}
	};
	if let Err(err) = apply_forwards(&project, &session.forwarded_ports).await {
		tracing::warn!(error = %err, "forward reapply after lifecycle event failed");
		return;
	}
	let payload = match list_forward_status(&project, &session.forwarded_ports).await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "could not compute forward status after reapply");
			return;
		}
	};
	emit(app.events.as_ref(), PORTS_STATE_EVENT, &payload);
}

/// Pure query — does not emit. The UI polls this on focus and
/// after long-running operations the user might have invoked
/// outside the IDE.
pub async fn container_status(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	Ok(container.status().await?)
}

/// First-time opt-in: regenerate `compose.yaml` from the current
/// bound-folder set, then `docker compose up -d --wait`.
pub async fn container_setup(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.setup(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(app).await;
	let status = snapshot_and_emit(app, &container).await?;
	reapply_persisted_forwards(app, &status).await;
	Ok(status)
}

pub async fn container_pause(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.pause().await?;
	reset_lsp_broker(app).await;
	snapshot_and_emit(app, &container).await
}

pub async fn container_resume(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.resume().await?;
	reset_lsp_broker(app).await;
	snapshot_and_emit(app, &container).await
}

pub async fn container_rebuild(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.rebuild(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(app).await;
	let status = snapshot_and_emit(app, &container).await?;
	reapply_persisted_forwards(app, &status).await;
	Ok(status)
}

pub async fn container_stop(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.stop().await?;
	reset_lsp_broker(app).await;
	snapshot_and_emit(app, &container).await
}

pub async fn container_teardown(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.teardown().await?;
	reset_lsp_broker(app).await;
	snapshot_and_emit(app, &container).await
}

/// Re-emit `compose.yaml` + `bound-folders.json` from the current
/// bound-folder set, and — if the project is running — apply the
/// diff with `docker compose up -d --wait`.
///
/// Called by the frontend after every successful folder add /
/// remove. Idempotent.
pub async fn container_apply_bound_folders(app: &AppCore) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(app).await?;
	container.apply_bound_folders(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(app).await;
	let status = snapshot_and_emit(app, &container).await?;
	reapply_persisted_forwards(app, &status).await;
	Ok(status)
}

/// Render what `compose.yaml` *would* contain without writing it.
pub async fn container_render_compose(app: &AppCore) -> Result<String, MoonError> {
	let container = workspace_handle(app).await?;
	Ok(container.render_compose(DEFAULT_DEV_IMAGE).yaml)
}

crate::commands! {
	container_status([app]),
	container_setup([app]),
	container_pause([app]),
	container_resume([app]),
	container_rebuild([app]),
	container_stop([app]),
	container_teardown([app]),
	container_apply_bound_folders([app]),
	container_render_compose([app]),
}
