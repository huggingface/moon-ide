//! LSP Tauri commands.
//!
//! Thin surface: open / update / close (fire-and-forget-ish
//! notifications) + hover / completion (requests). Diagnostics stream
//! out on the `lsp:diagnostics` event; server status transitions
//! (starting, running, unavailable, crashed, stopped) come through
//! `lsp:status`. The broker lives behind [`AppState::lsp`] and is
//! spawned lazily on the first open call — we don't pay the TS server
//! startup cost until the user actually opens a TS/JS file.
//!
//! This command module is intentionally dumb: every error path goes
//! through `MoonError` and every parameter is forwarded to the broker
//! unchanged. "No policy" is the policy. Path validation, language-id
//! mapping, and graceful `NotAvailable` fallback all live in
//! `moon_core::lsp` so `moon-agent` (remote mode) and the Tauri shell
//! share the same behaviour.

use camino::Utf8PathBuf;
use moon_core::lsp::{LspBroker, LspServerEvent};
use moon_protocol::lsp::{LspCompletionList, LspHover, LspLocation, LspPosition};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};
use tokio::task::JoinHandle;

use crate::state::AppState;

/// `textDocument/publishDiagnostics` fan-out. Payload is
/// `LspDiagnosticsEvent` (path + full diagnostic list). Full replacement
/// semantics: the server gives us the new truth for the whole file on
/// every publish, so the UI overwrites rather than merges.
pub const LSP_DIAGNOSTICS_EVENT: &str = "lsp:diagnostics";

/// Per-language server status transition. Payload is `LspStatusEvent`.
/// The UI keeps the latest per language and paints a status-bar pill
/// when anything but `Running` is active.
pub const LSP_STATUS_EVENT: &str = "lsp:status";

#[tauri::command]
pub async fn lsp_open(
	state: State<'_, AppState>,
	app: AppHandle,
	path: String,
	language_id: String,
	text: String,
) -> Result<(), MoonError> {
	let broker = ensure_broker(&state, &app).await?;
	broker
		.open(&path, text, &language_id)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

#[tauri::command]
pub async fn lsp_update(
	state: State<'_, AppState>,
	app: AppHandle,
	path: String,
	language_id: String,
	text: String,
) -> Result<(), MoonError> {
	let broker = ensure_broker(&state, &app).await?;
	broker
		.update(&path, text, &language_id)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

#[tauri::command]
pub async fn lsp_close(state: State<'_, AppState>, path: String, language_id: String) -> Result<(), MoonError> {
	// Don't spawn a broker just to close a file — if we never had
	// one, there's nothing to inform.
	let broker = {
		let guard = state.lsp.lock().await;
		match guard.as_ref() {
			Some(b) => b.broker.clone(),
			None => return Ok(()),
		}
	};
	broker
		.close(&path, &language_id)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

#[tauri::command]
pub async fn lsp_hover(
	state: State<'_, AppState>,
	app: AppHandle,
	path: String,
	language_id: String,
	position: LspPosition,
) -> Result<Option<LspHover>, MoonError> {
	let broker = ensure_broker(&state, &app).await?;
	broker
		.hover(&path, &language_id, position)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

#[tauri::command]
pub async fn lsp_completion(
	state: State<'_, AppState>,
	app: AppHandle,
	path: String,
	language_id: String,
	position: LspPosition,
) -> Result<LspCompletionList, MoonError> {
	let broker = ensure_broker(&state, &app).await?;
	broker
		.completion(&path, &language_id, position)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// Resolve `textDocument/definition` for the symbol at `position`.
/// Returns `Ok(None)` when the server doesn't know (e.g. the cursor
/// is on whitespace, a keyword with no jump, or a literal). The UI
/// treats that as "no link underline" rather than an error.
#[tauri::command]
pub async fn lsp_definition(
	state: State<'_, AppState>,
	app: AppHandle,
	path: String,
	language_id: String,
	position: LspPosition,
) -> Result<Option<LspLocation>, MoonError> {
	let broker = ensure_broker(&state, &app).await?;
	broker
		.definition(&path, &language_id, position)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// Hold for the currently-active broker plus the event-pump
/// supervisor. Dropping the handle drops the broker's `Arc`; the
/// supervisor task exits when its `broadcast::Receiver` returns
/// `Closed` (all senders dropped), which happens on the broker's
/// final `Arc` drop.
pub struct LspHandle {
	pub broker: std::sync::Arc<LspBroker>,
	pub root: Utf8PathBuf,
	_pump: JoinHandle<()>,
}

/// Get or create the broker for the current active folder. Spawned
/// lazily so no LSP process starts until the user actually opens a
/// file. If the active folder has changed since last time, the old
/// broker is shut down and a new one is built.
async fn ensure_broker(state: &AppState, app: &AppHandle) -> Result<std::sync::Arc<LspBroker>, MoonError> {
	let snap = state.workspaces.snapshot().await;
	let active = snap
		.active_folder
		.ok_or_else(|| MoonError::invalid("lsp: no active folder; open a workspace before using LSP"))?;
	let root = Utf8PathBuf::from(active);

	let mut guard = state.lsp.lock().await;
	if let Some(existing) = guard.as_ref() {
		if existing.root == root {
			return Ok(existing.broker.clone());
		}
		// Active folder changed. Tear down and rebuild.
		let old = guard.take().expect("guard.take after is_some");
		old.broker.shutdown_all().await;
		// `old` dropped here: its `_pump` receiver will return
		// `Closed` once the broker's `broadcast::Sender` side
		// drops below, and the pump exits.
	}

	let broker = LspBroker::new(root.clone());
	let mut rx = broker.subscribe();
	let app_clone = app.clone();
	let pump = tokio::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(LspServerEvent::Diagnostics(ev)) => {
					let _ = app_clone.emit(LSP_DIAGNOSTICS_EVENT, &ev);
				}
				Ok(LspServerEvent::StatusChanged(ev)) => {
					let _ = app_clone.emit(LSP_STATUS_EVENT, &ev);
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					// Should be rare (256-deep channel, one UI
					// consumer). Log and keep going rather than
					// exit — the next publish replaces whatever
					// was missed.
					tracing::warn!(skipped = n, "lsp event pump lagged");
				}
				Err(tokio::sync::broadcast::error::RecvError::Closed) => {
					break;
				}
			}
		}
	});

	*guard = Some(LspHandle {
		broker: broker.clone(),
		root,
		_pump: pump,
	});
	Ok(broker)
}
