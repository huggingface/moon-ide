//! Commands wrapping per-folder
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

use crate::events::{emit, EventSink};
use crate::AppCore;

/// Emitted after every successful per-folder lifecycle command.
/// Payload: [`ProjectComposeStateChange`].
pub const PROJECT_COMPOSE_STATE_EVENT: &str = "project_compose:state";

/// Resolve a [`ProjectCompose`] for the given folder under this
/// process's workspace.
///
/// `Ok(None)` for the second return slot means "the folder is
/// bound but has no compose file at its root"; callers handle
/// that case by reporting an `Absent` snapshot rather than
/// erroring — that's how the UI tells "no compose" from
/// "compose down".
async fn project_handle(app: &AppCore, folder_path: &Utf8Path) -> Result<Option<ProjectCompose>, MoonError> {
	let snapshot = app.workspaces.snapshot().await;
	let bound = snapshot.folders.iter().any(|f| Utf8Path::new(&f.path) == folder_path);
	if !bound {
		return Err(MoonError::NotFound(format!("folder {folder_path}")));
	}
	let state_dir = app.workspaces_dir.join(&snapshot.id);
	let pc = ProjectCompose::for_folder(&snapshot.id, &state_dir, folder_path)?;
	Ok(pc)
}

/// Project handle in the "must exist" form. Used by mutating
/// commands — calling `up` on a folder that hasn't got a compose
/// file is a programming error in the UI, not a runtime
/// condition we want to silently swallow. Shared with
/// `compose_logs`, which tails a service of the same project.
pub(crate) async fn require_project_handle(app: &AppCore, folder_path: &Utf8Path) -> Result<ProjectCompose, MoonError> {
	project_handle(app, folder_path)
		.await?
		.ok_or_else(|| MoonError::NotFound(format!("no compose file in {folder_path}")))
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
/// command after it completes.
async fn snapshot_and_emit(
	app: &AppCore,
	folder_path: &Utf8Path,
	pc: &ProjectCompose,
) -> Result<ProjectComposeStatus, MoonError> {
	let status = pc.status().await?;
	Ok(emit_project_compose_state(
		app.events.as_ref(),
		folder_path,
		Some(pc),
		status,
	))
}

/// Broadcast a `project_compose:state` event for an already-fetched
/// status and return the snapshot it carried. Shared with the
/// startup auto-resume path.
pub(crate) fn emit_project_compose_state(
	events: &dyn EventSink,
	folder_path: &Utf8Path,
	pc: Option<&ProjectCompose>,
	status: ContainerStatus,
) -> ProjectComposeStatus {
	let project = make_status(folder_path, pc, status);
	let payload = ProjectComposeStateChange {
		folder_path: folder_path.to_string(),
		project: project.clone(),
	};
	emit(events, PROJECT_COMPOSE_STATE_EVENT, &payload);
	project
}

/// Pure query — does not emit. The folder bar polls this on
/// focus + after long-running operations.
///
/// Returns an `Absent` snapshot with `compose_file: None` when
/// the folder has no root compose file (the indicator stays
/// hidden in the UI for those folders).
pub async fn project_compose_status(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = project_handle(app, &folder_path).await?;
	let status = match &pc {
		Some(pc) => pc.status().await?,
		None => ContainerStatus {
			state: ContainerState::Absent,
			services: Vec::new(),
		},
	};
	Ok(make_status(&folder_path, pc.as_ref(), status))
}

pub async fn project_compose_up(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.up().await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

pub async fn project_compose_pause(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.pause().await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

pub async fn project_compose_resume(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.resume().await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

pub async fn project_compose_rebuild(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.rebuild().await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

/// `docker compose stop` — SIGTERM every service in the project
/// without removing the containers. The cheaper counterpart to
/// `project_compose_down`: a follow-up `up` resumes from the
/// same containers and skips the image pull / build steps.
///
/// Also clears this folder's `compose_auto_resume` flag in
/// `session.json` so a clean IDE quit afterwards doesn't
/// resurrect a project the user just deliberately stopped.
pub async fn project_compose_stop(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.stop().await?;
	if let Some(id) = app.workspace_id() {
		crate::lifecycle::clear_compose_auto_resume(app, id, folder_path.as_str()).await;
	}
	snapshot_and_emit(app, &folder_path, &pc).await
}

/// `docker compose down` — stop containers, remove containers,
/// networks, and the project entry. Also clears this folder's
/// `compose_auto_resume` flag (same reason as
/// [`project_compose_stop`]).
pub async fn project_compose_down(app: &AppCore, folder_path: String) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.down().await?;
	if let Some(id) = app.workspace_id() {
		crate::lifecycle::clear_compose_auto_resume(app, id, folder_path.as_str()).await;
	}
	snapshot_and_emit(app, &folder_path, &pc).await
}

/// `docker compose start <service>` — bring a single created /
/// stopped service into `running` without recreating.
pub async fn project_compose_service_start(
	app: &AppCore,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.start_service(&service).await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

/// `docker compose stop <service>` — SIGTERM a single service's
/// container while leaving its record on the daemon.
pub async fn project_compose_service_stop(
	app: &AppCore,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.stop_service(&service).await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

/// `docker compose restart <service>` — stop + start a single
/// service's container without recreating it. The cheap "did
/// gitaly flake, try again" knob.
pub async fn project_compose_service_restart(
	app: &AppCore,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.restart_service(&service).await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

/// `docker compose up -d --no-deps --force-recreate --pull always
/// <service>` — pull the latest image and recreate one service's
/// container, leaving the rest of the project untouched.
pub async fn project_compose_service_recreate(
	app: &AppCore,
	folder_path: String,
	service: String,
) -> Result<ProjectComposeStatus, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(app, &folder_path).await?;
	pc.recreate_service(&service).await?;
	snapshot_and_emit(app, &folder_path, &pc).await
}

crate::commands! {
	project_compose_status([app] folder_path: String),
	project_compose_up([app] folder_path: String),
	project_compose_pause([app] folder_path: String),
	project_compose_resume([app] folder_path: String),
	project_compose_rebuild([app] folder_path: String),
	project_compose_stop([app] folder_path: String),
	project_compose_down([app] folder_path: String),
	project_compose_service_start([app] folder_path: String, service: String),
	project_compose_service_stop([app] folder_path: String, service: String),
	project_compose_service_restart([app] folder_path: String, service: String),
	project_compose_service_recreate([app] folder_path: String, service: String),
}
