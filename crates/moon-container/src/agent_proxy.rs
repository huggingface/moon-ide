//! Host-side ssh-agent proxy (ADR 0060).
//!
//! Dev containers used to bind-mount the host's `$SSH_AUTH_SOCK`
//! **file**. A file bind-mount pins the inode, so the moment the
//! host agent restarts (re-login, keyring restart) every container
//! held a dead socket until it was recreated — the same stale-inode
//! failure ADR 0026 fixed for `instance.sock` by mounting a
//! directory instead.
//!
//! The fix has two halves:
//!
//! - This module: the IDE's host process listens on a **stable**
//!   socket at `<moon data dir>/ssh-agent/ssh-auth.sock` and pipes
//!   each connection to whatever host agent is *currently* alive
//!   (re-resolved per connection).
//! - `compose.rs`: the container mounts that socket's parent
//!   **directory** at `/run/host-services`, so the proxy can rebind
//!   across IDE restarts without the container's mount going stale.
//!
//! Why not mount the live agent socket's own parent directory?
//! For gnome-keyring the agent socket lives in
//! `$XDG_RUNTIME_DIR/keyring/` **next to the Secret Service control
//! socket** — mounting that directory would hand every container a
//! path to the host keyring. The proxy directory contains exactly
//! one socket, ours.
//!
//! Linux-only: macOS containers use Docker Desktop's magic
//! `/run/host-services/ssh-auth.sock`, which is already a stable
//! host-managed forward.

use std::sync::OnceLock;

use camino::{Utf8Path, Utf8PathBuf};

/// Directory (under the moon data dir) holding only the proxy
/// socket. This is what compose mounts.
pub const PROXY_DIR_NAME: &str = "ssh-agent";
/// Socket file name — matches the Docker Desktop convention so the
/// in-container path is `/run/host-services/ssh-auth.sock` on every
/// platform.
pub const PROXY_SOCKET_NAME: &str = "ssh-auth.sock";

/// Set once by [`spawn`]; read by the compose layer
/// (`lifecycle::detect_ssh_agent_forward`) to decide between the
/// proxy-dir mount and the legacy direct-socket fallback.
static PROXY_DIR: OnceLock<Utf8PathBuf> = OnceLock::new();

/// The proxy directory registered by [`spawn`], if the proxy is
/// running in this process.
pub fn registered_dir() -> Option<&'static Utf8Path> {
	PROXY_DIR.get().map(Utf8PathBuf::as_path)
}

/// Connect timeout for one upstream-agent candidate. Local Unix
/// sockets either accept immediately or are dead.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// How often each IDE process re-checks that *some* live listener
/// serves the proxy socket. Process-per-workspace (ADR 0014) means
/// several IDE processes run concurrently and any of them may exit
/// at any time — whoever notices a dead socket first takes over.
const KEEPER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// Ensure the proxy socket is served, and register its directory for
/// the compose layer. Multi-process safe: if a sibling IDE process
/// already serves the socket we adopt it, and a keeper task
/// periodically re-probes and takes over when the owner exits — so
/// the proxy outlives any individual workspace process. Idempotent
/// per process. Errors from the *first* claim attempt are returned
/// (permissions, exotic platforms); later takeover failures only
/// warn — the next tick retries.
pub async fn spawn(moon_data_dir: &Utf8Path) -> std::io::Result<()> {
	if PROXY_DIR.get().is_some() {
		return Ok(());
	}
	let dir = moon_data_dir.join(PROXY_DIR_NAME);
	tokio::fs::create_dir_all(dir.as_std_path()).await?;
	let socket = dir.join(PROXY_SOCKET_NAME);
	if let Some(listener) = try_claim(&socket).await? {
		spawn_accept_loop(listener, socket.clone());
	}
	let keeper_socket = socket.clone();
	tokio::spawn(async move {
		loop {
			tokio::time::sleep(KEEPER_INTERVAL).await;
			// A live listener (ours or a sibling's) answers the
			// probe; nothing to do. A refused connect means the
			// owner died — claim the path.
			if probe(&keeper_socket).await {
				continue;
			}
			match try_claim(&keeper_socket).await {
				Ok(Some(listener)) => {
					tracing::info!("ssh-agent proxy: took over the socket from an exited sibling");
					spawn_accept_loop(listener, keeper_socket.clone());
				}
				Ok(None) => {} // a sibling won the race — fine
				Err(err) => tracing::warn!(error = %err, "ssh-agent proxy: takeover failed; retrying"),
			}
		}
	});
	let _ = PROXY_DIR.set(dir);
	Ok(())
}

/// Bind the proxy socket if nobody live owns it. `Ok(None)` when a
/// live sibling serves it (adopt); probe-before-unlink so we never
/// yank a working socket out from under a sibling process — the bug
/// the first version of this module shipped with.
async fn try_claim(socket: &Utf8Path) -> std::io::Result<Option<tokio::net::UnixListener>> {
	match tokio::net::UnixListener::bind(socket.as_std_path()) {
		Ok(listener) => Ok(Some(listener)),
		Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
			if probe(socket).await {
				return Ok(None);
			}
			match tokio::fs::remove_file(socket.as_std_path()).await {
				Ok(()) => {}
				Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
				Err(err) => return Err(err),
			}
			match tokio::net::UnixListener::bind(socket.as_std_path()) {
				Ok(listener) => Ok(Some(listener)),
				// Lost a takeover race to a sibling — treat as adopt.
				Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => Ok(None),
				Err(err) => Err(err),
			}
		}
		Err(err) => Err(err),
	}
}

/// Connect-probe: only a live listener accepts.
async fn probe(socket: &Utf8Path) -> bool {
	let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::UnixStream::connect(socket.as_std_path())).await;
	matches!(connect, Ok(Ok(_)))
}

