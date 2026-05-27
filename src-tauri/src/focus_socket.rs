//! Per-workspace single-instance lock + cross-process IPC.
//!
//! Process-per-workspace requires:
//!
//! 1. A way to detect "is a process already running for slug X?"
//! 2. A way to bring that process's window to front instead of
//!    spawning a duplicate.
//!
//! A Unix domain socket at
//! `<workspaces_dir>/<slug>/instance.sock` covers both. The
//! owner process binds it on startup and listens for newline-
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
//! `connect()`-then-write fails; we unlink and rebind. The socket
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

/// Path of the per-workspace single-instance socket.
pub fn socket_path(workspaces_dir: &Utf8Path, slug: &str) -> PathBuf {
	workspaces_dir.join(slug).join("instance.sock").into_std_path_buf()
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
			// Someone has it — either a live sibling or a
			// stale file from a crash. Probe.
			if probe_alive(&path).await {
				return Err(err);
			}
			tokio::fs::remove_file(&path).await.ok();
			UnixListener::bind(&path)
		}
		Err(err) => Err(err),
	}
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
pub fn spawn_focus_listener(listener: UnixListener, app: AppHandle, registry: Arc<EditorRegistry>) {
	tauri::async_runtime::spawn(async move {
		loop {
			match listener.accept().await {
				Ok((stream, _addr)) => {
					let app = app.clone();
					let registry = Arc::clone(&registry);
					tauri::async_runtime::spawn(async move {
						handle_connection(stream, app, registry).await;
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
}

async fn handle_connection(mut stream: UnixStream, app: AppHandle, registry: Arc<EditorRegistry>) {
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
/// process's exit path so a clean shutdown leaves no stale
/// file. Crashes still leave the file, but `try_bind` recovers.
pub async fn cleanup(workspaces_dir: &Utf8Path, slug: &str) {
	let path = socket_path(workspaces_dir, slug);
	if let Err(err) = tokio::fs::remove_file(&path).await {
		if err.kind() != std::io::ErrorKind::NotFound {
			tracing::debug!(error = %err, path = %path.display(), "socket cleanup failed");
		}
	}
}

#[cfg(test)]
mod tests {
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
}
