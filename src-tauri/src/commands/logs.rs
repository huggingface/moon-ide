//! Diagnostic logs IPC.
//!
//! Frontend → backend surface for the bottom-panel "Logs" view:
//!
//! - [`logs_snapshot`] replays the ring buffer for one source so a
//!   freshly-opened tab gets back-fill instead of waiting for the
//!   next live entry.
//! - [`logs_sources`] returns every source the backend has emitted
//!   into; the popover groups by this list.
//! - [`logs_clear`] empties one source's ring (the panel toolbar's
//!   Clear button).
//! - [`logs_emit`] lets the frontend push its own entries through
//!   the same sink, so client-side breadcrumbs (Ctrl+Space fired,
//!   format-on-save ran, …) appear in the same tab as backend ones
//!   without a separate buffer.
//!
//! Live entries fan out via the `logs:entry` Tauri event from
//! [`spawn_event_pump`], which the frontend listens for on startup.

use std::sync::Arc;

use moon_core::LogSink;
use moon_protocol::logs::{LogEntry, LogLevel};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Per-entry event name. Payload is [`LogEntry`].
pub const LOGS_ENTRY_EVENT: &str = "logs:entry";

#[tauri::command]
pub async fn logs_snapshot(state: State<'_, AppState>, source: String) -> Result<Vec<LogEntry>, MoonError> {
	Ok(state.logs.snapshot(&source))
}

#[tauri::command]
pub async fn logs_sources(state: State<'_, AppState>) -> Result<Vec<String>, MoonError> {
	Ok(state.logs.sources())
}

#[tauri::command]
pub async fn logs_clear(state: State<'_, AppState>, source: String) -> Result<(), MoonError> {
	state.logs.clear(&source);
	Ok(())
}

#[tauri::command]
pub async fn logs_emit(
	state: State<'_, AppState>,
	source: String,
	level: LogLevel,
	message: String,
) -> Result<(), MoonError> {
	state.logs.emit(&source, level, message);
	Ok(())
}

/// Subscribe to the sink's broadcast channel and re-emit each entry
/// on the `logs:entry` Tauri event. Spawned once at startup.
///
/// Posture matches the LSP event pump: a `Lagged` receiver logs the
/// drop count and keeps going (back-fill comes through
/// [`logs_snapshot`] anyway). `Closed` only happens during process
/// teardown.
pub fn spawn_event_pump(app: AppHandle, logs: Arc<LogSink>) {
	let mut rx = logs.subscribe();
	tauri::async_runtime::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(entry) => {
					let _ = app.emit(LOGS_ENTRY_EVENT, &entry);
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					tracing::warn!(skipped = n, "logs event pump lagged");
				}
				Err(tokio::sync::broadcast::error::RecvError::Closed) => {
					break;
				}
			}
		}
	});
}