/// Serve one claimed listener. A superseded loop (its socket file
/// replaced after a takeover race) just idles harmlessly — nothing
/// can connect to an unlinked inode.
fn spawn_accept_loop(listener: tokio::net::UnixListener, own_socket: Utf8PathBuf) {
	tokio::spawn(async move {
		loop {
			let Ok((downstream, _)) = listener.accept().await else {
				return;
			};
			let own = own_socket.clone();
			tokio::spawn(async move {
				if let Err(err) = forward_connection(downstream, &own).await {
					tracing::debug!(error = %err, "ssh-agent proxy connection ended");
				}
			});
		}
	});
}

/// Pipe one downstream (container) connection to the live host
/// agent, resolved fresh per connection.
async fn forward_connection(mut downstream: tokio::net::UnixStream, own_socket: &Utf8Path) -> std::io::Result<()> {
	let Some(mut upstream) = connect_live_agent(own_socket).await else {
		// No agent reachable right now: closing immediately gives
		// ssh the same "agent refused" it would see agent-less.
		return Ok(());
	};
	tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await?;
	Ok(())
}

/// Try each candidate agent socket in order; first accepted connect
/// wins. `own_socket` is excluded so an environment that points
/// `SSH_AUTH_SOCK` at the proxy itself can't loop.
async fn connect_live_agent(own_socket: &Utf8Path) -> Option<tokio::net::UnixStream> {
	for candidate in candidate_sockets() {
		if candidate == own_socket {
			continue;
		}
		let connect = tokio::time::timeout(
			CONNECT_TIMEOUT,
			tokio::net::UnixStream::connect(candidate.as_std_path()),
		)
		.await;
		if let Ok(Ok(stream)) = connect {
			return Some(stream);
		}
	}
	tracing::warn!("ssh-agent proxy: no live host agent found (is one running?)");
	None
}

/// Candidate host agent sockets, most specific first: the process
/// environment, then the stable per-login paths the common Linux
/// agents use (gnome-keyring, gcr's ssh-agent, systemd's
/// `ssh-agent.socket` user unit).
fn candidate_sockets() -> Vec<Utf8PathBuf> {
	let mut out = Vec::new();
	if let Ok(env) = std::env::var("SSH_AUTH_SOCK") {
		if !env.is_empty() {
			out.push(Utf8PathBuf::from(env));
		}
	}
	if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
		if !runtime.is_empty() {
			let base = Utf8PathBuf::from(runtime);
			out.push(base.join("keyring/ssh"));
			out.push(base.join("gcr/ssh"));
			out.push(base.join("ssh-agent.socket"));
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn try_claim_adopts_live_listener_and_takes_over_stale_socket() {
		let tmp = tempfile::tempdir().expect("tempdir");
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 tempdir");
		let socket = root.join("sock");

		// Live sibling: adopt (None), and the sibling's socket must
		// survive the call — probe-before-unlink.
		let sibling = tokio::net::UnixListener::bind(socket.as_std_path()).expect("sibling bind");
		assert!(try_claim(&socket).await.expect("claim vs live").is_none());
		assert!(probe(&socket).await, "sibling socket must not be unlinked");
		drop(sibling);

		// Dead sibling (file left behind, nobody accepting): take over.
		assert!(try_claim(&socket).await.expect("claim vs stale").is_some());
	}

	#[tokio::test]
	async fn proxy_pipes_to_live_agent_and_survives_agent_restart() {
		let tmp = tempfile::tempdir().expect("tempdir");
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).expect("utf8 tempdir");

		// Fake "agent": echoes one byte back.
		let agent_path = root.join("fake-agent.sock");
		let spawn_agent = |path: Utf8PathBuf| {
			let listener = std::os::unix::net::UnixListener::bind(path.as_std_path()).expect("bind fake agent");
			listener.set_nonblocking(true).expect("nonblocking");
			let listener = tokio::net::UnixListener::from_std(listener).expect("tokio listener");
			tokio::spawn(async move {
				while let Ok((mut conn, _)) = listener.accept().await {
					tokio::spawn(async move {
						use tokio::io::{AsyncReadExt, AsyncWriteExt};
						let mut buf = [0u8; 1];
						if conn.read_exact(&mut buf).await.is_ok() {
							let _ = conn.write_all(&buf).await;
						}
					});
				}
			})
		};
		let agent_task = spawn_agent(agent_path.clone());
		// Point the env at the fake agent for candidate resolution.
		// (Serialised: cargo runs tests in one process; this is the
		// only test mutating SSH_AUTH_SOCK.)
		unsafe { std::env::set_var("SSH_AUTH_SOCK", agent_path.as_str()) };

		spawn(&root).await.expect("spawn proxy");
		let proxy_socket = root.join(PROXY_DIR_NAME).join(PROXY_SOCKET_NAME);

		let roundtrip = || async {
			use tokio::io::{AsyncReadExt, AsyncWriteExt};
			let mut conn = tokio::net::UnixStream::connect(proxy_socket.as_std_path())
				.await
				.expect("connect proxy");
			conn.write_all(b"x").await.expect("write");
			let mut buf = [0u8; 1];
			conn.read_exact(&mut buf).await.expect("read");
			assert_eq!(&buf, b"x");
		};
		roundtrip().await;

		// "Restart" the agent: kill it, unlink, rebind at the same
		// path. A file bind-mount would now be stale; the proxy
		// re-resolves per connection and keeps working.
		agent_task.abort();
		std::fs::remove_file(agent_path.as_std_path()).expect("unlink agent");
		let _agent2 = spawn_agent(agent_path.clone());
		roundtrip().await;
	}
}
