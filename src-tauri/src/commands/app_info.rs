//! `app_info` — first IPC the frontend issues on hydrate to
//! learn what mode this process is in (workspace vs preboot)
//! and which workspace, if any, it owns.
//!
//! With process-per-workspace there's no room for ambiguity:
//! the answer is fixed at startup from CLI args + catalog
//! state, and never changes for the process's lifetime.

use moon_protocol::app_info::{AppInfo, AppInfoMode};
use moon_protocol::MoonError;
use tauri::State;

use crate::state::{AppMode, AppState};

#[tauri::command]
pub async fn app_info(state: State<'_, AppState>) -> Result<AppInfo, MoonError> {
	match &state.mode {
		AppMode::Preboot => Ok(AppInfo {
			mode: AppInfoMode::Preboot,
			workspace_id: None,
			workspace_name: None,
		}),
		AppMode::Workspace { id } => {
			let catalog = moon_core::app_state::load(&state.config_dir).await?;
			let name = catalog.workspaces.iter().find(|m| &m.id == id).map(|m| m.name.clone());
			Ok(AppInfo {
				mode: AppInfoMode::Workspace,
				workspace_id: Some(id.clone()),
				workspace_name: name,
			})
		}
	}
}
