//! Tauri commands wrapping `moon-container`.
//!
//! Phase 2.0 surface: snapshot the active workspace's compose
//! project, drive its lifecycle (set up, pause, resume, rebuild,
//! tear down), and preview the would-be `compose.yaml` before
//! committing to "Set up".
//!
//! Post-2.5 the workspace's identity is decoupled from any
//! specific folder — the compose project (`moon-ws-<id>`)
//! survives folder switches; only its bound-mount set changes
//! when folders are added or removed. The
//! [`container_apply_bound_folders`] command lets the frontend
//! re-emit `compose.yaml` after a folder add/remove and
//! transparently apply the diff if the project happens to be
//! running.
//!
//! Lifecycle-mutating commands (everything but `container_status`
//! and `container_render_compose`) emit a [`CONTAINER_STATE_EVENT`]
//! after they finish so other windows / panes that subscribed
//! stay in lockstep without polling.

use camino::Utf8PathBuf;
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig, DEFAULT_DEV_IMAGE};
use moon_protocol::container::{ContainerStateChange, ContainerStatus};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Emitted after every successful lifecycle command. Payload:
/// [`ContainerStateChange`].
pub const CONTAINER_STATE_EVENT: &str = "container:state";

/// Build the container workspace handle from current app state —
/// workspace id, bound-folder list, per-workspace state dir.
///
/// Returns the workspace ID alongside so the broadcast helper
/// can label the event without a second registry lookup. Doesn't
/// touch disk; cheap to call per command.
async fn workspace_handle(state: &AppState) -> Result<(String, ContainerWorkspace), MoonError> {
	let snapshot = state.workspaces.snapshot().await;
	let workspace_id = snapshot.id.clone();
	let bound_folders: Vec<Utf8PathBuf> = snapshot.folders.iter().map(|f| Utf8PathBuf::from(&f.path)).collect();
	let state_dir = state.workspace_state_dir(&workspace_id);
	let container = ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: workspace_id.clone(),
		state_dir,
		bound_folders,
	})?;
	Ok((workspace_id, container))
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

/// First-time opt-in: regenerate `compose.yaml` from the current
/// bound-folder set, then `docker compose up -d --wait`. The await
/// blocks until every service is healthy or one has failed —
/// exactly what the "Set up" button promises.
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
	container.rebuild(DEFAULT_DEV_IMAGE).await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

#[tauri::command]
pub async fn container_stop(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.stop().await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

#[tauri::command]
pub async fn container_teardown(app: AppHandle, state: State<'_, AppState>) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	container.teardown().await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

/// Re-emit `compose.yaml` + `bound-folders.json` from the current
/// bound-folder set, and — if the project is running — apply the
/// diff with `docker compose up -d --wait`.
///
/// Called by the frontend after every successful folder add /
/// remove. Idempotent: if the bound-folder set hasn't actually
/// changed and the project isn't running, this is a no-op.
/// Returns the post-call status so the pip can refresh in the
/// same round trip.
#[tauri::command]
pub async fn container_apply_bound_folders(
	app: AppHandle,
	state: State<'_, AppState>,
) -> Result<ContainerStatus, MoonError> {
	let (workspace_id, container) = workspace_handle(&state).await?;
	// `apply_bound_folders` writes the file unconditionally and
	// only `compose up -d --wait`s if the project is currently
	// `Running`. That keeps add/remove cheap when the user
	// hasn't opted in yet (status: Absent) or has paused on
	// purpose.
	container.apply_bound_folders(DEFAULT_DEV_IMAGE).await?;
	snapshot_and_emit(&app, workspace_id, &container).await
}

/// Render what `compose.yaml` *would* contain without writing it.
/// Backs an "Inspect compose.yaml" affordance for users who want
/// to see what `container_setup` will commit to disk before they
/// click.
#[tauri::command]
pub async fn container_render_compose(state: State<'_, AppState>) -> Result<String, MoonError> {
	let (_, container) = workspace_handle(&state).await?;
	Ok(container.render_compose(DEFAULT_DEV_IMAGE).yaml)
}
