//! Tauri commands wrapping per-folder
//! [`moon_container::ProjectCompose`].
//!
//! Each command takes the absolute path of a bound folder and
//! shells out to `docker compose -f <folder>/<compose> -p
//! moon-ws-<id>-<slug> ...`. The compose project name is derived
//! deterministically from the workspace id + folder basename, so
//! repeated calls always target the same project on the daemon.
//!
//! Lifecycle-mutating commands emit a [`PROJECT_COMPOSE_STATE_EVENT`]
//! after they finish, keyed on `folder_path` — the UI subscribes
//! per folder bar so a per-folder mutation only refreshes its own
//! row.

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::ProjectCompose;
use moon_protocol::container::{ContainerState, ContainerStatus, ProjectComposeStateChange, ProjectComposeStatus};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Emitted after every successful per-folder lifecycle command.
/// Payload: [`ProjectComposeStateChange`].
pub const PROJECT_COMPOSE_STATE_EVENT: &str = "project_compose:state";

/// Resolve a [`ProjectCompose`] for the given folder under the
/// active workspace.
///
/// Returns the workspace ID (for the event payload) alongside the
/// resolved handle. `Ok(None)` for the second return slot means
/// "the folder is bound but has no compose file at its root";
/// callers handle that case by reporting an `Absent` snapshot
/// rather than erroring — that's how the UI tells "no compose"
/// from "compose down".
async fn project_handle(
	state: &AppState,
	folder_path: &Utf8Path,
) -> Result<(String, Option<ProjectCompose>), MoonError> {
	let snapshot = state.workspaces.snapshot().await;
	let workspace_id = snapshot.id.clone();
	let bound = snapshot.folders.iter().any(|f| Utf8Path::new(&f.path) == folder_path);
	if !bound {
		return Err(MoonError::NotFound(format!("folder {folder_path}")));
	}
	let pc = ProjectCompose::for_folder(&workspace_id, folder_path)?;
	Ok((workspace_id, pc))
}

/// Project handle in the "must exist" form. Used by mutating
/// commands — calling `up` on a folder that hasn't got a compose
/// file is a programming error in the UI, not a runtime
/// condition we want to silently swallow.
async fn require_project_handle(
	state: &AppState,
	folder_path: &Utf8Path,
) -> Result<(String, ProjectCompose), MoonError> {
	let (workspace_id, pc) = project_handle(state, folder_path).await?;
	let pc = pc.ok_or_else(|| MoonError::NotFound(format!("no compose file in {folder_path}")))?;
	Ok((workspace_id, pc))
}

fn make_status(folder_path: &Utf8Path, pc: Option<&ProjectCompose>, status: ContainerStatus) -> ProjectComposeStatus {
	match pc {
		Some(pc) => ProjectComposeStatus {
			folder_path: folder_path.to_string(),
			compose_file: Some(pc.compose_file().to_string()),
			project_name: Some(pc.project().as_str().to_owned()),
			status,
		},
		None => ProjectComposeStatus {
			folder_path: folder_path.to_string(),
			compose_file: None,
			project_name: None,
			status: ContainerStatus {
				state: ContainerState::Absent,
				services: Vec::new(),
			},
		},
	}
}

/// Snapshot status and broadcast the resulting
/// `project_compose:state` event. Called by every mutating
/// command after it completes; emit failures only get a warn
/// (the command result is the authoritative success signal).
async fn snapshot_and_emit(
	app: &AppHandle,
	workspace_id: String,
	folder_path: &Utf8Path,
	pc: &ProjectCompose,
) -> Result<ProjectComposeStatus, MoonError> {
	let status = pc.status().await?;
	let project = make_status(folder_path, Some(pc), status);
	let payload = ProjectComposeStateChange {
		workspace_id,
		folder_path: folder_path.to_string(),
		project: project.clone(),
	};
	if let Err(err) = app.emit(PROJECT_COMPOSE_STATE_EVENT, &payload) {
		tracing::warn!(error = %err, "failed to emit project_compose:state");
	}
	Ok(project)
}

/// Pure query — does not emit. The folder bar polls this on
/// focus + after long-running operations.
///
/// Returns an `Absent` snapshot with `compose_file: None` when
/// the folder has no root compose file (the indicator stays
/// hidden in the UI for those folders).
#[tauri::command]
pub async fn project_compose_status(
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (_, pc) = project_handle(&state, &folder_path).await?;
	let status = match &pc {
		Some(pc) => pc.status().await?,
		None => ContainerStatus {
			state: ContainerState::Absent,
			services: Vec::new(),
		},
	};
	Ok(make_status(&folder_path, pc.as_ref(), status))
}

#[tauri::command]
pub async fn project_compose_up(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.up().await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

#[tauri::command]
pub async fn project_compose_pause(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.pause().await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

#[tauri::command]
pub async fn project_compose_resume(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.resume().await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

#[tauri::command]
pub async fn project_compose_rebuild(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.rebuild().await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

#[tauri::command]
pub async fn project_compose_down(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.down().await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

/// `docker compose start <service>` — bring a single created /
/// stopped service into `running` without recreating.
#[tauri::command]
pub async fn project_compose_service_start(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.start_service(&service).await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

/// `docker compose stop <service>` — SIGTERM a single service's
/// container while leaving its record on the daemon.
#[tauri::command]
pub async fn project_compose_service_stop(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.stop_service(&service).await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}

/// `docker compose restart <service>` — stop + start a single
/// service's container without recreating it. The cheap "did
/// gitaly flake, try again" knob.
#[tauri::command]
pub async fn project_compose_service_restart(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let (workspace_id, pc) = require_project_handle(&state, &folder_path).await?;
	pc.restart_service(&service).await?;
	snapshot_and_emit(&app, workspace_id, &folder_path, &pc).await
}
