//! Tauri commands wrapping `moon-container`.
//!
//! Phase 2.0 surface: snapshot the active workspace's compose
//! project, drive its lifecycle (set up, pause, resume, rebuild,
//! tear down), and preview the would-be `.moon/compose.yaml`
//! before committing to "Set up".
//!
//! Every command resolves the active workspace via
//! [`crate::state::AppState::workspaces`], so the frontend never
//! has to thread a workspace id through the call site — the same
//! shape the `fs_*` and `search_*` commands use.
//!
//! Lifecycle-mutating commands (everything but `container_status`
//! and `container_render_compose`) emit a [`CONTAINER_STATE_EVENT`]
//! after they finish so other windows / panes that subscribed
//! stay in lockstep without polling.

use camino::Utf8PathBuf;
use moon_container::{Workspace as ContainerWorkspace, DEFAULT_DEV_IMAGE};
use moon_protocol::container::{ContainerStateChange, ContainerStatus};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Emitted after every successful lifecycle command. Payload:
/// [`ContainerStateChange`].
pub const CONTAINER_STATE_EVENT: &str = "container:state";

/// Resolve the active workspace and turn it into a
/// [`ContainerWorkspace`] handle. Returns the workspace ID
/// alongside so the broadcast helper can label the event without
/// a second registry lookup.
async fn workspace_handle(state: &AppState) -> Result<(String, ContainerWorkspace), MoonError> {
	let ws = state.workspaces.require_active().await?;
	let root = Utf8PathBuf::from(&ws.record.root);
	Ok((ws.record.id.clone(), ContainerWorkspace::for_root(root)))
}

/// Snapshot status and broadcast the resulting `container:state`
/// event. Called by every mutating command after it completes.
///
/// Emit failures only get a warn — the command's `Result` is the
/// authoritative success/failure signal; events are best-effort
/// fan-out.
async fn snapshot_and_emit(
	app: &AppHandle,
	workspace_id: String,
	container: &ContainerWorkspace,
) -> Result<ContainerStatus, MoonError> {
	let status = container.status().await?;
	let payload = ContainerStateChange {
		workspace_id,
		status: status.clone(),
	};
	if let Err(err) = app.emit(CONTAINER_STATE_EVENT, &payload) {
		tracing::warn!(error = %err, "failed to emit container:state");
	}
	Ok(status)
}

/// Pure query — does not emit. The UI polls this on focus and
/// after long-running operations the user might have invoked
/// outside the IDE.
#[tauri::command]
pub async fn container_status(state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (_, container) = workspace_handle(&state).await?;
	Ok(container.status().await?)
}

/// First-time opt-in: generate `.moon/compose.yaml` if missing,
/// then `docker compose up -d --wait`. The await blocks until
/// every service is healthy or one has failed — exactly what the
/// "Set up" button promises.
#[tauri::command]
pub async fn container_setup(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.setup(DEFAULT_DEV_IMAGE).await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

#[tauri::command]
pub async fn container_pause(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.pause().await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

#[tauri::command]
pub async fn container_resume(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.resume().await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

#[tauri::command]
pub async fn container_rebuild(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.rebuild().await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

#[tauri::command]
pub async fn container_teardown(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.teardown().await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

/// Render what `<workspace>/.moon/compose.yaml` *would* contain
/// without writing it. Backs an "Inspect compose.yaml" affordance
/// for users who want to see what `container_setup` will commit
/// to disk before they click.
#[tauri::command]
pub async fn container_render_compose(state: State<'_, AppState>) -> Result<String, MoonError> {
	let (_, container) = workspace_handle(&state).await?;
	Ok(container.render_compose(DEFAULT_DEV_IMAGE).yaml)
}
