use std::collections::HashMap;
use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;
use moon_slack::{SlackClient, TokenStore};
use tokio::sync::{Mutex, RwLock};
use tokio::task::AbortHandle;

use crate::slack_poller::PollerHandle;

pub struct AppState {
	pub workspaces: WorkspaceRegistry,
	/// Where global, machine-local app state lives (last opened folder, etc.).
	/// Set once at startup from Tauri's `app_config_dir`.
	pub config_dir: Utf8PathBuf,
	/// Root of moon-ide's per-workspace state directories — one
	/// subdirectory per workspace id holds that workspace's
	/// `compose.yaml` and `bound-folders.json`. Resolved once at
	/// startup as `<dirs::data_local_dir>/moon-ide/workspaces/`.
	/// Decoupled from the workspace folder set so the compose
	/// project survives folder switches (see ADR 0007 amendment).
	pub workspaces_dir: Utf8PathBuf,
	/// Slack chat panel state. The token itself lives in the OS keyring;
	/// this is the in-memory client cache (populated at startup if the
	/// keyring has a token, otherwise lazily on first `slack_set_token`).
	pub slack: SlackState,
	/// Registry of active `docker compose logs -f` streams, keyed
	/// by the stream ID returned to the frontend. Each entry holds
	/// the `AbortHandle` of the supervisor task that owns the
	/// child process — aborting it drops the child, which is
	/// spawned with `kill_on_drop(true)` so the SIGKILL goes out
	/// immediately. See [`crate::commands::compose_logs`].
	pub log_streams: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

impl AppState {
	pub fn new(config_dir: Utf8PathBuf, workspaces_dir: Utf8PathBuf, slack: SlackState) -> Self {
		Self {
			workspaces: WorkspaceRegistry::new(),
			config_dir,
			workspaces_dir,
			slack,
			log_streams: Arc::new(Mutex::new(HashMap::new())),
		}
	}

	/// Path of the per-workspace state directory for the given
	/// id (e.g. `<workspaces_dir>/default/`). The directory itself
	/// is created lazily on first compose write — the existence
	/// check belongs in `moon-container`'s lifecycle layer, not
	/// here.
	pub fn workspace_state_dir(&self, workspace_id: &str) -> Utf8PathBuf {
		self.workspaces_dir.join(workspace_id)
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
