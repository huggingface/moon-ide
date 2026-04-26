use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;

pub struct AppState {
	pub workspaces: WorkspaceRegistry,
	/// Where global, machine-local app state lives (last opened folder, etc.).
	/// Set once at startup from Tauri's `app_config_dir`.
	pub config_dir: Utf8PathBuf,
}

impl AppState {
	pub fn new(config_dir: Utf8PathBuf) -> Self {
		Self {
			workspaces: WorkspaceRegistry::new(),
			config_dir,
		}
	}
}
