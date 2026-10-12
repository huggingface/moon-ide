//! Tauri commands wrapping `moon-container`.
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

use moon_container::{
	apply_forwards, list_forward_status, project_name_for_id, Workspace as ContainerWorkspace, DEFAULT_DEV_IMAGE,
};
use moon_core::session as core_session;
use moon_protocol::container::{ContainerState, ContainerStateChange, ContainerStatus};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::commands::ports::PORTS_STATE_EVENT;
use crate::state::AppState;

/// Emitted after every successful lifecycle command. Payload:
/// [`ContainerStateChange`].
pub const CONTAINER_STATE_EVENT: &str = "container:state";

/// Snapshot status and broadcast the resulting `container:state`
/// event. Called by every mutating command after it completes.
async fn snapshot_and_emit(app: &AppHandle, container: &ContainerWorkspace) -> Result<ContainerStatus, MoonError> {
	let status = container.status().await?;
	emit_container_state(app, &status);
	Ok(status)
}

/// Broadcast a `container:state` event for an already-fetched
/// status. Shared with the startup auto-resume path, which owns
/// its status query rather than going through a mutating command.
pub(crate) fn emit_container_state(app: &AppHandle, status: &ContainerStatus) {
	let payload = ContainerStateChange { status: status.clone() };
	if let Err(err) = app.emit(CONTAINER_STATE_EVENT, &payload) {
		tracing::warn!(error = %err, "failed to emit container:state");
	}
}

/// Drop any live LSP broker after a container mutation. The next
/// `lsp_open` call rebuilds against the current state (container
/// up → `DockerExec` spawner; container down → host fallback).
async fn reset_lsp_broker(state: &AppState) {
	let handle = state.lsp.lock().await.take();
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
pub(crate) async fn reapply_persisted_forwards(app: &AppHandle, state: &AppState, status: &ContainerStatus) {
	if !matches!(status.state, ContainerState::Running) {
		return;
	}
	let Some(id) = state.workspace_id() else {
		return;
	};
	let session = match core_session::load(&state.workspaces_dir, id).await {
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
	if let Err(err) = app.emit(PORTS_STATE_EVENT, &payload) {
		tracing::warn!(error = %err, "failed to emit ports:state after reapply");
	}
}

/// Pure query — does not emit. The UI polls this on focus and
/// after long-running operations the user might have invoked
/// outside the IDE.
#[tauri::command]
pub async fn container_status(state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	Ok(container.status().await?)
}

/// First-time opt-in: regenerate `compose.yaml` from the current
/// bound-folder set, then `docker compose up -d --wait`.
#[tauri::command]
pub async fn container_setup(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.setup(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(&state).await;
	let status = snapshot_and_emit(&app, &container).await?;
	reapply_persisted_forwards(&app, &state, &status).await;
	Ok(status)
}

#[tauri::command]
pub async fn container_pause(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.pause().await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_resume(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.resume().await?;
	reset_lsp_broker(&state).await;
	let status = snapshot_and_emit(&app, &container).await?;
	Ok(status)
}

#[tauri::command]
pub async fn container_rebuild(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.rebuild(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(&state).await;
	let status = snapshot_and_emit(&app, &container).await?;
	reapply_persisted_forwards(&app, &state, &status).await;
	Ok(status)
}

#[tauri::command]
pub async fn container_stop(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.stop().await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_teardown(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.teardown().await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

/// Re-emit `compose.yaml` + `bound-folders.json` from the current
/// bound-folder set, and — if the project is running — apply the
/// diff with `docker compose up -d --wait`.
///
/// Called by the frontend after every successful folder add /
/// remove. Idempotent.
#[tauri::command]
pub async fn container_apply_bound_folders(
	app: AppHandle,
	state: State<'_, AppState>,
) -> Result<ContainerStatus, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	container.apply_bound_folders(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(&state).await;
	let status = snapshot_and_emit(&app, &container).await?;
	reapply_persisted_forwards(&app, &state, &status).await;
	Ok(status)
}

/// Render what `compose.yaml` *would* contain without writing it.
#[tauri::command]
pub async fn container_render_compose(state: State<'_, AppState>) -> Result<String, MoonError> {
	let container = moon_app::container::workspace_handle(&state.core).await?;
	Ok(container.render_compose(DEFAULT_DEV_IMAGE).yaml)
}
