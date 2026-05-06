//! Tauri commands wrapping `moon-coder`.
//!
//! Phase 6.0 surface: device-flow sign-in, status probe, sign-out,
//! one-shot `send`, mid-turn `abort`. Loop events stream out on the
//! `coder:event` Tauri channel. See
//! `specs/test-plans/0039-coder-skeleton.md`.

use moon_coder::{CoderHandle, CoderStatus, DeviceCode, HfIdentity, SessionSummary};
use moon_core::app_state as app_state_store;
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Channel name the loop's events are emitted on. The frontend
/// listens via `getCurrent().listen('coder:event', ...)`. Mirrored in
/// `src/lib/coder.svelte.ts`.
pub const CODER_EVENT_CHANNEL: &str = "coder:event";

/// Spawn the long-running task that re-broadcasts the coder's
/// in-process broadcast channel onto Tauri's event bus. Called once
/// at app startup; the task lives for the entire process lifetime.
pub fn spawn_event_pump(app: AppHandle, coder: CoderHandle) {
	let mut rx = coder.subscribe();
	tauri::async_runtime::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(event) => {
					if let Err(err) = app.emit(CODER_EVENT_CHANNEL, &event) {
						tracing::warn!(error = %err, "failed to emit coder event");
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					// Rare, since we sized the channel generously.
					// Logged so a flood is visible without crashing the
					// pump — the frontend resyncs from `coder_status`
					// on its next mount.
					tracing::warn!(missed = n, "coder event pump lagged");
				}
				Err(tokio::sync::broadcast::error::RecvError::Closed) => {
					tracing::info!("coder event channel closed; pump exiting");
					break;
				}
			}
		}
	});
}

/// Snapshot the coder's auth + busy state. Polled by the panel on
/// mount so reopens land in the right shape.
#[tauri::command]
pub async fn coder_status(state: State<'_, AppState>) -> Result<CoderStatus, MoonError> {
	state.coder.status().await.map_err(MoonError::from)
}

/// Fetch the cached "Bound folders" description for `folder`
/// (absolute path matching `WorkspaceFolder.path`). Returns
/// `None` when the cache is cold or stale — the runner kicks off
/// regeneration on its next turn, and a `folder_summary_ready`
/// event will fire when it finishes. Used by the project bar
/// tooltip and sub-agent picker preview.
#[tauri::command]
pub async fn coder_folder_summary(state: State<'_, AppState>, folder: String) -> Result<Option<String>, MoonError> {
	Ok(state.coder.folder_summary(&folder).await)
}

/// Kick off the HF device flow. Returns the user/device code pair
/// immediately. The frontend opens `verification_uri_complete` in
/// the system browser then calls [`coder_poll_device_code`] to wait
/// for the consent screen.
#[tauri::command]
pub async fn coder_start_device_flow(state: State<'_, AppState>) -> Result<DeviceCode, MoonError> {
	state.coder.start_device_flow().await.map_err(MoonError::from)
}

/// Poll the token endpoint until the user approves / denies. Returns
/// the freshly-fetched [`HfIdentity`] on success. The future blocks
/// until completion; the frontend awaits with the modal still open.
#[tauri::command]
pub async fn coder_poll_device_code(state: State<'_, AppState>, code: DeviceCode) -> Result<HfIdentity, MoonError> {
	state.coder.poll_device_code(code).await.map_err(MoonError::from)
}

/// Drop the keyring entry + the in-memory session. Idempotent.
#[tauri::command]
pub async fn coder_sign_out(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.coder.sign_out().await.map_err(MoonError::from)
}

/// Send one user message and start a turn. Non-blocking — the future
/// resolves once the turn has been spawned, then events stream over
/// the `coder:event` channel. Errors here mean the turn never
/// started (no auth, already-running turn, etc.).
#[tauri::command]
pub async fn coder_send(state: State<'_, AppState>, text: String) -> Result<(), MoonError> {
	state.coder.send(text).await.map_err(MoonError::from)
}

/// Cancel the **active folder's** running turn, if any.
/// Background turns running in other folders are left alone — the
/// user has to switch to them and stop manually if they want
/// (per the multi-session "agents keep running per project"
/// contract). Async because resolving the active folder + its
/// `FolderSession` map entry needs `await`.
#[tauri::command]
pub async fn coder_abort(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.coder.abort().await;
	Ok(())
}

