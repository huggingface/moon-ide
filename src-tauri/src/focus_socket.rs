//! Per-workspace single-instance lock + cross-process IPC.
//!
//! Process-per-workspace requires:
//!
//! 1. A way to detect "is a process already running for slug X?"
//! 2. A way to bring that process's window to front instead of
//!    spawning a duplicate.
//!
//! A Unix domain socket at
//! `<workspaces_dir>/<slug>/run/instance.sock` covers both. It
//! lives in its own `run/` subdirectory rather than directly
//! under the slug dir so the dev container can bind-mount that
//! *directory* (never the socket file) and still see the socket
//! come and go across IDE restarts — see [ADR 0026](../../specs/decisions/0026-socket-dir-mount.md).
//! The owner process binds it on startup and listens for newline-
//! framed messages defined in [`moon_protocol::focus_socket`]:
//!
//! - `F\n` — "Focus your window." Fire-and-forget; the sender
//!   disconnects immediately.
//! - `E\n<host-path>\n` — "Open this host-absolute path as a
//!   buffer and block until the user is done." Used by the
//!   in-container `moon-edit` shim for forwarded `$GIT_EDITOR`
//!   invocations (`git commit --amend` and friends — see
//!   [ADR 0021](../../specs/decisions/0021-git-editor-forward.md)
//!   and [`specs/containers.md`](../../specs/containers.md)
//!   § "Editor forwarding").
//!
//! The `E` path parks the connection on a server-side oneshot
//! channel keyed by [`EditId`], emits a Tauri `editor:request`
//! event the frontend handles, and waits for the frontend to
//! call back via [`EditorRegistry::resolve`]. Resolution writes
//! `OK\n` or `CANCEL\n` back on the same socket and closes it.
//!
//! Stale sockets (orphaned by a crash) are detected because
//! `connect()`-then-write fails; we unlink and rebind. The same
//! recovery clears a root-owned directory Docker may have left at
//! the path from a botched bind mount (ADR 0026). The socket
//! lives under the user's own data dir, so no extra ACL is
//! needed beyond the user's umask.
//!
//! Windows support is deferred — the team is on Linux. When we
//! add Windows we'll swap the listener for a named pipe; the
//! protocol stays the same.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use moon_protocol::focus_socket::{
	encode_reply, encode_request, is_truncated, parse_request, Reply, Request, MAX_REQUEST_BYTES,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

/// How long we wait for the existing process to ack a focus
/// message before deciding "the socket is stale". 250 ms is
/// generous on a healthy local socket and short enough that a
/// genuine stale-file path doesn't make the user's launch feel
/// slow.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long we wait for the request body bytes after `accept()`.
/// Generous because the `moon-edit` shim might be the very first
/// thing scheduled after a fork.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long a relaunch waits for the previous
/// owner's `stop_all` teardown to finish before giving up and
/// proceeding anyway. `stop_all` issues `docker compose stop`
/// against the workspace shell plus every bound-folder project,
/// each with a ~10s SIGTERM grace period; serialised, a busy
/// workspace can legitimately take a while. We cap the wait so a
/// crashed previous owner (sentinel left behind, teardown never
/// finished) can't hang the new launch forever — on timeout we
/// proceed, and the auto-resume path's "already running, skip"
/// logic handles whatever the daemon actually has up.
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(45);

/// Poll interval while waiting for the shutdown sentinel to clear.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Tauri event emitted when a forwarded `$GIT_EDITOR` request
/// arrives. The frontend opens the file as an external buffer,
/// tags it with `pendingEdit = id`, and calls
/// `editor_forward_finish` / `editor_forward_cancel` (defined in
/// `commands/editor_forward.rs`) when the user is done.
pub const EDITOR_REQUEST_EVENT: &str = "editor:request";

/// Identifier for one in-flight forwarded edit.
///
/// Generated server-side on accept; sent to the frontend in the
/// `editor:request` payload; sent back via the Tauri commands
/// that finish/cancel the edit. Random UUID rather than a counter
/// so a stale frontend reply from a previous edit can't
/// accidentally resolve a fresh request.
pub type EditId = String;

