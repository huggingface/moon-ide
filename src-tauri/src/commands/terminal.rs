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
	TerminalAgentOpened, TerminalClosed, TerminalOpenRequest, TerminalOutput, TerminalRemoved, TerminalRespawned,
	TerminalTarget as ProtocolTarget,
};
use moon_protocol::MoonError;
use moon_terminal::{
	container_name_for_workspace, editor_forward_env_for_workspace, spawn, AgentTerminalRequest, StartupCommand,
	TerminalKind, TerminalRegistration, TerminalRegistry, TerminalSpawner, TerminalTarget,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::state::{AppState, TerminalCommand, TerminalStreamHandle};

/// Per-chunk event name. Payload is [`TerminalOutput`].
pub const TERMINAL_OUTPUT_EVENT: &str = "terminal:output";

/// Emitted once when the underlying child exits. Payload is
/// [`TerminalClosed`].
pub const TERMINAL_CLOSED_EVENT: &str = "terminal:closed";

/// An agent opened a terminal. Payload is [`TerminalAgentOpened`].
pub const TERMINAL_AGENT_OPENED_EVENT: &str = "terminal:agent_opened";

/// A terminal's shell was replaced in place. Payload is
/// [`TerminalRespawned`].
pub const TERMINAL_RESPAWNED_EVENT: &str = "terminal:respawned";

/// The backend closed a terminal. Payload is [`TerminalRemoved`].
pub const TERMINAL_REMOVED_EVENT: &str = "terminal:removed";

/// Initial PTY size for agent-opened terminals, before the tab's
/// xterm fits and resizes it. Wider than the 80x24 default so a
/// dev server's banner doesn't wrap in an early `read_terminal`.
const AGENT_TERMINAL_COLS: u16 = 120;
const AGENT_TERMINAL_ROWS: u16 = 30;

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
	let stream_id = Uuid::new_v4().to_string();
	// A `command` in the request is prefilled at the fresh shell's
	// prompt, not executed (restart / session replay) — see
	// `moon_terminal::spawn`.
	let startup = request.command.as_deref().map(StartupCommand::Prefill);
	start_stream(&app, &state, &stream_id, &request, startup, true).await?;
	Ok(stream_id)
}

