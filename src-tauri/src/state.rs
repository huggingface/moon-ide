use std::collections::HashMap;
use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_coder::CoderHandle;
use moon_core::{LogSink, NextEditServerSupervisor, WorkspaceRegistry};
use moon_slack::{SlackClient, TokenStore};
use tokio::sync::{Mutex, RwLock};
use tokio::task::AbortHandle;

use crate::commands::lsp::LspHandle;
use crate::fs_watcher::FsWatcherHandle;
use crate::slack_poller::PollerHandle;

pub struct AppState {
	/// The single workspace this process owns. Phase 7's
	/// process-per-workspace model: each `moon-ide` process is
	/// pinned to one workspace at startup (CLI arg
	/// `--workspace <slug>`), so the registry is a singleton —
	/// no map, no per-call resolution. Cross-workspace
	/// operations (`workspace_create`, the picker, focus
	/// existing) go cross-process via the per-workspace lock
	/// socket; see [`crate::commands::window`] +
	/// [`crate::focus_socket`].
	///
	/// In preboot mode (no `--workspace` arg, empty catalog)
	/// the registry id is the sentinel
	/// [`PREBOOT_WORKSPACE_ID`] and the registry never gains
	/// folders; the frontend renders the "Name your workspace"
	/// landing instead of the regular IDE chrome.
	pub workspaces: Arc<WorkspaceRegistry>,
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
	/// Registry of active `docker compose logs -f` streams.
	pub log_streams: Arc<Mutex<HashMap<String, AbortHandle>>>,
	/// Registry of active terminal sessions.
	pub terminal_streams: Arc<Mutex<HashMap<String, TerminalStreamHandle>>>,
	/// Filesystem watcher actor.
	pub fs_watcher: FsWatcherHandle,
	/// LSP broker plus its event-pump task.
	pub lsp: Arc<Mutex<Option<LspHandle>>>,
	/// In-process AI coding agent (Phase 6).
	pub coder: CoderHandle,
	/// Optional `llama-server` child for local autocomplete (HF `--hf-repo`).
	pub next_edit_server: Arc<NextEditServerSupervisor>,
	/// Process mode + identity. The frontend's first hydrate
	/// reads this via [`crate::commands::app_info::app_info`]
	/// to decide whether to render the preboot landing or the
	/// regular IDE shell.
	pub mode: AppMode,
	/// Diagnostic log sink. Wired into the LSP broker (and,
	/// later, format-on-save / fs-watcher / git) so user-
	/// facing breadcrumbs are available in the bottom-panel
	/// logs view instead of only in launcher stderr.
	pub logs: Arc<LogSink>,
	/// Abort handle for the per-workspace focus-socket listener
	/// task (see [`crate::focus_socket::spawn_focus_listener`]).
	/// Held so shutdown can drop the listening `UnixListener`
	/// *before* the slow `stop_all` work runs — otherwise a
	/// relaunch probing `instance.sock` connects successfully
	/// for the whole shutdown window and reports the workspace
	/// as still in use. `None` in preboot mode (no socket bound).
	pub focus_listener: Mutex<Option<AbortHandle>>,
	/// Flipped to `true` once the launch-time workspace-shell
	/// auto-resume ([`crate::shutdown::auto_resume_shell`]) has
	/// settled. `container_await_startup` waits on it so the
	/// startup terminal auto-spawn sees the post-resume container
	/// state instead of racing the resume and silently falling
	/// back to a host terminal. Starts `true` in preboot mode —
	/// there is no shell to resume.
	pub shell_auto_resume_settled: tokio::sync::watch::Sender<bool>,
	/// Outbound remote-bridge connection handle (Phase 14.3, ADR
	/// 0031). `None` when the IDE isn't connected to a remote bridge
	/// (local mode). Held so the `companion_remote_status` /
	/// `companion_remote_disconnect` commands can reach it.
	pub remote_bridge: Mutex<Option<crate::remote_bridge::RemoteBridgeHandle>>,
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

/// Owning handle the terminal commands keep per stream. The
/// `tx` channel is read by the supervisor task; sending fails
/// once the supervisor exits (process dead) so write commands
/// translate that to a no-op.
pub struct TerminalStreamHandle {
	pub tx: tokio::sync::mpsc::Sender<TerminalCommand>,
	pub abort: AbortHandle,
}

/// Inputs the terminal supervisor accepts on its mpsc channel.
/// Resize is in-band so we don't need a second mutex around the
/// `PtySession`.
pub enum TerminalCommand {
	Write(Vec<u8>),
	Resize { cols: u16, rows: u16 },
}

impl AppState {
	// Eight fields by design: this is the process's whole world
	// (workspace registry, paths, every long-lived actor handle).
	// A builder or grouping struct would just shuffle the same
	// arity around without paying for itself.
	#[allow(clippy::too_many_arguments)]
	pub fn new(
		config_dir: Utf8PathBuf,
		workspaces_dir: Utf8PathBuf,
		slack: SlackState,
		fs_watcher: FsWatcherHandle,
		workspaces: Arc<WorkspaceRegistry>,
		coder: CoderHandle,
		mode: AppMode,
		logs: Arc<LogSink>,
	) -> Self {
		let (shell_auto_resume_settled, _) = tokio::sync::watch::channel(matches!(mode, AppMode::Preboot));
		Self {
			workspaces,
			config_dir,
			workspaces_dir,
			slack,
			log_streams: Arc::new(Mutex::new(HashMap::new())),
			terminal_streams: Arc::new(Mutex::new(HashMap::new())),
			fs_watcher,
			lsp: Arc::new(Mutex::new(None)),
			coder,
			next_edit_server: Arc::new(NextEditServerSupervisor::default()),
			mode,
			logs,
			focus_listener: Mutex::new(None),
			shell_auto_resume_settled,
			remote_bridge: Mutex::new(None),
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
