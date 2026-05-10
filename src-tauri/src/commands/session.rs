//! Tauri commands wrapping [`moon_core::session`].
//!
//! One `session.json` per workspace, holding the per-workspace
//! UI state (folders bound, open tabs, splits, focused folder,
//! SCM filters). Loaded on hydrate, saved on every persist
//! tick.
//!
//! Process-per-workspace: each process owns one workspace and
//! reads/writes its own `session.json`. No `workspace_id`
//! parameter: the file path is derived from
//! `state.workspace_id()`.

use moon_core::session as core_session;
use moon_protocol::session::WorkspaceSession;
use moon_protocol::MoonError;
use tauri::State;

use crate::commands::window::bump_last_active;
use crate::state::AppState;

fn require_workspace_id(state: &AppState) -> Result<&str, MoonError> {
	state
		.workspace_id()
		.ok_or_else(|| MoonError::invalid("session: no workspace bound to this process"))
}

#[tauri::command]
pub async fn session_load(state: State<'_, AppState>) -> Result<WorkspaceSession, MoonError> {
	let id = require_workspace_id(&state)?;
	core_session::load(&state.workspaces_dir, id).await
}

#[tauri::command]
pub async fn session_save(state: State<'_, AppState>, session: WorkspaceSession) -> Result<(), MoonError> {
	let id = require_workspace_id(&state)?.to_owned();
	core_session::save(&state.workspaces_dir, &id, &session).await?;
	// Every persist tick is meaningful activity for the
	// workspace — bumping `last_active_at` here means the
	// "most-recently-active" sort tracks real usage rather than
	// just process-launch events.
	bump_last_active(&state, &id).await;
	Ok(())
}
