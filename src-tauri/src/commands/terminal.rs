//! Terminal session Tauri commands.
//!
//! Mirrors the [`compose_logs`] shape: each open call mints a
//! UUID, spawns a supervisor task, registers an `AbortHandle`
//! in [`AppState::terminal_streams`], and ferries IO over Tauri
//! events keyed on that UUID. Closing a tab on the frontend
//! aborts the supervisor; the `PtySession` is dropped, which
//! SIGKILLs the child (host shell or `docker exec`).
//!
//! See ADR 0009 for the wire-format rationale and the host /
//! container target split.
//!
//! [`compose_logs`]: crate::commands::compose_logs

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use camino::Utf8PathBuf;
use moon_protocol::terminal::{TerminalClosed, TerminalOpenRequest, TerminalOutput, TerminalTarget as ProtocolTarget};
use moon_protocol::MoonError;
use moon_terminal::{container_name_for_workspace, spawn, TerminalTarget};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::state::{AppState, TerminalCommand, TerminalStreamHandle};

/// Per-chunk event name. Payload is [`TerminalOutput`].
pub const TERMINAL_OUTPUT_EVENT: &str = "terminal:output";

/// Emitted once when the underlying child exits. Payload is
/// [`TerminalClosed`].
pub const TERMINAL_CLOSED_EVENT: &str = "terminal:closed";

/// Channel depth for inbound write/resize commands. Writes are
/// already small (xterm sends a few bytes per keystroke); 256
/// is more than enough headroom and bounds memory if a runaway
/// `cat /dev/urandom > /proc/self/fd/0` ever showed up.
const COMMAND_CHANNEL_DEPTH: usize = 256;

#[tauri::command]
pub async fn terminal_open(
	app: AppHandle,
	state: State<'_, AppState>,
	request: TerminalOpenRequest,
) -> Result<String, MoonError> {
	let target = into_internal_target(request.target, &state)?;

	let stream_id = Uuid::new_v4().to_string();
	let (cmd_tx, cmd_rx) = mpsc::channel::<TerminalCommand>(COMMAND_CHANNEL_DEPTH);

	// Spawn the PTY synchronously so an immediate failure (bad
	// shell path, missing container) surfaces as the open
	// command's error rather than a silent close event later.
	let session = spawn(&target, request.cols, request.rows).map_err(|e| MoonError::internal(e.to_string()))?;

	let registry = state.terminal_streams.clone();
	let supervisor = tokio::spawn(supervise(app, registry.clone(), stream_id.clone(), session, cmd_rx));

	registry.lock().await.insert(
		stream_id.clone(),
		TerminalStreamHandle {
			tx: cmd_tx,
			abort: supervisor.abort_handle(),
		},
	);
	Ok(stream_id)
}

#[tauri::command]
pub async fn terminal_write(state: State<'_, AppState>, stream_id: String, data: String) -> Result<(), MoonError> {
	let bytes = BASE64
		.decode(data.as_bytes())
		.map_err(|e| MoonError::invalid(format!("terminal_write: bad base64 payload: {e}")))?;
	let registry = state.terminal_streams.lock().await;
	let Some(handle) = registry.get(&stream_id) else {
		// Frontend is racing a close — drop silently.
		return Ok(());
	};
	// `try_send` rather than `send().await`: we hold the
	// registry mutex and don't want to await with it held.
	// The 256-deep channel makes a full queue unrealistic
	// for human typing.
	let _ = handle.tx.try_send(TerminalCommand::Write(bytes));
	Ok(())
}

#[tauri::command]
pub async fn terminal_resize(
	state: State<'_, AppState>,
	stream_id: String,
	cols: u16,
	rows: u16,
) -> Result<(), MoonError> {
	let registry = state.terminal_streams.lock().await;
	let Some(handle) = registry.get(&stream_id) else {
		return Ok(());
	};
	let _ = handle.tx.try_send(TerminalCommand::Resize { cols, rows });
	Ok(())
}

#[tauri::command]
pub async fn terminal_close(state: State<'_, AppState>, stream_id: String) -> Result<(), MoonError> {
	let handle = state.terminal_streams.lock().await.remove(&stream_id);
	if let Some(handle) = handle {
		handle.abort.abort();
	}
	Ok(())
}

fn into_internal_target(t: ProtocolTarget, state: &AppState) -> Result<TerminalTarget, MoonError> {
	match t {
		ProtocolTarget::Host { cwd } => Ok(TerminalTarget::Host {
			cwd: cwd.map(Utf8PathBuf::from),
			shell: None,
		}),
		ProtocolTarget::Container { cwd } => {
			let id = state
				.workspace_id()
				.ok_or_else(|| MoonError::invalid("terminal_open: container target requires a bound workspace"))?;
			Ok(TerminalTarget::Container {
				container_name: container_name_for_workspace(id),
				cwd: Utf8PathBuf::from(cwd),
				shell: None,
			})
		}
	}
}

/// Supervisor: pumps PTY output to Tauri events and inbound
/// commands (write/resize) into the PTY. Exits when the child
/// closes its master (EOF on `next_output`) or the registry
/// channel is dropped (frontend close call).
async fn supervise(
	app: AppHandle,
	registry: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, TerminalStreamHandle>>>,
	stream_id: String,
	mut session: moon_terminal::PtySession,
	mut cmd_rx: mpsc::Receiver<TerminalCommand>,
) {
	loop {
		tokio::select! {
			chunk = session.next_output() => {
				let Some(bytes) = chunk else {
					break;
				};
				let payload = TerminalOutput {
					stream_id: stream_id.clone(),
					data: BASE64.encode(&bytes),
				};
				if app.emit(TERMINAL_OUTPUT_EVENT, &payload).is_err() {
					// Window's gone; stop the loop so we drop
					// the session and SIGKILL the child.
					break;
				}
			}
			cmd = cmd_rx.recv() => {
				let Some(cmd) = cmd else {
					break;
				};
				match cmd {
					TerminalCommand::Write(bytes) => {
						if let Err(e) = session.write(&bytes).await {
							tracing::warn!(stream_id = %stream_id, error = %e, "terminal write failed");
						}
					}
					TerminalCommand::Resize { cols, rows } => {
						if let Err(e) = session.resize(cols, rows).await {
							tracing::warn!(stream_id = %stream_id, error = %e, "terminal resize failed");
						}
					}
				}
			}
		}
	}

	// Take the exit code if the child has surfaced one. We poll
	// once after the loop ends — if the supervisor exited
	// because of a frontend close (registry drop), the child
	// may not have fully exited yet, but `PtySession::drop`
	// will SIGKILL it shortly.
	let code = session.next_exit().await;
	drop(session);

	registry.lock().await.remove(&stream_id);

	let _ = app.emit(
		TERMINAL_CLOSED_EVENT,
		&TerminalClosed {
			stream_id: stream_id.clone(),
			code,
		},
	);
}