/// Payload of the `editor:request` Tauri event. Frontend
/// signature is shared via `src/lib/protocol.ts`.
#[derive(Debug, Clone, Serialize)]
pub struct EditRequestPayload {
	pub id: EditId,
	pub host_path: String,
}

/// Outcome of a forwarded edit, communicated by the frontend
/// back through the Tauri command surface.
#[derive(Debug, Clone, Copy)]
pub enum EditOutcome {
	Finished,
	Cancelled,
}

/// In-memory registry of parked edit requests, keyed by [`EditId`].
///
/// One instance per IDE process, managed via Tauri's state
/// container. The connection handler inserts a oneshot sender
/// when a new `E` request arrives; the Tauri command
/// `editor_forward_finish` (or `_cancel`) looks the id up and
/// fires the matching outcome through the channel.
///
/// Entries are removed eagerly by `resolve` so the map can't
/// grow unbounded. A dropped sender (resolve never called —
/// process crash, frontend bug) leaks one map entry until the
/// process exits; not a correctness concern.
#[derive(Default)]
pub struct EditorRegistry {
	inner: Mutex<HashMap<EditId, oneshot::Sender<EditOutcome>>>,
}

impl EditorRegistry {
	pub fn new() -> Self {
		Self::default()
	}

	/// Register a new pending edit. Returns the receiver the
	/// connection handler should await.
	async fn register(&self, id: EditId) -> oneshot::Receiver<EditOutcome> {
		let (tx, rx) = oneshot::channel();
		self.inner.lock().await.insert(id, tx);
		rx
	}

	/// Resolve a pending edit. Returns `true` if a matching id
	/// was found and the outcome was delivered; `false` if the
	/// id was unknown (already resolved, or the connection
	/// dropped before the frontend got a chance to reply). The
	/// `false` case is benign — the connection handler will see
	/// the dropped sender on its side and just close the socket.
	pub async fn resolve(&self, id: &str, outcome: EditOutcome) -> bool {
		let Some(tx) = self.inner.lock().await.remove(id) else {
			return false;
		};
		// `oneshot::Sender::send` returns Err only when the
		// receiver was already dropped — meaning the connection
		// handler has already finished (timeout, socket EOF).
		// Nothing to do in either case.
		let _ = tx.send(outcome);
		true
	}

	/// Test/diagnostic accessor — the count of currently-parked edits.
	#[cfg(test)]
	pub async fn pending_count(&self) -> usize {
		self.inner.lock().await.len()
	}
}

/// Directory holding the per-workspace single-instance socket.
///
/// The socket lives one level below the slug's state dir, in a
/// dedicated `run/` directory that holds nothing else. The dev
/// container bind-mounts *this directory* (not the socket file)
/// so the socket can be unlinked and rebound across IDE restarts
/// without the container's mount going stale, and so a missing
/// source can never make Docker auto-create the socket path as a
/// root-owned file/dir. See [ADR 0026](../../specs/decisions/0026-socket-dir-mount.md).
pub fn socket_dir(workspaces_dir: &Utf8Path, slug: &str) -> PathBuf {
	workspaces_dir.join(slug).join("run").into_std_path_buf()
}

/// Path of the per-workspace single-instance socket, inside
/// [`socket_dir`].
pub fn socket_path(workspaces_dir: &Utf8Path, slug: &str) -> PathBuf {
	socket_dir(workspaces_dir, slug).join("instance.sock")
}

/// Try to bind the socket for `slug`. On success the caller
/// owns this workspace for the rest of its process life; on
/// failure it should treat that as "another process already has
/// it" and either focus that one or fail.
///
/// Stale sockets — left behind by a previous crash — are
/// auto-recovered: if `bind` would fail because the path
/// exists, we attempt a probe-connect; if the probe fails,
/// the socket is dead and we unlink + retry. If the probe
/// succeeds, a real owner exists and we surface the bind
/// failure.
pub async fn try_bind(workspaces_dir: &Utf8Path, slug: &str) -> std::io::Result<UnixListener> {
	let path = socket_path(workspaces_dir, slug);
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}
	match UnixListener::bind(&path) {
		Ok(listener) => Ok(listener),
		Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
			// Something is at the path. Either a live sibling
			// owns it, or it's debris: a stale socket file from
			// a crash, or a root-owned directory Docker created
			// when it found a missing bind-mount source (see
			// ADR 0026). Probe — a real listener means a live
			// owner; anything else we clear and rebind over.
			if probe_alive(&path).await {
				return Err(err);
			}
			clear_stale_entry(&path).await?;
			UnixListener::bind(&path)
		}
		Err(err) => Err(err),
	}
}

