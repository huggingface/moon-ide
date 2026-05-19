//! Per-service `docker compose logs -f` streaming.
//!
//! The bottom panel's log tab kind opens a stream by calling
//! [`compose_logs_open`], which:
//!
//! 1. Resolves the per-folder [`ProjectCompose`] handle so we have the
//!    user's compose file and the daemon project name.
//! 2. Spawns `docker compose -f <file> -p <project> logs -f
//!    --tail 500 <service>` with `kill_on_drop(true)` so the
//!    child can't outlive its supervisor task.
//! 3. Generates a stream UUID and supervisor task. The task pipes
//!    stdout + stderr through Tauri events keyed on the UUID, then
//!    awaits the child's exit and emits a final `closed` event.
//! 4. Stores the supervisor's [`AbortHandle`] in
//!    [`AppState::log_streams`] keyed on the UUID and hands the
//!    UUID back to the frontend.
//!
//! The frontend later calls [`compose_logs_close`] to abort the
//! supervisor (which drops the child → SIGKILL) and remove the
//! registry entry. There's intentionally no shared "follow tail"
//! flag: that's UI concern, not lifecycle, and the frontend keeps
//! its own buffer of received lines.
//!
//! Why one process per stream
//! --------------------------
//!
//! `docker compose logs -f` doesn't have a way to add or drop
//! services from a running invocation — even a multi-service log
//! tail is a single, fixed list of services from spawn to kill.
//! Spawning one child per opened tab keeps the model simple and
//! lets us close one tab without disturbing the others. The
//! per-stream supervisor task also keeps the event-emission
//! ordering per-stream so the frontend's buffer can rely on
//! line order without sequencing in payloads.

use std::process::Stdio;

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::ProjectCompose;
use moon_protocol::container::{LogStreamClosed, LogStreamLine};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

use crate::state::AppState;

/// Per-line event name. Payload is [`LogStreamLine`].
pub const LOG_STREAM_LINE_EVENT: &str = "compose_logs:line";

/// Emitted once when the underlying child exits (clean or otherwise).
/// Payload is [`LogStreamClosed`].
pub const LOG_STREAM_CLOSED_EVENT: &str = "compose_logs:closed";

/// Lines requested from the historical tail before live streaming
/// kicks in. 500 covers most "what just happened?" debug
/// scenarios; users who want more history can re-run
/// `docker compose logs` directly.
const TAIL_LINES: &str = "500";

/// Open a streaming log tail for `service` in the compose project
/// at `folder_path`. Returns the stream UUID; the frontend uses it
/// to subscribe to `compose_logs:*` events and to call
/// [`compose_logs_close`] when the user closes the tab.
#[tauri::command]
pub async fn compose_logs_open(
	app: AppHandle,
	state: State<'_, AppState>,
	folder_path: String,
	service: String,
) -> Result<String, MoonError> {
	let folder_path = Utf8PathBuf::from(folder_path);
	let pc = require_project_handle(&state, &folder_path).await?;

	let stream_id = Uuid::new_v4().to_string();
	let handle = spawn_supervisor(app, state.log_streams.clone(), stream_id.clone(), pc, service).await?;
	state.log_streams.lock().await.insert(stream_id.clone(), handle);
	Ok(stream_id)
}

/// Close a previously-opened stream by aborting its supervisor.
/// The supervisor's child was spawned with `kill_on_drop(true)`,
/// so the docker process gets SIGKILL'd as the abort drops it.
#[tauri::command]
pub async fn compose_logs_close(state: State<'_, AppState>, stream_id: String) -> Result<(), MoonError> {
	let handle = state.log_streams.lock().await.remove(&stream_id);
	if let Some(handle) = handle {
		handle.abort();
	}
	Ok(())
}

/// Tauri command resolver mirror of `project_compose::require_project_handle`,
/// duplicated locally to keep this module self-contained.
async fn require_project_handle(state: &AppState, folder_path: &Utf8Path) -> Result<ProjectCompose, MoonError> {
	let snapshot = state.workspaces.snapshot().await;
	let bound = snapshot.folders.iter().any(|f| Utf8Path::new(&f.path) == folder_path);
	if !bound {
		return Err(MoonError::NotFound(format!("folder {folder_path}")));
	}
	let state_dir = state.workspace_state_dir(&snapshot.id);
	let pc = ProjectCompose::for_folder(&snapshot.id, &state_dir, folder_path)?
		.ok_or_else(|| MoonError::NotFound(format!("no compose file in {folder_path}")))?;
	Ok(pc)
}

async fn spawn_supervisor(
	app: AppHandle,
	registry: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>>,
	stream_id: String,
	pc: ProjectCompose,
	service: String,
) -> Result<tokio::task::AbortHandle, MoonError> {
	let mut child = Command::new("docker")
		.arg("compose")
		.arg("-f")
		.arg(pc.compose_file().as_str())
		.arg("-p")
		.arg(pc.project().as_str())
		.arg("logs")
		.arg("-f")
		.arg("--no-color")
		.arg("--tail")
		.arg(TAIL_LINES)
		.arg(&service)
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true)
		.spawn()
		.map_err(|err| MoonError::internal(format!("spawn `docker compose logs`: {err}")))?;

	let stdout = child
		.stdout
		.take()
		.ok_or_else(|| MoonError::internal("missing stdout pipe"))?;
	let stderr = child
		.stderr
		.take()
		.ok_or_else(|| MoonError::internal("missing stderr pipe"))?;

	let app_clone = app.clone();
	let id_for_task = stream_id.clone();
	let supervisor = tokio::spawn(async move {
		// Move the child into the task so it gets dropped (and
		// SIGKILL'd via `kill_on_drop`) when the task is aborted
		// from `compose_logs_close`.
		let mut child = child;

		// Stdout reader. Detached: the supervisor doesn't await
		// it, both readers finish naturally when the docker
		// process closes its pipes, and emit failures only happen
		// when the window itself is gone — we ignore them so a
		// closed window can't leak the loop.
		let stdout_task = {
			let app = app_clone.clone();
			let id = id_for_task.clone();
			tokio::spawn(async move {
				let mut lines = BufReader::new(stdout).lines();
				while let Ok(Some(text)) = lines.next_line().await {
					let _ = app.emit(
						LOG_STREAM_LINE_EVENT,
						&LogStreamLine {
							stream_id: id.clone(),
							channel: "stdout".to_owned(),
							text,
						},
					);
				}
			})
		};
		let stderr_task = {
			let app = app_clone.clone();
			let id = id_for_task.clone();
			tokio::spawn(async move {
				let mut lines = BufReader::new(stderr).lines();
				while let Ok(Some(text)) = lines.next_line().await {
					let _ = app.emit(
						LOG_STREAM_LINE_EVENT,
						&LogStreamLine {
							stream_id: id.clone(),
							channel: "stderr".to_owned(),
							text,
						},
					);
				}
			})
		};

		let exit = child.wait().await;
		let _ = stdout_task.await;
		let _ = stderr_task.await;

		// Drop the registry entry first so a frontend close call
		// arriving simultaneously can't race us into a double
		// abort on a stale handle.
		registry.lock().await.remove(&id_for_task);

		let code = exit.ok().and_then(|s| s.code());
		let _ = app_clone.emit(
			LOG_STREAM_CLOSED_EVENT,
			&LogStreamClosed {
				stream_id: id_for_task,
				code,
			},
		);
	});

	Ok(supervisor.abort_handle())
}
