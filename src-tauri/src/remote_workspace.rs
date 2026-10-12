//! A window whose workspace process runs on a server (ADR 0095). The
//! desktop keeps an SSH connection running `moon-remote attach`,
//! forwards the webview's `call`s over it and re-emits the server's
//! events, so the frontend runs unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use moon_app::attach::{AttachClient, Connect, Connection, RemoteStatus, REMOTE_STATUS_EVENT};
use moon_app::ports::PORTS_STATE_EVENT;
use moon_app::EventSink;
use moon_protocol::ports::ForwardedPortStatus;
use moon_protocol::workspace::{validate_workspace_id, RemoteTarget};
use moon_protocol::MoonError;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;

pub struct RemoteWorkspace {
	pub client: Arc<AttachClient>,
	/// The server's SSH connection, shared by the attach channel,
	/// tunnels and file transfers.
	pub ssh: Ssh,
	tunnels: Arc<Tunnels>,
	status: Arc<Mutex<RemoteStatus>>,
}

impl RemoteWorkspace {
	/// Start connecting. Must run on the tokio runtime.
	pub fn connect(target: &RemoteTarget, events: Arc<dyn EventSink>) -> Result<Self, MoonError> {
		validate_target(target)?;
		let ssh = Ssh {
			host: target.host.clone(),
			control_dir: control_dir()?,
		};
		let tunnels = Arc::new(Tunnels {
			ssh: ssh.clone(),
			forwarded: tokio::sync::Mutex::default(),
		});
		let status = Arc::new(Mutex::new(RemoteStatus::Connecting));
		let events: Arc<dyn EventSink> = Arc::new(StatusTracker {
			inner: events,
			status: status.clone(),
			tunnels: tunnels.clone(),
		});
		let client = AttachClient::spawn(attach_connect(ssh.clone(), target.workspace.clone()), events);
		Ok(Self {
			client,
			ssh,
			tunnels,
			status,
		})
	}

	pub fn status(&self) -> RemoteStatus {
		self.status.lock().expect("remote status poisoned").clone()
	}

	/// Forward a server call and adapt the answers that name a server
	/// loopback port, so the webview gets something it can reach.
	pub async fn call(&self, method: &str, params: Value) -> Result<Value, MoonError> {
		let result = self.client.call(method, params).await?;
		match method {
			"browser_resolve_url" => self.tunnels.localize_url(result).await,
			"ports_status" => {
				self.tunnels.forward_declared(&result);
				Ok(result)
			}
			_ => Ok(result),
		}
	}
}

/// Loopback ports on the server made reachable on this desktop, each
/// one requested explicitly (a browser tab's preview proxy, a declared
/// port forward) — never by scanning (ADR 0095, invariant 3).
struct Tunnels {
	ssh: Ssh,
	/// Server port -> desktop port.
	forwarded: tokio::sync::Mutex<HashMap<u16, u16>>,
}

impl Tunnels {
	/// The desktop port reaching `remote` on the server: the same
	/// number when `same_port` (declared forwards keep the port the
	/// user picked), otherwise any free one.
	async fn local_port(&self, remote: u16, same_port: bool) -> Result<u16, MoonError> {
		let mut forwarded = self.forwarded.lock().await;
		if let Some(local) = forwarded.get(&remote) {
			return Ok(*local);
		}
		let local = if same_port { remote } else { free_local_port()? };
		self.ssh.forward_local(local, remote).await?;
		forwarded.insert(remote, local);
		Ok(local)
	}

	/// `browser_resolve_url` answers with the server's preview proxy on
	/// `127.0.0.1:<port>`; point it at a tunnel to that port instead.
	async fn localize_url(&self, value: Value) -> Result<Value, MoonError> {
		let Some(mut url) = value.as_str().and_then(|raw| url::Url::parse(raw).ok()) else {
			return Ok(value);
		};
		let Some(port) = url.port().filter(|_| url.host_str() == Some("127.0.0.1")) else {
			return Ok(value);
		};
		let local = self.local_port(port, false).await?;
		url
			.set_port(Some(local))
			.map_err(|()| MoonError::internal("could not set tunnel port"))?;
		Ok(Value::String(url.into()))
	}

	/// Declared port forwards publish on the server's loopback; mirror
	/// each onto the same desktop port. Best-effort: a desktop port
	/// already in use is logged and skipped.
	fn forward_declared(self: &Arc<Self>, statuses: &Value) {
		let Ok(statuses) = serde_json::from_value::<Vec<ForwardedPortStatus>>(statuses.clone()) else {
			return;
		};
		for status in statuses {
			let port = status.forward.host_port;
			let tunnels = self.clone();
			tauri::async_runtime::spawn(async move {
				if let Err(err) = tunnels.local_port(port, true).await {
					tracing::warn!(port, error = %err, "could not forward declared port to this desktop");
				}
			});
		}
	}
}

fn free_local_port() -> Result<u16, MoonError> {
	let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
	Ok(listener.local_addr()?.port())
}

