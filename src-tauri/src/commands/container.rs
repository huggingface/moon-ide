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

use camino::Utf8PathBuf;
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig, DEFAULT_DEV_IMAGE};
use moon_protocol::container::{ContainerStateChange, ContainerStatus};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Emitted after every successful lifecycle command. Payload:
/// [`ContainerStateChange`].
pub const CONTAINER_STATE_EVENT: &str = "container:state";

/// Build the container workspace handle for this process's
/// workspace from the current bound-folder set.
async fn workspace_handle(state: &AppState) -> Result<ContainerWorkspace, MoonError> {
	let snapshot = state.workspaces.snapshot().await;
	let bound_folders: Vec<Utf8PathBuf> = snapshot.folders.iter().map(|f| Utf8PathBuf::from(&f.path)).collect();
	let state_dir = state.workspace_state_dir(&snapshot.id);
	let container = ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: snapshot.id,
		state_dir,
		bound_folders,
	})?;
	Ok(container)
}

/// Snapshot status and broadcast the resulting `container:state`
/// event. Called by every mutating command after it completes.
async fn snapshot_and_emit(app: &AppHandle, container: &ContainerWorkspace) -> Result<ContainerStatus, MoonError> {
	let status = container.status().await?;
	let payload = ContainerStateChange { status: status.clone() };
	if let Err(err) = app.emit(CONTAINER_STATE_EVENT, &payload) {
		tracing::warn!(error = %err, "failed to emit container:state");
	}
	Ok(status)
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

/// Pure query — does not emit. The UI polls this on focus and
/// after long-running operations the user might have invoked
/// outside the IDE.
#[tauri::command]
pub async fn container_status(state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
	Ok(container.status().await?)
}

/// First-time opt-in: regenerate `compose.yaml` from the current
/// bound-folder set, then `docker compose up -d --wait`.
#[tauri::command]
pub async fn container_setup(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
	container.setup(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_pause(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
	container.pause().await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_resume(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
	container.resume().await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_rebuild(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
	container.rebuild(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_stop(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
	container.stop().await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

#[tauri::command]
pub async fn container_teardown(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let container = workspace_handle(&state).await?;
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
	let container = workspace_handle(&state).await?;
	container.apply_bound_folders(DEFAULT_DEV_IMAGE).await?;
	reset_lsp_broker(&state).await;
	snapshot_and_emit(&app, &container).await
}

/// Render what `compose.yaml` *would* contain without writing it.
#[tauri::command]
pub async fn container_render_compose(state: State<'_, AppState>) -> Result<String, MoonError> {
	let container = workspace_handle(&state).await?;
	Ok(container.render_compose(DEFAULT_DEV_IMAGE).yaml)
}
