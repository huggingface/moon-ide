use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;
use moon_slack::{SlackClient, TokenStore};
use tokio::sync::RwLock;

use crate::slack_poller::PollerHandle;

pub struct AppState {
	pub workspaces: WorkspaceRegistry,
	/// Where global, machine-local app state lives (last opened folder, etc.).
	/// Set once at startup from Tauri's `app_config_dir`.
	pub config_dir: Utf8PathBuf,
	/// Slack chat panel state. The token itself lives in the OS keyring;
	/// this is the in-memory client cache (populated at startup if the
	/// keyring has a token, otherwise lazily on first `slack_set_token`).
	pub slack: SlackState,
}

impl AppState {
	pub fn new(config_dir: Utf8PathBuf, slack: SlackState) -> Self {
		Self {
			workspaces: WorkspaceRegistry::new(),
			config_dir,
			slack,
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
			tokens: TokenStore::default(),
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
