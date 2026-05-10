//! Per-workspace single-instance lock + focus IPC.
//!
//! Process-per-workspace requires:
//!
//! 1. A way to detect "is a process already running for slug X?"
//! 2. A way to bring that process's window to front instead of
//!    spawning a duplicate.
//!
//! A Unix domain socket at
//! `<workspaces_dir>/<slug>/instance.sock` covers both. The
//! owner process binds it on startup and listens for a tiny
//! one-byte message kind:
//!
//! - `b"F\n"` — "Focus your window."
//!
//! That's the entire protocol. Stale sockets (orphaned by a
//! crash) are detected because connect-then-write fails; we
//! unlink and rebind. Permissions follow the user's umask —
//! the socket lives under their own data dir, so no extra
//! ACL is needed.
//!
//! Windows support is deferred — the team is on Linux. When we
//! add Windows we'll swap the listener for a named pipe; the
//! protocol stays the same.

use std::path::{Path, PathBuf};
use std::time::Duration;

use camino::Utf8Path;
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const FOCUS_MESSAGE: &[u8] = b"F\n";
/// How long we wait for the existing process to ack a focus
/// message before deciding "the socket is stale". 250 ms is
/// generous on a healthy local socket and short enough that a
/// genuine stale-file path doesn't make the user's launch feel
/// slow.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

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
	let connect = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&path))
		.await
		.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))??;
	let mut stream = connect;
	stream.write_all(FOCUS_MESSAGE).await?;
	stream.flush().await?;
	Ok(())
}

/// Spawn the focus listener task. The owning process keeps
/// this running for its whole life; on every accepted
/// connection it reads one message and reacts.
///
/// Currently only `Focus` is defined: bring the `main` window
/// to front. The handler ignores unknown message bodies so we
/// can extend the protocol additively later (e.g. "open file"
/// from a CLI handoff in a future phase).
pub fn spawn_focus_listener(listener: UnixListener, app: AppHandle) {
	tauri::async_runtime::spawn(async move {
		loop {
			match listener.accept().await {
				Ok((stream, _addr)) => {
					handle_connection(stream, app.clone()).await;
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

async fn handle_connection(mut stream: UnixStream, app: AppHandle) {
	let mut buf = [0u8; 16];
	let n = match tokio::time::timeout(CONNECT_TIMEOUT, stream.read(&mut buf)).await {
		Ok(Ok(n)) => n,
		Ok(Err(err)) => {
			tracing::warn!(error = %err, "focus listener read failed");
			return;
		}
		Err(_) => {
			tracing::warn!("focus listener read timed out");
			return;
		}
	};
	let msg = &buf[..n];
	if msg.starts_with(b"F") {
		focus_main_window(&app);
	} else {
		tracing::debug!(msg = ?msg, "ignored unknown focus-socket message");
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