/// Remove whatever non-socket debris is sitting at the lock path
/// so a fresh `bind` can succeed. Two shapes show up in practice:
///
/// - A stale socket *file* left behind by a crashed owner.
/// - A *directory* Docker created when it found the bind-mount
///   source missing at `docker compose up` time — owned by root
///   because the daemon runs as root (the bug ADR 0026 fixes by
///   mounting the socket's parent dir instead of the file).
///
/// Removal is governed by the *parent* directory's permissions,
/// which moon-ide owns, so even a root-owned empty directory
/// unlinks cleanly. A non-empty root-owned directory (contents we
/// can't unlink) surfaces the error rather than silently looping.
async fn clear_stale_entry(path: &Path) -> std::io::Result<()> {
	let meta = match tokio::fs::symlink_metadata(path).await {
		Ok(meta) => meta,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(err) => return Err(err),
	};
	if meta.is_dir() {
		return tokio::fs::remove_dir_all(path).await;
	}
	tokio::fs::remove_file(path).await
}

/// Returns true if a process is actively listening on the
/// workspace's instance socket. Used by the cross-process
/// "focus or spawn" path and by `workspace_delete` to refuse
/// dropping a workspace that's currently open elsewhere.
pub async fn workspace_is_live(workspaces_dir: &Utf8Path, slug: &str) -> bool {
	let path = socket_path(workspaces_dir, slug);
	if !path.exists() {
		return false;
	}
	probe_alive(&path).await
}

/// Connect-and-handshake check. We can't just `exists()` the
/// socket because a crashed process leaves the file behind —
/// only a real listener will accept the connection.
async fn probe_alive(path: &Path) -> bool {
	let connect = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path)).await;
	matches!(connect, Ok(Ok(_)))
}

/// Send a `Focus` message to the existing owner of `slug`.
/// Returns `Ok(())` on success, `Err` if the socket is missing,
/// stale, or the write fails.
pub async fn send_focus(workspaces_dir: &Utf8Path, slug: &str) -> std::io::Result<()> {
	let path = socket_path(workspaces_dir, slug);
	let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
		.await
		.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))??;
	let bytes = encode_request(&Request::Focus)?;
	stream.write_all(&bytes).await?;
	stream.flush().await?;
	Ok(())
}

/// Spawn the focus listener task. The owning process keeps
/// this running for its whole life; on every accepted
/// connection it reads one message and reacts.
///
/// Handles both message kinds defined in
/// [`moon_protocol::focus_socket`]:
///
/// - `Request::Focus` — brings the `main` window to front and
///   closes the connection.
/// - `Request::Edit` — registers a pending edit on `registry`,
///   emits the `editor:request` Tauri event, and parks the
///   connection until the matching `resolve` call comes back
///   from the frontend.
pub fn spawn_focus_listener(
	listener: UnixListener,
	app: AppHandle,
	registry: Arc<EditorRegistry>,
	rpc: Arc<dyn BridgeRpcHandler>,
) -> tokio::task::AbortHandle {
	// `tauri::async_runtime::spawn` rather than `tokio::spawn` —
	// `setup` runs synchronously on a thread that isn't bound to
	// Tokio's runtime, so a bare `tokio::spawn` panics with
	// "there is no reactor running". Same trap the slack poller
	// and system theme watcher have notes about. The Tauri wrapper
	// still drops onto the same Tokio runtime; we just have to
	// reach through `inner()` for the `AbortHandle` we cache in
	// `AppState::focus_listener`.
	let task = tauri::async_runtime::spawn(async move {
		loop {
			match listener.accept().await {
				Ok((stream, _addr)) => {
					let app = app.clone();
					let registry = Arc::clone(&registry);
					let rpc = Arc::clone(&rpc);
					tauri::async_runtime::spawn(async move {
						handle_connection(stream, app, registry, rpc).await;
					});
				}
				Err(err) => {
					tracing::warn!(error = %err, "focus listener accept failed");
					// Don't tight-loop on a failing
					// listener; back off briefly.
					tokio::time::sleep(Duration::from_millis(100)).await;
				}
			}
		}
	});
	task.inner().abort_handle()
}