/// `ssh` interprets a leading `-` as an option, and the workspace slug
/// is interpolated into the remote command line.
fn validate_target(target: &RemoteTarget) -> Result<(), MoonError> {
	if target.host.is_empty() || target.host.starts_with('-') || target.host.chars().any(char::is_whitespace) {
		return Err(MoonError::invalid(format!("invalid ssh host `{}`", target.host)));
	}
	validate_workspace_id(&target.workspace)
}

/// One multiplexed connection per server (`ControlMaster`): the attach
/// channel opens it and tunnels / transfers ride it.
#[derive(Clone)]
pub struct Ssh {
	pub host: String,
	control_dir: Utf8PathBuf,
}

impl Ssh {
	/// `ssh … <host>`, ready for a remote command line to be appended.
	pub fn command(&self) -> tokio::process::Command {
		let mut command = tokio::process::Command::new("ssh");
		command
			.args(["-T", "-o", "BatchMode=yes", "-o", "ControlMaster=auto", "-o"])
			.arg(format!("ControlPath={}", self.control_dir.join("%C")))
			.args([
				"-o",
				"ControlPersist=600",
				"-o",
				"ServerAliveInterval=15",
				"-o",
				"ServerAliveCountMax=3",
			])
			.arg(&self.host)
			.kill_on_drop(true);
		command
	}

	/// Forward `127.0.0.1:local` on this desktop to `127.0.0.1:remote`
	/// on the server, through the running master connection.
	pub async fn forward_local(&self, local: u16, remote: u16) -> Result<(), MoonError> {
		let output = tokio::process::Command::new("ssh")
			.arg("-o")
			.arg(format!("ControlPath={}", self.control_dir.join("%C")))
			.args(["-O", "forward", "-L"])
			.arg(format!("127.0.0.1:{local}:127.0.0.1:{remote}"))
			.arg(&self.host)
			.output()
			.await?;
		if !output.status.success() {
			return Err(MoonError::io(format!(
				"ssh forward {local} -> {remote} failed: {}",
				String::from_utf8_lossy(&output.stderr).trim()
			)));
		}
		Ok(())
	}
}

/// Quote `value` as one word for the remote POSIX shell.
pub fn shell_quote(value: &str) -> String {
	format!("'{}'", value.replace('\'', "'\\''"))
}

fn attach_connect(ssh: Ssh, workspace: String) -> Connect {
	Box::new(move || {
		let ssh = ssh.clone();
		let workspace = workspace.clone();
		Box::pin(async move {
			// A login shell so `moon-remote` resolves through the
			// user's profile PATH (`~/.cargo/bin` and friends).
			let remote_command = format!("sh -lc 'exec moon-remote attach --workspace {workspace}'");
			let mut child = ssh
				.command()
				.arg(remote_command)
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped())
				.stderr(std::process::Stdio::piped())
				.spawn()?;
			let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
			else {
				return Err(std::io::Error::other("ssh child has no stdio"));
			};
			let host = ssh.host.clone();
			tauri::async_runtime::spawn(async move {
				let mut lines = tokio::io::BufReader::new(stderr).lines();
				while let Ok(Some(line)) = lines.next_line().await {
					tracing::info!(host = %host, "ssh: {line}");
				}
			});
			Ok(Connection {
				reader: Box::new(stdout),
				writer: Box::new(stdin),
				guard: Some(Box::new(child)),
			})
		})
	})
}

/// `ControlPath` sockets are bound to `sun_path` (~104 bytes on
/// macOS), which rules out the long per-user temp dirs there.
fn control_dir() -> Result<Utf8PathBuf, MoonError> {
	use std::os::unix::fs::PermissionsExt;
	// SAFETY: `getuid` has no preconditions and cannot fail.
	let uid = unsafe { libc::getuid() };
	let dir = Utf8PathBuf::from(format!("/tmp/moon-ssh-{uid}"));
	std::fs::create_dir_all(&dir)?;
	std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
	Ok(dir)
}

/// Remembers the latest connection status so a webview that loads
/// after the first status event can still ask for it.
struct StatusTracker {
	inner: Arc<dyn EventSink>,
	status: Arc<Mutex<RemoteStatus>>,
	tunnels: Arc<Tunnels>,
}

impl EventSink for StatusTracker {
	fn emit(&self, event: &str, payload: Value) {
		if event == REMOTE_STATUS_EVENT {
			if let Ok(status) = serde_json::from_value::<RemoteStatus>(payload.clone()) {
				*self.status.lock().expect("remote status poisoned") = status;
			}
		}
		if event == PORTS_STATE_EVENT {
			self.tunnels.forward_declared(&payload);
		}
		self.inner.emit(event, payload);
	}
}

/// Bridge-RPC handler for a remote window's focus socket: phones reach
/// a remote workspace through its server's relay, so the local bridge
/// has nothing to serve.
pub struct NoCompanion;

#[async_trait::async_trait]
impl moon_remote::rpc::BridgeRpcHandler for NoCompanion {
	async fn dispatch(&self, _method: &str, _params: Value) -> Result<Value, String> {
		Err(UNSERVED.to_owned())
	}

	async fn subscribe(&self, _method: &str, _params: Value) -> Result<tokio::sync::mpsc::Receiver<Value>, String> {
		Err(UNSERVED.to_owned())
	}
}

const UNSERVED: &str = "remote workspace: pair the phone with the server running it";
