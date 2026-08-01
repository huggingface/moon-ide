//! Terminal session Tauri commands.
//!
//! Mirrors the [`compose_logs`] shape: each open call mints a
//! UUID, spawns a supervisor task, registers an `AbortHandle`
//! in [`AppState::terminal_streams`], and ferries IO over Tauri
//! events keyed on that UUID. Closing a tab on the frontend
//! aborts the supervisor; the `PtySession` is dropped, which
//! SIGKILLs the child (host shell or `docker exec`).
//!
//! Each open terminal is also recorded in
//! [`AppState::terminals`](crate::state::AppState::terminals) —
//! its target, cwd, owning project, and a bounded ring of its raw
//! output — so the coder's `list_terminals` / `read_terminal`
//! tools can inspect the terminals of the project they're working
//! in (ADR 0048). Registration tracks the *tab*: an exited shell
//! stays readable and is dropped on `terminal_close`.
//!
//! See ADR 0009 for the wire-format rationale and the host /
//! container target split.
//!
//! [`compose_logs`]: crate::commands::compose_logs

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use camino::Utf8PathBuf;
use moon_protocol::terminal::{
	TerminalCloseReason, TerminalClosed, TerminalOpenRequest, TerminalOutput, TerminalTarget as ProtocolTarget,
};
use moon_protocol::MoonError;
use moon_terminal::{
	container_name_for_workspace, container_running, editor_forward_env_for_workspace, spawn, TerminalKind,
	TerminalRegistration, TerminalRegistry, TerminalTarget,
};
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
	// For Container targets we also gather the bound-folder list,
	// which the editor-forward env vars (`MOON_EDIT_PATH_MAP`)
	// need. The snapshot read is async so we do it up here and
	// pass the result into the sync target builder.
	let bound_folders = if matches!(request.target, ProtocolTarget::Container { .. }) {
		state
			.workspaces
			.snapshot()
			.await
			.folders
			.into_iter()
			.map(|f| Utf8PathBuf::from(f.path))
			.collect::<Vec<_>>()
	} else {
		Vec::new()
	};
	let registration = registration_for(&request);
	let target = into_internal_target(request.target, &state, &bound_folders)?;

	let stream_id = Uuid::new_v4().to_string();
	let (cmd_tx, cmd_rx) = mpsc::channel::<TerminalCommand>(COMMAND_CHANNEL_DEPTH);

	// Spawn the PTY synchronously so an immediate failure (bad
	// shell path, missing container) surfaces as the open
	// command's error rather than a silent close event later. A
	// `command` in the request is typed into the fresh shell
	// (restart / session replay) — see `moon_terminal::spawn`.
	let session = spawn(&target, request.cols, request.rows, request.command.as_deref())
		.map_err(|e| MoonError::internal(e.to_string()))?;

	// Register before the supervisor starts so no output chunk can
	// race ahead of the entry it belongs in.
	state.terminals.register(&stream_id, registration).await;

	// What the supervisor needs to classify the close at the end:
	// host shells always classify `ShellExited`; container
	// terminals get their `docker exec` output probed and their
	// container's liveness checked.
	let container_name = match &target {
		TerminalTarget::Host { .. } => None,
		TerminalTarget::Container { container_name, .. } => Some(container_name.clone()),
	};

	let registry = state.terminal_streams.clone();
	let supervisor = tauri::async_runtime::spawn(supervise(
		app,
		registry.clone(),
		state.terminals.clone(),
		stream_id.clone(),
		session,
		cmd_rx,
		container_name,
	));

	registry.lock().await.insert(
		stream_id.clone(),
		TerminalStreamHandle {
			tx: cmd_tx,
			abort: supervisor.inner().abort_handle(),
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
	// Mirror the new size onto the registry entry so a coder read
	// renders the terminal at the width the user is looking at.
	state.terminals.record_resize(&stream_id, cols, rows).await;
	let registry = state.terminal_streams.lock().await;
	let Some(handle) = registry.get(&stream_id) else {
		return Ok(());
	};
	let _ = handle.tx.try_send(TerminalCommand::Resize { cols, rows });
	Ok(())
}

/// Clear the shell's current line and type `command` into it, as
/// if the user had typed it. Distinct from `terminal_write` because
/// the clear-then-type sequencing (Ctrl+C, wait for the fresh
/// prompt, then the bytes) must not interleave with regular
/// keystrokes — the supervisor owns it. Used by the session-replay
/// path when a persisted terminal's shell turns out to have
/// survived the container blip that triggered the replay.
#[tauri::command]
pub async fn terminal_rerun_command(
	state: State<'_, AppState>,
	stream_id: String,
	command: String,
) -> Result<(), MoonError> {
	if command.is_empty() {
		return Ok(());
	}
	let handle = state
		.terminal_streams
		.lock()
		.await
		.get(&stream_id)
		.map(|h| h.tx.clone());
	let Some(tx) = handle else {
		return Ok(());
	};
	// `send().await`, not `try_send`: the channel holds the full
	// clear-then-type payload as one item, and a keystroke burst
	// filling all 256 slots right now would silently drop the
	// replay otherwise.
	let _ = tx.send(TerminalCommand::RerunCommand(command)).await;
	Ok(())
}

#[tauri::command]
pub async fn terminal_close(state: State<'_, AppState>, stream_id: String) -> Result<(), MoonError> {
	let handle = state.terminal_streams.lock().await.remove(&stream_id);
	if let Some(handle) = handle {
		handle.abort.abort();
	}
	// The tab is gone, so its scrollback is no longer something the
	// user can see either — drop it rather than leaving output the
	// coder could still read. Aborting the supervisor skips its own
	// cleanup tail, so this is the only place a user-closed terminal
	// gets forgotten.
	state.terminals.forget(&stream_id).await;
	Ok(())
}

/// Metadata the registry keeps for a terminal, taken off the open
/// request before `target` is consumed by the internal conversion.
/// `cwd` is recorded in the target's own path space — a host path
/// for host terminals, an in-container path for container ones —
/// which is what a reader needs to make sense of the shell's output.
fn registration_for(request: &TerminalOpenRequest) -> TerminalRegistration {
	let (kind, cwd) = match &request.target {
		ProtocolTarget::Host { cwd } => (TerminalKind::Host, cwd.clone().unwrap_or_else(|| "~".to_owned())),
		ProtocolTarget::Container { cwd } => (TerminalKind::Container, cwd.clone()),
	};
	TerminalRegistration {
		kind,
		cwd,
		folder: request.folder.as_deref().map(Utf8PathBuf::from),
		cols: request.cols,
		rows: request.rows,
	}
}

fn into_internal_target(
	t: ProtocolTarget,
	state: &AppState,
	bound_folders: &[Utf8PathBuf],
) -> Result<TerminalTarget, MoonError> {
	match t {
		ProtocolTarget::Host { cwd } => Ok(TerminalTarget::Host {
			cwd: cwd.map(Utf8PathBuf::from),
			shell: None,
		}),
		ProtocolTarget::Container { cwd } => {
			let id = state
				.workspace_id()
				.ok_or_else(|| MoonError::invalid("terminal_open: container target requires a bound workspace"))?;
			// Inject the `$GIT_EDITOR` forwarding vars so
			// `git commit --amend` and friends route back to the
			// host IDE — see ADR 0021 and
			// `specs/containers.md` § "Editor forwarding".
			// The helper returns an empty list when there are
			// no bound folders to build a path map from, which
			// safely no-ops the feature for an empty workspace.
			let env = editor_forward_env_for_workspace(bound_folders);
			Ok(TerminalTarget::Container {
				container_name: container_name_for_workspace(id),
				cwd: Utf8PathBuf::from(cwd),
				shell: None,
				env,
			})
		}
	}
}

/// Supervisor: pumps PTY output to Tauri events and inbound
/// commands (write/resize/rerun) into the PTY. Exits when the child
/// closes its master (EOF on `next_output`) or the registry
/// channel is dropped (frontend close call).
///
/// `container_name` is `Some` for container terminals: the close
/// classification needs it for the post-exit liveness probe, and the
/// `docker exec` refusal detector only arms on container targets.
async fn supervise(
	app: AppHandle,
	registry: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, TerminalStreamHandle>>>,
	terminals: std::sync::Arc<TerminalRegistry>,
	stream_id: String,
	mut session: moon_terminal::PtySession,
	mut cmd_rx: mpsc::Receiver<TerminalCommand>,
	container_name: Option<String>,
) {
	// Ring of recent output for the `docker exec` refusal detector.
	// Bounded small — the refusal message lands within the first
	// chunk or two; we only ever inspect the tail on close.
	let mut output_sample: Vec<u8> = Vec::new();
	loop {
		tokio::select! {
			chunk = session.next_output() => {
				let Some(bytes) = chunk else {
					break;
				};
				if container_name.is_some() {
					output_sample.extend_from_slice(&bytes);
					if output_sample.len() > OUTPUT_SAMPLE_BYTES {
						output_sample.drain(..output_sample.len() - OUTPUT_SAMPLE_BYTES);
					}
				}
				terminals.record_output(&stream_id, &bytes).await;
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
					TerminalCommand::RerunCommand(command) => {
						// The readline line can hold a half-typed
						// command the user hasn't run yet — clear it
						// (Ctrl+C: SIGINT, fresh prompt) before
						// typing the replacement, or the replay would
						// append to whatever was sitting there. Then
						// wait out the SIGINT → prompt round trip
						// before the real bytes.
						if let Err(e) = session.write(b"\x03").await {
							tracing::warn!(stream_id = %stream_id, error = %e, "terminal ctrl-c write failed");
							continue;
						}
						tokio::time::sleep(std::time::Duration::from_millis(150)).await;
						let mut bytes = command.into_bytes();
						bytes.push(b'\n');
						if let Err(e) = session.write(&bytes).await {
							tracing::warn!(stream_id = %stream_id, error = %e, "terminal command replay write failed");
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
	let reason = classify_close(&container_name, code, &output_sample).await;
	drop(session);

	registry.lock().await.remove(&stream_id);
	// Keep the registry entry: the tab is still there showing the
	// output, so a coder read should still answer for it. The entry
	// goes away with the tab, in `terminal_close`.
	terminals.mark_exited(&stream_id, code).await;

	let _ = app.emit(
		TERMINAL_CLOSED_EVENT,
		&TerminalClosed {
			stream_id: stream_id.clone(),
			code,
			reason,
		},
	);
}

/// Bound on the output tail kept for the refusal detector — 8 KiB
/// is far past the one-line `docker exec` refusal and cheap to
/// keep per terminal.
const OUTPUT_SAMPLE_BYTES: usize = 8 * 1024;

/// Work out *why* the terminal's child exited. The frontend's whole
/// auto-close / auto-respawn policy hangs off this, so the order of
/// the checks matters: a `docker exec` refusal (container still
/// booting) is decided from the output alone and skips the daemon
/// probe; every other container exit is decided by whether the
/// container is still running afterwards.
async fn classify_close(
	container_name: &Option<String>,
	code: Option<i32>,
	output_sample: &[u8],
) -> TerminalCloseReason {
	let Some(container_name) = container_name else {
		return TerminalCloseReason::ShellExited;
	};
	if looks_like_exec_refusal(output_sample) {
		return TerminalCloseReason::ContainerNotRunning;
	}
	if code.is_none() {
		return TerminalCloseReason::Unknown;
	}
	if container_running(container_name).await {
		TerminalCloseReason::ContainerShellExited
	} else {
		TerminalCloseReason::ContainerStopped
	}
}

/// `docker exec` refuses to run the remote process — container
/// stopped/paused/being recreated or unknown — with a one-line
/// message on its own output and exit code 125. Match loosely on
/// the stable prefix rather than the full sentence so a docker CLI
/// rewording doesn't silently flip the classification.
fn looks_like_exec_refusal(output_sample: &[u8]) -> bool {
	String::from_utf8_lossy(output_sample).contains("Error response from daemon")
}