async fn handle_connection(
	mut stream: UnixStream,
	app: AppHandle,
	registry: Arc<EditorRegistry>,
	rpc: Arc<dyn BridgeRpcHandler>,
) {
	// Read the request, looping until we have a complete framed
	// message or we exceed the byte cap / time budget. The
	// listener's only clients are sibling launcher processes
	// and the moon-edit shim, so the practical worst-case is a
	// `PATH_MAX`-sized path arriving in one syscall.
	let mut buf = Vec::with_capacity(256);
	let request = loop {
		if buf.len() > MAX_REQUEST_BYTES {
			tracing::warn!(bytes = buf.len(), "focus listener request exceeded cap; dropping");
			return;
		}
		let read_result = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(&mut buf)).await;
		match read_result {
			Ok(Ok(0)) => {
				if buf.is_empty() {
					tracing::debug!("focus listener: empty connection");
				} else {
					tracing::warn!(bytes = buf.len(), "focus listener: connection closed mid-request");
				}
				return;
			}
			Ok(Ok(_)) => {
				// Try to parse what we have so far.
				match parse_request(&buf) {
					Ok((req, _consumed)) => break req,
					Err(err) if is_truncated(&err) => continue,
					Err(err) => {
						tracing::warn!(error = %err, "focus listener: invalid request");
						return;
					}
				}
			}
			Ok(Err(err)) => {
				tracing::warn!(error = %err, "focus listener: read failed");
				return;
			}
			Err(_) => {
				tracing::warn!("focus listener: read timed out");
				return;
			}
		}
	};

	match request {
		Request::Focus => {
			focus_main_window(&app);
			// Fire-and-forget; the sender already disconnected
			// or is about to. Nothing to send back.
		}
		Request::Edit { host_path } => {
			handle_edit_request(stream, app, registry, host_path).await;
		}
		Request::Rpc { json } => {
			handle_rpc_request(stream, rpc, json).await;
		}
		Request::Subscribe { json } => {
			handle_subscribe_request(stream, rpc, json).await;
		}
	}
}