/// List persisted sessions for the active workspace folder. Empty
/// when the folder has none — including when no folder is active.
#[tauri::command]
pub async fn coder_list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionSummary>, MoonError> {
	state.coder.list_sessions().await.map_err(MoonError::from)
}

/// Snapshot of the active in-memory session, if any. `None` for a
/// blank session — the panel uses this to decide between "show
/// the sessions list" and "show this session's transcript" on
/// mount.
#[tauri::command]
pub async fn coder_active_session(state: State<'_, AppState>) -> Result<Option<SessionSummary>, MoonError> {
	Ok(state.coder.active_session().await)
}

/// Drop the in-memory session and start a blank one. Doesn't
/// touch disk — empty sessions never write a file.
#[tauri::command]
pub async fn coder_new_session(state: State<'_, AppState>) -> Result<SessionSummary, MoonError> {
	state.coder.new_session().await.map_err(MoonError::from)
}

/// Replace the in-memory session with the persisted one
/// identified by `id`. Backend emits `session_loaded` + per-record
/// replay events on the `coder:event` channel; the frontend reacts
/// to those rather than getting the records back inline.
#[tauri::command]
pub async fn coder_open_session(state: State<'_, AppState>, id: String) -> Result<SessionSummary, MoonError> {
	let summary = state.coder.open_session(id).await.map_err(MoonError::from)?;
	let folder = active_folder_path(&state).await;
	if let Some(folder) = folder {
		persist_last_session(&state.config_dir, &folder, Some(summary.id.clone())).await;
	}
	Ok(summary)
}

/// Resolve the on-disk JSONL path of a session under the active
/// workspace folder. Frontend uses this to open the raw trace in
/// the editor via the host-direct file path (same mechanism as
/// `Ctrl+O` for files outside the workspace). Works for sub-agent
/// ids too — they share the parent folder's slug.
#[tauri::command]
pub async fn coder_session_jsonl_path(state: State<'_, AppState>, id: String) -> Result<String, MoonError> {
	let path = state.coder.session_jsonl_path(id).await.map_err(MoonError::from)?;
	Ok(path.into_string())
}

/// Delete a persisted session for the active workspace folder.
/// Idempotent. Emits `session_list_changed` afterwards.
#[tauri::command]
pub async fn coder_delete_session(state: State<'_, AppState>, id: String) -> Result<(), MoonError> {
	state.coder.delete_session(id.clone()).await.map_err(MoonError::from)?;
	let folder = active_folder_path(&state).await;
	if let Some(folder) = folder {
		let mut current = app_state_store::load(&state.config_dir).await?;
		if current.coder.last_session_by_folder.get(&folder).map(|v| v.as_str()) == Some(id.as_str()) {
			current.coder.last_session_by_folder.remove(&folder);
			app_state_store::save(&state.config_dir, &current).await?;
		}
	}
	Ok(())
}

/// Persist the last-opened session id for the given workspace
/// folder so a relaunch lands the user back in the right
/// transcript per project. Best-effort: a write failure logs but
/// doesn't fail the open call. `None` clears the entry (e.g. the
/// user just deleted the session).
async fn persist_last_session(config_dir: &camino::Utf8Path, folder: &str, id: Option<String>) {
	let current = match app_state_store::load(config_dir).await {
		Ok(state) => state,
		Err(err) => {
			tracing::warn!(error = %err, "could not load app state to persist last session id");
			return;
		}
	};
	let mut next = current;
	let existing = next.coder.last_session_by_folder.get(folder).cloned();
	match (existing, id) {
		(Some(prev), Some(new)) if prev == new => return,
		(None, None) => return,
		(_, Some(new)) => {
			next.coder.last_session_by_folder.insert(folder.to_string(), new);
		}
		(_, None) => {
			next.coder.last_session_by_folder.remove(folder);
		}
	}
	if let Err(err) = app_state_store::save(config_dir, &next).await {
		tracing::warn!(error = %err, "could not persist last session id");
	}
}

/// Active workspace folder's absolute path, or `None` when the
/// workspace is empty / no folder is bound. Used by the
/// per-folder persistence helpers.
async fn active_folder_path(state: &AppState) -> Option<String> {
	state
		.workspaces
		.active_folder()
		.await
		.map(|entry| entry.folder.path.clone())
}
