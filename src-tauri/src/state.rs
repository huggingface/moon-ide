use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;
use moon_slack::{SlackClient, TokenStore};
use tokio::sync::RwLock;

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
	pub fn new(config_dir: Utf8PathBuf) -> Self {
		Self {
			workspaces: WorkspaceRegistry::new(),
			config_dir,
			slack: SlackState::default(),
		}
	}
}

#[derive(Default)]
pub struct SlackState {
	pub tokens: TokenStore,
	client: RwLock<Option<SlackClient>>,
}

impl SlackState {
	pub async fn client(&self) -> Option<SlackClient> {
		self.client.read().await.clone()
	}

	pub async fn set_client(&self, client: SlackClient) {
		*self.client.write().await = Some(client);
	}

	pub async fn clear_client(&self) {
		*self.client.write().await = None;
	}
}
