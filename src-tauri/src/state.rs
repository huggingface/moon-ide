use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_core::NextEditServerSupervisor;
use moon_slack::{SlackClient, TokenStore};
use tokio::sync::{Mutex, RwLock};
use tokio::task::AbortHandle;

use crate::slack_poller::PollerHandle;

pub struct AppState {
	/// Where this window's workspace process runs; the frontend's
	/// `call`s reach it through `commands::rpc::rpc`.
	pub backend: Backend,
	/// Where global, machine-local app state lives (theme,
	/// catalog, slack creds). Set once at startup from Tauri's
	/// `app_config_dir`.
	pub config_dir: Utf8PathBuf,
	/// Root of moon-ide's per-workspace state directories — one
	/// subdirectory per workspace id holds that workspace's
	/// `compose.yaml` / `bound-folders.json` / `session.json` /
	/// `instance.sock`. Resolved once at startup as
	/// `<dirs::data_local_dir>/moon-ide/workspaces/`.
	pub workspaces_dir: Utf8PathBuf,
	/// Slack chat panel state. The token itself lives in the OS keyring;
	/// this is the in-memory client cache (populated at startup if the
	/// keyring has a token, otherwise lazily on first `slack_set_token`).
	pub slack: SlackState,
	/// Optional `llama-server` child for local autocomplete (HF `--hf-repo`).
	pub next_edit_server: Arc<NextEditServerSupervisor>,
	/// Process mode + identity. The frontend's first hydrate
	/// reads this via [`crate::commands::app_info::app_info`]
	/// to decide whether to render the preboot landing or the
	/// regular IDE shell.
	pub mode: AppMode,
	/// Abort handle for the per-workspace focus-socket listener
	/// task (see [`crate::focus_socket::spawn_focus_listener`]).
	/// Held so shutdown can drop the listening `UnixListener`
	/// *before* the slow `stop_all` work runs — otherwise a
	/// relaunch probing `instance.sock` connects successfully
	/// for the whole shutdown window and reports the workspace
	/// as still in use. `None` in preboot mode (no socket bound).
	pub focus_listener: Mutex<Option<AbortHandle>>,
	/// Outbound remote-bridge connection handle (Phase 14.3, ADR
	/// 0031). `None` when the IDE isn't connected to a remote bridge
	/// (local mode). Held so the `companion_remote_status` /
	/// `companion_remote_disconnect` commands can reach it.
	pub remote_bridge: Mutex<Option<crate::remote_bridge::RemoteBridgeHandle>>,
}

/// Where the workspace process behind this window lives (ADR 0095).
pub enum Backend {
	/// In this process. The workspace is the process's own: in
	/// preboot mode (no `--workspace` arg, empty catalog) its id is
	/// the sentinel [`PREBOOT_WORKSPACE_ID`] and it never gains
	/// folders.
	Local(Arc<moon_app::AppCore>),
	/// On a server, reached over SSH.
	Remote(crate::remote_workspace::RemoteWorkspace),
}

/// What this process is doing. Picked once at startup based on
/// CLI args + catalog state; never mutates afterwards.
#[derive(Debug, Clone)]
pub enum AppMode {
	/// `moon-ide --workspace <slug>` mode. The registry is
	/// bound to `slug`, every IPC operates on it. The user
	/// closes this window with Ctrl+Shift+W → process exits.
	Workspace { id: String },
	/// `moon-ide` with no args, catalog empty. The frontend
	/// shows the workspace-naming landing UI; on submit it
	/// calls `workspace_create` then `window_open(slug)` which
	/// spawns a real `--workspace <slug>` child and exits this
	/// preboot.
	Preboot,
}

/// Sentinel id used by the preboot registry. Starts with `_`
/// so [`moon_protocol::workspace::validate_workspace_id`]
/// rejects it as user input — there's no path by which a real
/// workspace can collide.
pub const PREBOOT_WORKSPACE_ID: &str = "__preboot__";

impl AppState {
	pub fn new(
		config_dir: Utf8PathBuf,
		workspaces_dir: Utf8PathBuf,
		slack: SlackState,
		mode: AppMode,
		backend: Backend,
	) -> Self {
		Self {
			backend,
			config_dir,
			workspaces_dir,
			slack,
			next_edit_server: Arc::new(NextEditServerSupervisor::default()),
			mode,
			focus_listener: Mutex::new(None),
			remote_bridge: Mutex::new(None),
		}
	}

	/// The in-process workspace, or `None` for a remote window.
	pub fn local_core(&self) -> Option<&Arc<moon_app::AppCore>> {
		match &self.backend {
			Backend::Local(core) => Some(core),
			Backend::Remote(_) => None,
		}
	}

	/// Path of the per-workspace state directory for the given
	/// id (e.g. `<workspaces_dir>/huggingface/`). The directory
	/// itself is created lazily on first compose write — the
	/// existence check belongs in `moon-container`'s lifecycle
	/// layer, not here.
	pub fn workspace_state_dir(&self, workspace_id: &str) -> Utf8PathBuf {
		self.workspaces_dir.join(workspace_id)
	}

	/// Workspace id this process owns, or `None` in preboot
	/// mode. Cheap inline copy; called from every command that
	/// needs to derive a workspace-scoped path.
	pub fn workspace_id(&self) -> Option<&str> {
		match &self.mode {
			AppMode::Workspace { id } => Some(id.as_str()),
			AppMode::Preboot => None,
		}
	}
}

pub struct SlackState {
	pub tokens: TokenStore,
	/// `Arc<RwLock<…>>` so the poll task can clone a handle and read
	/// the live client without going back through Tauri's state map.
	pub client: Arc<RwLock<Option<SlackClient>>>,
	/// Drives the background polling loop — set panel visibility,
	/// active thread, OS focus from the matching commands and the
	/// loop wakes up. See [`crate::slack_poller`].
	pub poller: PollerHandle,
}

impl SlackState {
	pub fn new(client: Arc<RwLock<Option<SlackClient>>>, poller: PollerHandle) -> Self {
		Self {
			tokens: TokenStore,
			client,
			poller,
		}
	}

	pub async fn current_client(&self) -> Option<SlackClient> {
		self.client.read().await.clone()
	}

	pub async fn set_client(&self, client: SlackClient) {
		*self.client.write().await = Some(client);
	}

	pub async fn clear_client(&self) {
		*self.client.write().await = None;
	}
}