/// Handler the focus listener calls for an `R` (RPC) request. The
/// `src-tauri` side implements this against the coder + workspace
/// registry; the focus-socket module stays decoupled from those
/// types by going through this trait. Phase 13 (mobile companion).
#[async_trait::async_trait]
pub trait BridgeRpcHandler: Send + Sync {
	/// Dispatch one method call. Returns the result JSON or an error
	/// message; the listener wraps either into an
	/// [`moon_protocol::focus_socket::RpcResponse`].
	async fn dispatch(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String>;

	/// Subscribe to an event stream for a `Subscribe` request. Returns
	/// a receiver of JSON event payloads the listener forwards
	/// one-per-line until the client disconnects, or an error string
	/// if the method isn't a known stream. Implemented against the
	/// coder's broadcast channel in `src-tauri`; the protocol crate
	/// stays decoupled by trafficking in `serde_json::Value`.
	async fn subscribe(
		&self,
		method: &str,
		params: serde_json::Value,
	) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>, String>;
}

async fn handle_rpc_request(mut stream: UnixStream, rpc: Arc<dyn BridgeRpcHandler>, json: String) {
	let request: moon_protocol::focus_socket::RpcRequest = match serde_json::from_str(&json) {
		Ok(req) => req,
		Err(err) => {
			let resp = moon_protocol::focus_socket::RpcResponse::error(format!("malformed rpc request: {err}"));
			write_rpc_response(&mut stream, &resp).await;
			return;
		}
	};

	let resp = match rpc.dispatch(&request.method, request.params).await {
		Ok(value) => moon_protocol::focus_socket::RpcResponse::ok(value),
		Err(message) => moon_protocol::focus_socket::RpcResponse::error(message),
	};
	write_rpc_response(&mut stream, &resp).await;
}

async fn handle_subscribe_request(mut stream: UnixStream, rpc: Arc<dyn BridgeRpcHandler>, json: String) {
	let request: moon_protocol::focus_socket::RpcRequest = match serde_json::from_str(&json) {
		Ok(req) => req,
		Err(err) => {
			let resp = moon_protocol::focus_socket::RpcResponse::error(format!("malformed subscribe request: {err}"));
			write_rpc_response(&mut stream, &resp).await;
			return;
		}
	};

	let mut rx = match rpc.subscribe(&request.method, request.params).await {
		Ok(rx) => rx,
		Err(message) => {
			let resp = moon_protocol::focus_socket::RpcResponse::error(message);
			write_rpc_response(&mut stream, &resp).await;
			return;
		}
	};

	// Forward one event per line until the channel closes (the
	// workspace stopped emitting) or the write fails (the bridge
	// disconnected — the phone closed its WS). A failed write is the
	// normal teardown path, logged at debug only.
	while let Some(event) = rx.recv().await {
		let resp = moon_protocol::focus_socket::RpcResponse::ok(event);
		let bytes = moon_protocol::focus_socket::encode_rpc_response(&resp);
		if stream.write_all(&bytes).await.is_err() {
			tracing::debug!("subscribe stream: client disconnected");
			return;
		}
		if stream.flush().await.is_err() {
			return;
		}
	}
}

async fn write_rpc_response(stream: &mut UnixStream, resp: &moon_protocol::focus_socket::RpcResponse) {
	let bytes = moon_protocol::focus_socket::encode_rpc_response(resp);
	if let Err(err) = stream.write_all(&bytes).await {
		tracing::warn!(error = %err, "focus listener: failed to write rpc response");
		return;
	}
	if let Err(err) = stream.flush().await {
		tracing::debug!(error = %err, "focus listener: rpc flush failed");
	}
}

async fn handle_edit_request(mut stream: UnixStream, app: AppHandle, registry: Arc<EditorRegistry>, host_path: String) {
	let id: EditId = Uuid::new_v4().to_string();
	let rx = registry.register(id.clone()).await;
	let payload = EditRequestPayload {
		id: id.clone(),
		host_path: host_path.clone(),
	};
	if let Err(err) = app.emit(EDITOR_REQUEST_EVENT, &payload) {
		// Couldn't reach the frontend at all — best we can do
		// is reply CANCEL so the shim exits clean. Clean up
		// the registry entry on the way out.
		tracing::warn!(error = %err, "failed to emit editor:request; cancelling forwarded edit");
		registry.resolve(&id, EditOutcome::Cancelled).await;
		send_reply(&mut stream, Reply::Cancel).await;
		return;
	}
	// Park until the frontend resolves the edit. A dropped rx
	// (handle_connection task cancelled, registry entry removed
	// out from under us) folds into "Cancel" — we'd rather a
	// shim hang gets resolved as a clean abort than risk
	// leaving git stuck.
	let outcome = match rx.await {
		Ok(outcome) => outcome,
		Err(_) => {
			tracing::warn!(id = %id, "edit registry entry resolved without an outcome; treating as cancel");
			EditOutcome::Cancelled
		}
	};
	let reply = match outcome {
		EditOutcome::Finished => Reply::Ok,
		EditOutcome::Cancelled => Reply::Cancel,
	};
	send_reply(&mut stream, reply).await;
}

async fn send_reply(stream: &mut UnixStream, reply: Reply) {
	let bytes = encode_reply(reply);
	if let Err(err) = stream.write_all(&bytes).await {
		tracing::warn!(error = %err, "focus listener: failed to write reply");
		return;
	}
	if let Err(err) = stream.flush().await {
		tracing::debug!(error = %err, "focus listener: flush failed");
	}
}

fn focus_main_window(app: &AppHandle) {
	let Some(window) = app.get_webview_window("main") else {
		return;
	};
	if let Err(err) = window.unminimize() {
		tracing::debug!(error = %err, "unminimize failed");
	}
	if let Err(err) = window.show() {
		tracing::warn!(error = %err, "show failed");
	}
	if let Err(err) = window.set_focus() {
		tracing::warn!(error = %err, "set_focus failed");
	}
}

/// Best-effort cleanup: unlink the socket file. Called from the
/// process's exit path — *before* the slower `stop_all` teardown
/// and after the listener task is aborted — so a clean shutdown
/// drops the single-instance lock immediately and a concurrent
/// relaunch doesn't see the workspace as still in use. Crashes
/// still leave the file, but `try_bind` recovers.
pub async fn cleanup(workspaces_dir: &Utf8Path, slug: &str) {
	let path = socket_path(workspaces_dir, slug);
	if let Err(err) = tokio::fs::remove_file(&path).await {
		if err.kind() != std::io::ErrorKind::NotFound {
			tracing::debug!(error = %err, path = %path.display(), "socket cleanup failed");
		}
	}
}

/// Path of the per-workspace "shutdown in progress" sentinel, a
/// sibling of [`socket_path`] in the same `run/` directory.
fn shutdown_sentinel_path(workspaces_dir: &Utf8Path, slug: &str) -> PathBuf {
	socket_dir(workspaces_dir, slug).join("shutting-down")
}

/// Mark this workspace as tearing down.
///
/// Written by the exiting process *before* it unlinks
/// `instance.sock` and runs the multi-second `stop_all`, and
/// removed by [`clear_shutdown_sentinel`] once `stop_all`
/// returns. It exists to close a relaunch-during-shutdown race:
/// the single-instance lock is released early (so a relaunch's
/// `probe_alive` doesn't wrongly report the workspace as still in
/// use), which means a quick relaunch can `try_bind` successfully
/// while the previous `stop_all` is still issuing `docker compose
/// stop`. Without the sentinel the new process would query
/// container state mid-teardown, see the containers still
/// `Running`, paint the pip green, then watch the old `stop_all`
/// kill them out from under it (`exited (137)`). The sentinel lets
/// [`await_previous_shutdown`] hold the new launch's auto-resume
/// until the teardown finishes.
///
/// The file body is the writer's PID so a relaunch can tell a
/// genuinely-in-progress teardown from a stale sentinel left by a
/// crashed owner.
pub async fn write_shutdown_sentinel(workspaces_dir: &Utf8Path, slug: &str) {
	let path = shutdown_sentinel_path(workspaces_dir, slug);
	if let Some(parent) = path.parent() {
		if let Err(err) = tokio::fs::create_dir_all(parent).await {
			tracing::debug!(error = %err, "write_shutdown_sentinel: create run dir failed");
			return;
		}
	}
	let body = std::process::id().to_string();
	if let Err(err) = tokio::fs::write(&path, body).await {
		tracing::debug!(error = %err, path = %path.display(), "write_shutdown_sentinel failed");
	}
}

/// Remove the shutdown sentinel written by
/// [`write_shutdown_sentinel`]. Called once `stop_all` has
/// finished so a relaunch waiting on it can proceed. Best-effort:
/// a crash before this point leaves the file, which
/// [`await_previous_shutdown`] treats as stale once the writing
/// PID is gone (and times out on regardless).
pub async fn clear_shutdown_sentinel(workspaces_dir: &Utf8Path, slug: &str) {
	let path = shutdown_sentinel_path(workspaces_dir, slug);
	if let Err(err) = tokio::fs::remove_file(&path).await {
		if err.kind() != std::io::ErrorKind::NotFound {
			tracing::debug!(error = %err, path = %path.display(), "clear_shutdown_sentinel failed");
		}
	}
}

/// Block until the previous owner's `stop_all` teardown finishes,
/// so a relaunch that fired during shutdown doesn't act on stale
/// (still-`Running`) container state.
///
/// Returns once either the [`write_shutdown_sentinel`] file is
/// gone (clean handoff: the previous process finished `stop_all`
/// and removed it), the sentinel is stale (its writer PID is no
/// longer alive — a crash mid-teardown), or
/// [`SHUTDOWN_WAIT_TIMEOUT`] elapses (defensive cap). After this
/// returns the containers are guaranteed stopped on the clean
/// path, so the caller's `auto_resume_*` sees `Stopped` and brings
/// them back up instead of trusting a mid-teardown `Running`.
///
/// No sentinel present is the overwhelmingly common case (cold
/// launch, or a relaunch well after the previous quit) and returns
/// immediately.
pub async fn await_previous_shutdown(workspaces_dir: &Utf8Path, slug: &str) {
	let path = shutdown_sentinel_path(workspaces_dir, slug);
	if !path.exists() {
		return;
	}

	tracing::info!(slug = %slug, "await_previous_shutdown: previous session still tearing down, waiting");
	let deadline = tokio::time::Instant::now() + SHUTDOWN_WAIT_TIMEOUT;
	loop {
		match tokio::fs::read_to_string(&path).await {
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
				tracing::info!(slug = %slug, "await_previous_shutdown: previous teardown finished");
				return;
			}
			Err(err) => {
				tracing::debug!(error = %err, "await_previous_shutdown: read failed; proceeding");
				return;
			}
			Ok(body) => {
				if !writer_is_alive(body.trim()) {
					tracing::warn!(slug = %slug, "await_previous_shutdown: sentinel is stale (writer gone); proceeding");
					return;
				}
			}
		}
		if tokio::time::Instant::now() >= deadline {
			tracing::warn!(slug = %slug, "await_previous_shutdown: timed out waiting for previous teardown; proceeding");
			return;
		}
		tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
	}
}

