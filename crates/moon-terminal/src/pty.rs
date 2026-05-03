//! Thin async wrapper around `portable_pty`.
//!
//! `portable_pty` is sync (read/write/wait are blocking), so
//! every PTY runs on its own `spawn_blocking` thread for
//! reads and a separate one for the `wait()` call. Writes are
//! cheap enough to do directly on the caller's thread (we hold
//! the writer behind a tokio mutex).
//!
//! See [ADR 0009](../../../specs/decisions/0009-terminal-pty-and-targets.md)
//! for why portable-pty rather than libc / pty-process / bollard.

use std::io::{Read, Write};
use std::sync::Arc;

use portable_pty::{native_pty_system, ChildKiller, MasterPty, PtySize};
use tokio::sync::{mpsc, Mutex};

use crate::target::TerminalTarget;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
	#[error("PTY allocation failed: {0}")]
	Open(String),
	#[error("spawn failed: {0}")]
	Spawn(String),
	#[error("write failed: {0}")]
	Write(String),
	#[error("resize failed: {0}")]
	Resize(String),
}

/// Active PTY session: a running child process, the master
/// half of its PTY, and a channel of bytes the supervisor
/// pumps from the reader thread.
///
/// Closing means dropping `Self` — the inner `Child` killer is
/// invoked on drop so neither the host shell nor the
/// `docker exec` survive.
pub struct PtySession {
	master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
	writer: Arc<Mutex<Box<dyn Write + Send>>>,
	output_rx: mpsc::Receiver<Vec<u8>>,
	exit_rx: mpsc::Receiver<Option<i32>>,
	killer: Box<dyn ChildKiller + Send + Sync>,
}

impl PtySession {
	/// Read the next chunk of output (raw bytes — escapes,
	/// partial UTF-8, the lot). Returns `None` when the reader
	/// thread has exited (PTY closed).
	pub async fn next_output(&mut self) -> Option<Vec<u8>> {
		self.output_rx.recv().await
	}

	/// Wait for the child to exit. Returns its exit code if
	/// captured, or `None` if portable-pty couldn't surface
	/// one (signal, daemon weirdness). Resolves at most once;
	/// subsequent calls return `None`.
	pub async fn next_exit(&mut self) -> Option<i32> {
		self.exit_rx.recv().await.flatten()
	}

	/// Send `data` to the child via the master PTY.
	pub async fn write(&self, data: &[u8]) -> Result<(), PtyError> {
		let mut writer = self.writer.lock().await;
		writer.write_all(data).map_err(|e| PtyError::Write(e.to_string()))?;
		writer.flush().map_err(|e| PtyError::Write(e.to_string()))?;
		Ok(())
	}

	/// Push a new size to the master PTY. The kernel raises
	/// SIGWINCH inside the container too — `docker exec`
	/// bridges it through.
	pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
		let master = self.master.lock().await;
		master
			.resize(PtySize {
				rows,
				cols,
				pixel_width: 0,
				pixel_height: 0,
			})
			.map_err(|e| PtyError::Resize(e.to_string()))
	}
}

impl Drop for PtySession {
	fn drop(&mut self) {
		// SIGKILL the child eagerly. The reader thread observes
		// the resulting EOF and exits; the wait thread observes
		// the exit and exits. Both threads' channels close, the
		// supervisor task on the consumer side picks it up via
		// a `None` from `next_output`.
		if let Err(e) = self.killer.kill() {
			tracing::warn!(error = %e, "PtySession::drop failed to kill child");
		}
	}
}

/// Allocate a PTY, spawn the target's command in it, and start
/// the read / wait pump threads. Returns a `PtySession` whose
/// `next_output` channel begins yielding bytes as soon as the
/// child writes anything.
pub fn spawn(target: &TerminalTarget, cols: u16, rows: u16) -> Result<PtySession, PtyError> {
	let pty_system = native_pty_system();
	let pair = pty_system
		.openpty(PtySize {
			rows,
			cols,
			pixel_width: 0,
			pixel_height: 0,
		})
		.map_err(|e| PtyError::Open(e.to_string()))?;

	let cmd = target.to_command();
	let mut child = pair
		.slave
		.spawn_command(cmd)
		.map_err(|e| PtyError::Spawn(e.to_string()))?;
	let killer = child.clone_killer();

	// Take the reader/writer once — `MasterPty` returns owned
	// boxes and rejects subsequent calls.
	let mut reader = pair
		.master
		.try_clone_reader()
		.map_err(|e| PtyError::Open(format!("clone reader: {e}")))?;
	let writer = pair
		.master
		.take_writer()
		.map_err(|e| PtyError::Open(format!("take writer: {e}")))?;

	// Drop the slave: the child's already inherited it, and
	// holding ours would keep the PTY alive past the child's
	// exit (preventing the reader from seeing EOF).
	drop(pair.slave);

	let master = Arc::new(Mutex::new(pair.master));
	let writer = Arc::new(Mutex::new(writer));

	// Bounded channel: 64 chunks ≈ 256 KB at the 4 KB chunk
	// size the reader uses. If the consumer falls behind we
	// apply backpressure on the kernel buffer rather than
	// growing memory unboundedly.
	let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(64);
	let (exit_tx, exit_rx) = mpsc::channel::<Option<i32>>(1);

	// Reader thread. portable_pty's reader is blocking; one
	// thread per PTY is the canonical pattern.
	std::thread::Builder::new()
		.name("moon-pty-reader".into())
		.spawn(move || {
			let mut buf = [0u8; 4096];
			loop {
				match reader.read(&mut buf) {
					Ok(0) => break,
					Ok(n) => {
						// `blocking_send` blocks the OS thread
						// when the channel is full — exactly
						// what we want for backpressure.
						if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
							break;
						}
					}
					Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
					Err(_) => break,
				}
			}
		})
		.map_err(|e| PtyError::Spawn(format!("reader thread: {e}")))?;

	// Wait thread. `child.wait()` is sync; we surface the exit
	// status to the supervisor via a one-shot mpsc.
	std::thread::Builder::new()
		.name("moon-pty-wait".into())
		.spawn(move || {
			let status = child.wait().ok();
			let code = status.and_then(|s| {
				let raw = s.exit_code();
				// portable_pty exposes exit codes as u32 (it
				// folds signal exits to ≥128 like the shell);
				// downcast to i32 for the protocol surface.
				i32::try_from(raw).ok()
			});
			let _ = exit_tx.blocking_send(code);
		})
		.map_err(|e| PtyError::Spawn(format!("wait thread: {e}")))?;

	Ok(PtySession {
		master,
		writer,
		output_rx,
		exit_rx,
		killer,
	})
}
