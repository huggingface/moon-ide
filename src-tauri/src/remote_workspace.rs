//! A window whose workspace process runs on a server (ADR 0095). The
//! desktop keeps an SSH connection running `moon-remote attach`,
//! forwards the webview's `call`s over it and re-emits the server's
//! events, so the frontend runs unchanged.

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use moon_app::attach::{AttachClient, Connect, Connection, RemoteStatus, REMOTE_STATUS_EVENT};
use moon_app::EventSink;
use moon_protocol::workspace::{validate_workspace_id, RemoteTarget};
use moon_protocol::MoonError;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;

pub struct RemoteWorkspace {
	pub client: Arc<AttachClient>,
	status: Arc<Mutex<RemoteStatus>>,
}

impl RemoteWorkspace {
	/// Start connecting. Must run on the tokio runtime.
	pub fn connect(target: &RemoteTarget, events: Arc<dyn EventSink>) -> Result<Self, MoonError> {
		validate_target(target)?;
		let status = Arc::new(Mutex::new(RemoteStatus::Connecting));
		let events: Arc<dyn EventSink> = Arc::new(StatusTracker {
			inner: events,
			status: status.clone(),
		});
		let client = AttachClient::spawn(ssh_connect(target.clone())?, events);
		Ok(Self { client, status })
	}

	pub fn status(&self) -> RemoteStatus {
		self.status.lock().expect("remote status poisoned").clone()
	}
}

/// `ssh` interprets a leading `-` as an option, and the workspace slug
/// is interpolated into the remote command line.
fn validate_target(target: &RemoteTarget) -> Result<(), MoonError> {
	if target.host.is_empty() || target.host.starts_with('-') || target.host.chars().any(char::is_whitespace) {
		return Err(MoonError::invalid(format!("invalid ssh host `{}`", target.host)));
	}
	validate_workspace_id(&target.workspace)
}

/// One multiplexed connection per server (`ControlMaster`), so
/// reconnects and, later, tunnels and file transfers reuse it.
fn ssh_connect(target: RemoteTarget) -> Result<Connect, MoonError> {
	let control_dir = control_dir()?;
	Ok(Box::new(move || {
		let target = target.clone();
		let control_path = control_dir.join("%C");
		Box::pin(async move {
			// A login shell so `moon-remote` resolves through the
			// user's profile PATH (`~/.cargo/bin` and friends).
			let remote_command = format!("sh -lc 'exec moon-remote attach --workspace {}'", target.workspace);
			let mut child = tokio::process::Command::new("ssh")
				.args(["-T", "-o", "BatchMode=yes", "-o", "ControlMaster=auto", "-o"])
				.arg(format!("ControlPath={control_path}"))
				.args([
					"-o",
					"ControlPersist=600",
					"-o",
					"ServerAliveInterval=15",
					"-o",
					"ServerAliveCountMax=3",
				])
				.arg(&target.host)
				.arg(remote_command)
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped())
				.stderr(std::process::Stdio::piped())
				.kill_on_drop(true)
				.spawn()?;
			let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
			else {
				return Err(std::io::Error::other("ssh child has no stdio"));
			};
			let host = target.host.clone();
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
	}))
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
}

impl EventSink for StatusTracker {
	fn emit(&self, event: &str, payload: Value) {
		if event == REMOTE_STATUS_EVENT {
			if let Ok(status) = serde_json::from_value::<RemoteStatus>(payload.clone()) {
				*self.status.lock().expect("remote status poisoned") = status;
			}
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
