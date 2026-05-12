//! Window-management Tauri commands.
//!
//! Process-per-workspace: each `moon-ide` process owns exactly
//! one workspace and exactly one OS window labelled `main`.
//! "Open another workspace" doesn't spawn a webview — it spawns
//! a sibling `moon-ide --workspace <slug>` process.
//!
//! The cross-process coordination lives in [`crate::focus_socket`]:
//! before spawning, the launcher tries to connect to that
//! workspace's instance socket; if a sibling owns it, send a
//! "focus" message instead and skip the spawn.
//!
//! Dev-mode caveat: `bun run dev` supervises a vite dev server
//! that's tied to the lifetime of this Rust binary, so spawning
//! a child would yield a window with no frontend to load. In
//! debug builds [`window_open`] therefore refuses to spawn and
//! returns a clear error explaining how to test multi-workspace
//! flows (use a release build).

use moon_core::app_state as core_app_state;
use moon_protocol::workspace::{validate_workspace_id, WorkspaceMeta};
use moon_protocol::MoonError;
use tauri::State;

use crate::focus_socket;
use crate::state::AppState;

/// Open a window for `workspace_id`. If a sibling process
/// already owns the workspace's instance socket we send it a
/// focus message and return; otherwise we spawn a fresh
/// `moon-ide --workspace <slug>` child and let it bind the
/// socket itself.
///
/// Returns immediately — the new process's startup is async
/// from this call's POV. Errors mean the spawn / focus dispatch
/// itself failed (validation, missing executable, IPC error);
/// the new process's own startup errors surface in its window,
/// not in this caller's result.
///
/// Bumps `last_active_at` so the picker's "recent" sort and the
/// launcher's restore pick reflect the user's last touch.
#[tauri::command]
pub async fn window_open(state: State<'_, AppState>, workspace_id: String) -> Result<(), MoonError> {
	validate_workspace_id(&workspace_id)?;

	if Some(workspace_id.as_str()) == state.workspace_id() {
		// Same window already showing this workspace — nothing
		// to do. The frontend's picker shouldn't usually call
		// us in this shape, but tolerate it idempotently.
		return Ok(());
	}

	if focus_socket::workspace_is_live(&state.workspaces_dir, &workspace_id).await {
		focus_socket::send_focus(&state.workspaces_dir, &workspace_id)
			.await
			.map_err(|err| MoonError::Internal(format!("failed to focus existing window: {err}")))?;
		bump_last_active(&state, &workspace_id).await;
		return Ok(());
	}

	// Bump the catalog timestamp regardless of whether we
	// can actually spawn the child — in dev mode the user
	// is told to restart `bun run dev`, and the next launch
	// should restore the workspace they just asked for.
	bump_last_active(&state, &workspace_id).await;

	if cfg!(debug_assertions) {
		return Err(MoonError::invalid(format!(
			"Switching workspaces from `bun run dev` is not supported (vite is owned by the parent dev process). Quit and run `bun run dev` again to open `{workspace_id}` — it's now the most-recently-used workspace and will be restored automatically."
		)));
	}

	let exe = std::env::current_exe()
		.map_err(|err| MoonError::Internal(format!("failed to resolve current executable: {err}")))?;
	std::process::Command::new(&exe)
		.arg("--workspace")
		.arg(&workspace_id)
		.spawn()
		.map_err(|err| MoonError::Internal(format!("failed to spawn moon-ide for `{workspace_id}`: {err}")))?;

	Ok(())
}

/// Close the calling window. With one window per process, this
/// just exits the process — the OS reaps the rest. Tauri's own
/// drop sequence handles the window-state plugin's persistence
/// and the runtime shutdown hook (see `crate::shutdown`).
#[tauri::command]
pub async fn window_close(app: tauri::AppHandle) -> Result<(), MoonError> {
	app.exit(0);
	Ok(())
}

/// Update the calling window's OS title. The frontend rebuilds
/// the title from workspace name + active folder + branch on
/// every relevant change and routes through here so each window
/// only ever rewrites its own title.
#[tauri::command]
pub async fn window_set_title(window: tauri::Window, title: String) -> Result<(), MoonError> {
	window
		.set_title(&title)
		.map_err(|err| MoonError::Internal(format!("failed to set window title: {err}")))?;
	Ok(())
}

/// Bump `last_active_at` for `workspace_id` in the on-disk
/// `state.json`. Best-effort: a write failure is logged but
/// otherwise swallowed — the picker just shows a slightly stale
/// "recent" sort until the next successful save.
pub async fn bump_last_active(state: &AppState, workspace_id: &str) {
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);
	let id = workspace_id.to_string();
	let id_for_log = id.clone();
	let result = core_app_state::mutate(&state.config_dir, move |s| {
		for meta in s.workspaces.iter_mut() {
			if meta.id == id {
				meta.last_active_at = now;
				return;
			}
		}
		// The workspace was bumped before the catalog knew
		// about it. Repair rather than silently drop.
		s.workspaces.push(WorkspaceMeta {
			id: id.clone(),
			name: id,
			last_active_at: now,
			color: None,
		});
	})
	.await;
	if let Err(err) = result {
		tracing::warn!(error = %err, workspace_id = %id_for_log, "failed to persist last_active_at bump");
	}
}
