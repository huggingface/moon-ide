//! Desktop binding of the shared companion RPC dispatcher
//! (`moon_remote::rpc`). The dispatcher itself — every
//! `coder_*` / `workspace_*` method the phone can call — lives in
//! `moon-remote` so the headless binary serves the identical
//! surface; the desktop only contributes the Tauri-flavoured
//! pieces: the settings context (dirs + workspace id from
//! [`crate::state::AppState`]) and a launcher that runs the same
//! focus-or-spawn path as the desktop's `window_open` command.

use std::sync::Arc;

pub use moon_remote::rpc::BridgeRpcHandler;
use moon_remote::rpc::{BridgeRpc, WorkspaceLauncher};

/// Launch a sibling workspace process via the desktop's
/// focus-or-spawn path. The phone asks the bridge to open a stopped
/// workspace; the bridge forwards to the owning IDE, which spawns
/// `moon-ide --workspace <id>` exactly like a dock click would.
struct DesktopLauncher {
	app: tauri::AppHandle,
}

#[async_trait::async_trait]
impl WorkspaceLauncher for DesktopLauncher {
	async fn launch(&self, workspace_id: &str) -> Result<(), String> {
		use tauri::Manager;
		let state = self
			.app
			.try_state::<crate::state::AppState>()
			.ok_or_else(|| "app state not ready yet".to_owned())?;
		crate::commands::window::window_open_impl(state.inner(), workspace_id.to_owned())
			.await
			.map_err(|e| e.to_string())
	}
}

/// Build the desktop's bridge-RPC handler: shared dispatcher +
/// desktop launcher. `workspace_id` is `None` in preboot mode.
pub fn new_handler(
	coder: moon_coder::CoderHandle,
	workspaces: Arc<moon_core::WorkspaceRegistry>,
	app: tauri::AppHandle,
	config_dir: camino::Utf8PathBuf,
	workspaces_dir: camino::Utf8PathBuf,
	workspace_id: Option<String>,
) -> Arc<dyn BridgeRpcHandler> {
	Arc::new(BridgeRpc::new(
		coder,
		workspaces,
		moon_remote::settings::SettingsContext {
			config_dir,
			workspaces_dir,
			workspace_id,
		},
		Some(Arc::new(DesktopLauncher { app })),
	))
}