/// Spawn a PTY for `request` and supervise it under `stream_id`.
/// `fresh` registers a new terminal; otherwise the registry entry
/// already exists (an in-place restart) and only has its output
/// reset.
async fn start_stream(
	app: &AppHandle,
	state: &AppState,
	stream_id: &str,
	request: &TerminalOpenRequest,
	startup: Option<StartupCommand<'_>>,
	fresh: bool,
) -> Result<(), MoonError> {
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
	let target = into_internal_target(request.target.clone(), state, &bound_folders)?;

	let (cmd_tx, cmd_rx) = mpsc::channel::<TerminalCommand>(COMMAND_CHANNEL_DEPTH);

	// Spawn the PTY synchronously so an immediate failure (bad
	// shell path, missing container) surfaces as the open
	// command's error rather than a silent close event later.
	let session = spawn(&target, request.cols, request.rows, startup).map_err(|e| MoonError::internal(e.to_string()))?;

	// Register before the supervisor starts so no output chunk can
	// race ahead of the entry it belongs in.
	if fresh {
		state.terminals.register(stream_id, registration_for(request)).await;
	} else {
		state.terminals.reset(stream_id).await;
	}

	let registry = state.terminal_streams.clone();
	let task = tauri::async_runtime::spawn(supervise(
		app.clone(),
		registry.clone(),
		state.terminals.clone(),
		stream_id.to_owned(),
		session,
		cmd_rx,
	));

	registry
		.lock()
		.await
		.insert(stream_id.to_owned(), TerminalStreamHandle { tx: cmd_tx, task });
	Ok(())
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

#[tauri::command]
pub async fn terminal_close(state: State<'_, AppState>, stream_id: String) -> Result<(), MoonError> {
	close_stream(&state, &stream_id).await;
	Ok(())
}

async fn close_stream(state: &AppState, stream_id: &str) {
	let handle = state.terminal_streams.lock().await.remove(stream_id);
	if let Some(handle) = handle {
		handle.task.abort();
	}
	// The tab is gone, so its scrollback is no longer something the
	// user can see either — drop it rather than leaving output the
	// coder could still read. Aborting the supervisor skips its own
	// cleanup tail, so this is the only place a user-closed terminal
	// gets forgotten.
	state.terminals.forget(stream_id).await;
}

/// The coder's way to open, restart and close terminals (ADR 0090).
/// Installed on the shared [`TerminalRegistry`] once `AppState` is
/// managed; resolves it from the app handle per call.
pub struct AgentTerminalSpawner {
	pub app: AppHandle,
}

#[async_trait::async_trait]
impl TerminalSpawner for AgentTerminalSpawner {
	async fn open(&self, request: AgentTerminalRequest) -> Result<String, String> {
		let state = self.app.state::<AppState>();
		let stream_id = Uuid::new_v4().to_string();
		let folder = request.folder.to_string();
		let open = TerminalOpenRequest {
			target: request.target.clone(),
			cols: AGENT_TERMINAL_COLS,
			rows: AGENT_TERMINAL_ROWS,
			command: Some(request.agent.command.clone()),
			folder: Some(folder.clone()),
			agent: Some(request.agent.clone()),
		};
		let startup = Some(StartupCommand::Run(&request.agent.command));
		start_stream(&self.app, &state, &stream_id, &open, startup, true)
			.await
			.map_err(|err| err.to_string())?;
		// Output that beats this event to the frontend is queued
		// there by stream id until the tab mounts.
		let _ = self.app.emit(
			TERMINAL_AGENT_OPENED_EVENT,
			&TerminalAgentOpened {
				stream_id: stream_id.clone(),
				target: request.target,
				folder: Some(folder),
				agent: request.agent,
			},
		);
		Ok(stream_id)
	}

	async fn restart(&self, id: &str) -> Result<(), String> {
		let state = self.app.state::<AppState>();
		let info = state
			.terminals
			.info(id)
			.await
			.ok_or_else(|| format!("no terminal `{id}`"))?;
		let agent = info
			.agent
			.clone()
			.ok_or_else(|| format!("terminal `{id}` wasn't opened by an agent"))?;
		let target = match info.kind {
			TerminalKind::Host => ProtocolTarget::Host {
				cwd: (info.cwd != "~").then(|| info.cwd.clone()),
			},
			TerminalKind::Container => ProtocolTarget::Container { cwd: info.cwd.clone() },
		};
		let old = state.terminal_streams.lock().await.remove(id);
		if let Some(old) = old {
			old.task.abort();
			let _ = old.task.await;
		}
		// Keep the old run's scrollback on screen, visibly fenced off.
		let _ = self.app.emit(
			TERMINAL_OUTPUT_EVENT,
			&TerminalOutput {
				stream_id: id.to_owned(),
				data: BASE64.encode(b"\r\n\x1b[2m--- restarted by agent ---\x1b[0m\r\n"),
			},
		);
		let open = TerminalOpenRequest {
			target,
			cols: info.cols,
			rows: info.rows,
			command: Some(agent.command.clone()),
			folder: info.folder.as_ref().map(ToString::to_string),
			agent: Some(agent.clone()),
		};
		let startup = Some(StartupCommand::Run(&agent.command));
		start_stream(&self.app, &state, id, &open, startup, false)
			.await
			.map_err(|err| err.to_string())?;
		let _ = self.app.emit(
			TERMINAL_RESPAWNED_EVENT,
			&TerminalRespawned {
				stream_id: id.to_owned(),
			},
		);
		Ok(())
	}

	async fn close(&self, id: &str) -> Result<(), String> {
		let state = self.app.state::<AppState>();
		close_stream(&state, id).await;
		let _ = self.app.emit(
			TERMINAL_REMOVED_EVENT,
			&TerminalRemoved {
				stream_id: id.to_owned(),
			},
		);
		Ok(())
	}
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
		agent: request.agent.clone(),
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
async fn supervise(
	app: AppHandle,
	registry: std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<String, TerminalStreamHandle>>>,
	terminals: std::sync::Arc<TerminalRegistry>,
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
	// The entry stays readable until the frontend reacts to the close
	// event by closing the tab (`terminal_close` forgets it).
	terminals.mark_exited(&stream_id, code).await;

	let _ = app.emit(
		TERMINAL_CLOSED_EVENT,
		&TerminalClosed {
			stream_id: stream_id.clone(),
			code,
		},
	);
}