/// Best-effort liveness check for the PID recorded in a shutdown
/// sentinel. `kill(pid, 0)` returns success if the process exists
/// (whether or not we could actually signal it). An unparseable
/// body is treated as "not alive" so a malformed sentinel doesn't
/// strand the launch. Unix-only, matching the rest of this module.
fn writer_is_alive(body: &str) -> bool {
	let Ok(pid) = body.parse::<i32>() else {
		return false;
	};
	if pid <= 0 {
		return false;
	}
	// SAFETY: `kill` with signal 0 performs only an existence /
	// permission check and never delivers a signal, so there's no
	// memory or process-state hazard.
	let rc = unsafe { libc::kill(pid, 0) };
	if rc == 0 {
		return true;
	}
	// ESRCH = no such process (dead). EPERM = alive but we can't
	// signal it (still counts as alive). Any other errno: be
	// conservative and treat as alive so we wait rather than
	// resume over a live teardown.
	std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
	use camino::Utf8PathBuf;

	use super::*;

	#[tokio::test]
	async fn registry_resolves_finished() {
		let reg = EditorRegistry::new();
		let id = "abc".to_owned();
		let rx = reg.register(id.clone()).await;
		assert_eq!(reg.pending_count().await, 1);
		assert!(reg.resolve(&id, EditOutcome::Finished).await);
		assert!(matches!(rx.await.unwrap(), EditOutcome::Finished));
		assert_eq!(reg.pending_count().await, 0);
	}

	#[tokio::test]
	async fn registry_resolve_unknown_id_returns_false() {
		let reg = EditorRegistry::new();
		assert!(!reg.resolve("nope", EditOutcome::Finished).await);
	}

	#[tokio::test]
	async fn registry_drop_receiver_does_not_panic() {
		let reg = EditorRegistry::new();
		let id = "abc".to_owned();
		drop(reg.register(id.clone()).await);
		// Resolve goes through — the sender's send returns Err
		// but `resolve` swallows it.
		assert!(reg.resolve(&id, EditOutcome::Cancelled).await);
	}

	/// Unique temp workspaces dir for a single test; removed on drop.
	struct TmpWorkspaces(Utf8PathBuf);
	impl Drop for TmpWorkspaces {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}
	fn tmp_workspaces() -> TmpWorkspaces {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		let p = std::env::temp_dir().join(format!("moon-focus-test-{nanos}-{:?}", std::thread::current().id()));
		std::fs::create_dir_all(&p).unwrap();
		TmpWorkspaces(Utf8PathBuf::from_path_buf(p).unwrap())
	}

	#[tokio::test]
	async fn try_bind_creates_socket_in_run_dir() {
		let tmp = tmp_workspaces();
		let listener = try_bind(&tmp.0, "ws").await.expect("bind");
		drop(listener);
		assert!(socket_path(&tmp.0, "ws").exists());
		assert!(socket_dir(&tmp.0, "ws").is_dir());
	}

	#[tokio::test]
	async fn try_bind_recovers_from_stale_socket_file() {
		let tmp = tmp_workspaces();
		let dir = socket_dir(&tmp.0, "ws");
		std::fs::create_dir_all(&dir).unwrap();
		// A crashed owner left a socket file with nobody listening.
		std::fs::write(socket_path(&tmp.0, "ws"), b"").unwrap();
		// We must clear it and bind fresh rather than fail.
		let _listener = try_bind(&tmp.0, "ws").await.expect("recover stale file");
	}

	#[tokio::test]
	async fn try_bind_recovers_from_directory_at_socket_path() {
		let tmp = tmp_workspaces();
		// Stand in for the root-owned directory Docker leaves at
		// the socket path from a botched bind mount (ADR 0026).
		// We can only create a user-owned one in a test, but the
		// recovery path (probe-dead -> remove_dir_all -> rebind)
		// is identical.
		std::fs::create_dir_all(socket_path(&tmp.0, "ws")).unwrap();
		let _listener = try_bind(&tmp.0, "ws").await.expect("recover directory");
	}

	#[tokio::test]
	async fn try_bind_refuses_when_a_live_owner_holds_the_socket() {
		let tmp = tmp_workspaces();
		let owner = try_bind(&tmp.0, "ws").await.expect("first bind");
		// A second bind while the owner is alive must fail — that's
		// the single-instance guarantee.
		let err = try_bind(&tmp.0, "ws").await.expect_err("second bind");
		assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
		drop(owner);
	}

	#[tokio::test]
	async fn await_previous_shutdown_returns_immediately_without_sentinel() {
		let tmp = tmp_workspaces();
		// No sentinel on disk: the common cold-launch path must not
		// block. Wrap in a tight timeout so a regression that
		// spins on the poll loop fails loudly.
		tokio::time::timeout(Duration::from_secs(2), await_previous_shutdown(&tmp.0, "ws"))
			.await
			.expect("await_previous_shutdown should return immediately with no sentinel");
	}

	#[tokio::test]
	async fn await_previous_shutdown_skips_stale_sentinel_from_dead_writer() {
		let tmp = tmp_workspaces();
		std::fs::create_dir_all(socket_dir(&tmp.0, "ws")).unwrap();
		// PID 2^31-1 is effectively never live: a relaunch must treat
		// the sentinel as stale and proceed rather than wait out the
		// full timeout.
		std::fs::write(shutdown_sentinel_path(&tmp.0, "ws"), i32::MAX.to_string()).unwrap();
		tokio::time::timeout(Duration::from_secs(2), await_previous_shutdown(&tmp.0, "ws"))
			.await
			.expect("stale sentinel should not block past its dead writer");
	}

	#[tokio::test]
	async fn await_previous_shutdown_unblocks_when_sentinel_cleared() {
		let tmp = tmp_workspaces();
		std::fs::create_dir_all(socket_dir(&tmp.0, "ws")).unwrap();
		// Live writer (our own PID) so the wait can't short-circuit on
		// staleness — it must block until the file is removed.
		std::fs::write(shutdown_sentinel_path(&tmp.0, "ws"), std::process::id().to_string()).unwrap();
		// Remove the sentinel from a plain OS thread after a short
		// delay so the awaiting future has to actually block on the
		// poll loop and then observe the file disappear. (A tokio
		// task would trip the repo's `tokio::spawn` lint; a thread
		// doing a blocking `fs::remove_file` exercises the same
		// path.)
		let sentinel = shutdown_sentinel_path(&tmp.0, "ws");
		let clearer = std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(300));
			std::fs::remove_file(&sentinel).unwrap();
		});
		tokio::time::timeout(Duration::from_secs(3), await_previous_shutdown(&tmp.0, "ws"))
			.await
			.expect("await should unblock once the sentinel is cleared");
		clearer.join().unwrap();
	}

	#[test]
	fn writer_is_alive_self_is_alive() {
		assert!(writer_is_alive(&std::process::id().to_string()));
	}

	#[test]
	fn writer_is_alive_garbage_is_not_alive() {
		assert!(!writer_is_alive("not-a-pid"));
		assert!(!writer_is_alive("0"));
		assert!(!writer_is_alive("-5"));
	}
}
