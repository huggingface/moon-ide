//! Spawn and supervise a local `llama-server` process (HF repo + HTTP API).

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use moon_protocol::next_edit::{NextEditServerSnapshot, NextEditServerStartParams};
use moon_protocol::MoonError;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const LOG_CAP: usize = 500;
const LINE_MAX: usize = 8000;
const POLL_MS: u64 = 400;

fn push_log(lines: &Arc<Mutex<VecDeque<String>>>, prefix: &str, text: String) {
	let mut t = text;
	if t.len() > LINE_MAX {
		t.truncate(LINE_MAX);
		t.push('…');
	}
	let line = if prefix.is_empty() { t } else { format!("{prefix}{t}") };
	let Ok(mut g) = lines.try_lock() else {
		return;
	};
	while g.len() >= LOG_CAP {
		g.pop_front();
	}
	g.push_back(line);
}

async fn pump_lines<R: tokio::io::AsyncRead + Unpin>(
	mut reader: R,
	sink: Arc<Mutex<VecDeque<String>>>,
	prefix: &'static str,
) {
	let mut buf = BufReader::new(&mut reader);
	let mut line = String::new();
	loop {
		line.clear();
		match buf.read_line(&mut line).await {
			Ok(0) => {
				return;
			}
			Ok(_) => {
				let t = line.trim_end_matches(['\r', '\n']).to_string();
				if !t.is_empty() {
					push_log(&sink, prefix, t);
				}
			}
			Err(_) => {
				return;
			}
		}
	}
}

fn resolve_program(binary: &str) -> String {
	let t = binary.trim();
	if t.is_empty() {
		return "llama-server".to_string();
	}
	t.to_string()
}

struct Inner {
	child: Option<Child>,
	logs: Arc<Mutex<VecDeque<String>>>,
	last_exit_code: Option<i32>,
	start_error: Option<String>,
	poll: Option<tokio::task::JoinHandle<()>>,
}

/// Owns an optional `llama-server` child and a ring buffer of recent log lines.
pub struct NextEditServerSupervisor {
	inner: Mutex<Inner>,
}

impl Default for NextEditServerSupervisor {
	fn default() -> Self {
		Self::new()
	}
}

impl NextEditServerSupervisor {
	pub fn new() -> Self {
		Self {
			inner: Mutex::new(Inner {
				child: None,
				logs: Arc::new(Mutex::new(VecDeque::new())),
				last_exit_code: None,
				start_error: None,
				poll: None,
			}),
		}
	}

	pub async fn snapshot(&self) -> NextEditServerSnapshot {
		let g = self.inner.lock().await;
		let running = g.child.is_some();
		let pid = g.child.as_ref().and_then(|c| c.id());
		let log_tail: Vec<String> = {
			let lg = g.logs.lock().await;
			lg.iter().rev().take(200).rev().cloned().collect()
		};
		NextEditServerSnapshot {
			running,
			pid,
			last_exit_code: g.last_exit_code,
			start_error: g.start_error.clone(),
			log_tail,
		}
	}

	async fn cleanup_process(&self) {
		let mut g = self.inner.lock().await;
		if let Some(h) = g.poll.take() {
			h.abort();
		}
		if let Some(mut c) = g.child.take() {
			let _ = c.kill().await;
			let _ = c.wait().await;
		}
	}

	/// Stop the server if running. Idempotent.
	pub async fn stop(&self) -> Result<(), MoonError> {
		self.cleanup_process().await;
		Ok(())
	}

	/// Spawn `llama-server` with `--hf-repo`. Requires `Arc` for the exit poller.
	pub async fn start(arc: Arc<Self>, params: NextEditServerStartParams) -> Result<(), MoonError> {
		let program = resolve_program(&params.llama_binary);
		let hf = params.hf_repo.trim();
		if hf.is_empty() {
			return Err(MoonError::invalid(
				"HF repo is empty — set a Hugging Face repo id (e.g. sweepai/sweep-next-edit-1.5B)",
			));
		}
		let host = params.server_host.trim();
		if host.is_empty() {
			return Err(MoonError::invalid("listen host is empty"));
		}

		arc.cleanup_process().await;

		{
			let mut g = arc.inner.lock().await;
			g.start_error = None;
			g.last_exit_code = None;
			g.logs = Arc::new(Mutex::new(VecDeque::new()));
			let mut lg = g.logs.lock().await;
			lg.push_back(format!(
				"[moon-ide] spawning `{program}` --host {host} --port {} --hf-repo {hf}",
				params.server_port
			));
		}

		let mut cmd = Command::new(&program);
		cmd.args([
			"--host",
			host,
			"--port",
			&params.server_port.to_string(),
			"--hf-repo",
			hf,
		]);
		cmd.stdin(Stdio::null());
		cmd.stdout(Stdio::piped());
		cmd.stderr(Stdio::piped());
		cmd.kill_on_drop(true);

		let mut child = match cmd.spawn() {
			Ok(c) => c,
			Err(e) => {
				let msg = format!("failed to spawn `{program}`: {e}");
				let mut g = arc.inner.lock().await;
				g.start_error = Some(msg.clone());
				return Err(MoonError::internal(msg));
			}
		};

		let logs = {
			let g = arc.inner.lock().await;
			Arc::clone(&g.logs)
		};

		let stdout = match child.stdout.take() {
			Some(s) => s,
			None => {
				let mut g = arc.inner.lock().await;
				g.start_error = Some("llama-server has no stdout".into());
				return Err(MoonError::internal("llama-server stdout"));
			}
		};
		let stderr = match child.stderr.take() {
			Some(s) => s,
			None => {
				let mut g = arc.inner.lock().await;
				g.start_error = Some("llama-server has no stderr".into());
				return Err(MoonError::internal("llama-server stderr"));
			}
		};

		let out_sink = Arc::clone(&logs);
		tokio::spawn(pump_lines(stdout, out_sink, ""));

		let err_sink = Arc::clone(&logs);
		tokio::spawn(pump_lines(stderr, err_sink, "[stderr] "));

		{
			let mut g = arc.inner.lock().await;
			g.child = Some(child);
		}

		let this = Arc::clone(&arc);
		let poll = tokio::spawn(async move {
			loop {
				tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
				let mut g = this.inner.lock().await;
				let Some(ref mut ch) = g.child else {
					return;
				};
				match ch.try_wait() {
					Ok(Some(status)) => {
						g.last_exit_code = status.code();
						g.child = None;
						return;
					}
					Err(e) => {
						let msg = format!("[moon-ide] wait error: {e}");
						{
							let mut lg = g.logs.lock().await;
							while lg.len() >= LOG_CAP {
								lg.pop_front();
							}
							lg.push_back(msg);
						}
						g.child = None;
						return;
					}
					Ok(None) => {}
				}
			}
		});

		{
			let mut g = arc.inner.lock().await;
			g.poll = Some(poll);
		}

		Ok(())
	}
}
