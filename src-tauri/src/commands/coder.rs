//! Tauri commands wrapping `moon-coder`.
//!
//! Phase 6.0 surface: device-flow sign-in, status probe, sign-out,
//! one-shot `send`, mid-turn `abort`. Loop events stream out on the
//! `coder:event` Tauri channel. See
//! `specs/test-plans/0039-coder-skeleton.md`.

use moon_coder::{CoderHandle, CoderStatus, DeviceCode, HfIdentity, SessionSummary};
use moon_core::app_state as app_state_store;
use moon_protocol::coder_models::{
	CoderModelSettings, CoderProviderConfig, ProviderModelSummary, ProviderProbeResult, RouterModel,
};
use moon_protocol::MoonError;
use serde::Deserialize;
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

/// Suggest a kebab-cased branch name based on the user's draft
/// commit message and the active folder's `git diff HEAD --stat`
/// summary. Used by the SCM panel's "Commit to new branch…" form
/// to populate the branch input. Falls back to an error string
/// the panel surfaces verbatim when the model is unreachable —
/// the user can still type a name manually.
#[tauri::command]
pub async fn coder_suggest_branch_name(state: State<'_, AppState>, message: String) -> Result<String, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let diff = entry.host.git_diff_summary().await.unwrap_or_default();
	state
		.coder
		.suggest_branch_name(&message, &diff)
		.await
		.map_err(MoonError::from)
}

/// Suggest a one-line commit subject from the active folder's
/// `git diff HEAD` patch (capped to ~64 KB upstream) and whatever
/// the user has already typed in the composer. Used by the SCM
/// panel's sparkle button inset on the commit textarea. Errors
/// surface verbatim through the panel as a flash toast — the user
/// can still type the message manually.
#[tauri::command]
pub async fn coder_suggest_commit_message(state: State<'_, AppState>, message: String) -> Result<String, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let diff = entry.host.git_diff_patch().await.unwrap_or_default();
	state
		.coder
		.suggest_commit_message(&message, &diff)
		.await
		.map_err(MoonError::from)
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
		let id_owned = id.clone();
		app_state_store::mutate(&state.config_dir, move |s| {
			if s.coder.last_session_by_folder.get(&folder).map(|v| v.as_str()) == Some(id_owned.as_str()) {
				s.coder.last_session_by_folder.remove(&folder);
			}
		})
		.await?;
	}
	Ok(())
}

/// Persist the last-opened session id for the given workspace
/// folder so a relaunch lands the user back in the right
/// transcript per project. Best-effort: a write failure logs but
/// doesn't fail the open call. `None` clears the entry (e.g. the
/// user just deleted the session).
async fn persist_last_session(config_dir: &camino::Utf8Path, folder: &str, id: Option<String>) {
	let folder_owned = folder.to_string();
	let result = app_state_store::mutate(config_dir, move |s| {
		let existing = s.coder.last_session_by_folder.get(&folder_owned).cloned();
		match (existing, id) {
			(Some(prev), Some(new)) if prev == new => {}
			(None, None) => {}
			(_, Some(new)) => {
				s.coder.last_session_by_folder.insert(folder_owned, new);
			}
			(_, None) => {
				s.coder.last_session_by_folder.remove(&folder_owned);
			}
		}
	})
	.await;
	if let Err(err) = result {
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

/// Snapshot of the user's current model picks. The popover reads
/// this on open so it doesn't fall out of sync if a different
/// surface (or a future hotkey) wrote to AppState. Returns the
/// full picker state in one shot: HF picks, the active provider
/// id, and the user-added providers list (each carrying its own
/// `has_api_key` flag, which is sourced from the keyring rather
/// than echoed from disk).
#[tauri::command]
pub async fn coder_get_model_settings(state: State<'_, AppState>) -> Result<CoderModelSettings, MoonError> {
	let models = state.coder.current_models().await;
	Ok(CoderModelSettings {
		standard_model: models.standard,
		cheap_model: models.cheap,
		bill_to: models.bill_to.unwrap_or_default(),
		active_provider: models.active_provider,
		providers: models.providers,
	})
}

/// Persist + apply the new picker settings. Writes through
/// AppState (so a relaunch sees the same picks) and pokes the
/// coder so the very next round-trip uses the new model +
/// bill_to + provider list. Slugs are already in their final
/// `model:provider` form because the picker concatenates on
/// click; the runner doesn't do any post-processing.
///
/// API keys do **not** travel through this command. The picker
/// uses `coder_set_provider_api_key` / `coder_clear_provider_api_key`
/// (per-id, keyring-backed) so secrets never round-trip through
/// AppState or the IPC layer's logging.
#[tauri::command]
pub async fn coder_set_model_settings(
	state: State<'_, AppState>,
	settings: CoderModelSettings,
) -> Result<(), MoonError> {
	let bill_to = if settings.bill_to.is_empty() {
		None
	} else {
		Some(settings.bill_to.clone())
	};
	let providers_for_runner = settings.providers.clone();
	let active_for_runner = settings.active_provider.clone();
	state
		.coder
		.set_user_picks(settings.standard_model.clone(), settings.cheap_model.clone(), bill_to)
		.await;
	state.coder.set_providers(providers_for_runner, active_for_runner).await;

	app_state_store::mutate(&state.config_dir, move |s| {
		s.coder.standard_model = settings.standard_model;
		s.coder.cheap_model = settings.cheap_model;
		s.coder.bill_to = settings.bill_to;
		s.coder.active_provider = settings.active_provider;
		// Strip `has_api_key` before persisting — it's keyring-derived,
		// not state, and surviving it on disk would let a hand-edited
		// `state.json` claim a key is configured when the keyring is
		// empty.
		s.coder.providers = settings
			.providers
			.into_iter()
			.map(|mut p| {
				p.has_api_key = false;
				p
			})
			.collect();
	})
	.await?;
	Ok(())
}

/// Fetch the router's `/v1/models` catalog. **HF-only** — when a
/// user provider is active, the command errors and the picker is
/// expected to call `coder_list_provider_models` instead. One
/// round trip per call; the frontend caches the result for the
/// lifetime of the popover so flipping filters doesn't re-hit
/// the network.
#[tauri::command]
pub async fn coder_list_models(state: State<'_, AppState>) -> Result<Vec<RouterModel>, MoonError> {
	state.coder.list_models().await.map_err(MoonError::from)
}

/// Allocate a fresh opaque provider id. The picker's `Add provider`
/// modal calls this before any keyring / config write so the
/// keyring slot is addressable from the moment the user types a
/// key, even if they cancel out of the modal before saving.
/// Idempotent in the sense that a leaked id without any matching
/// state is harmless — nothing reads the keyring slot until the
/// provider config lands in `AppState`.
#[tauri::command]
pub async fn coder_new_provider_id(state: State<'_, AppState>) -> Result<String, MoonError> {
	Ok(state.coder.new_provider_id())
}

/// Probe a `(base_url, api_key)` pair before the picker commits.
/// Surfaces the upstream HTTP failure verbatim on error so the
/// user can see "401 Unauthorized" / "couldn't reach host" / etc.
/// `api_key` empty = probe without an `Authorization` header
/// (local llama.cpp / Ollama).
#[derive(Debug, Deserialize)]
pub struct ProbeProviderArgs {
	pub base_url: String,
	#[serde(default)]
	pub api_key: String,
}

#[tauri::command]
pub async fn coder_probe_provider(
	state: State<'_, AppState>,
	args: ProbeProviderArgs,
) -> Result<ProviderProbeResult, MoonError> {
	let key = if args.api_key.is_empty() {
		None
	} else {
		Some(args.api_key.as_str())
	};
	state
		.coder
		.probe_provider(&args.base_url, key)
		.await
		.map_err(MoonError::from)
}

/// Persist a per-provider API key in the OS keyring. Empty values
/// are rejected — same trap the Tavily key avoids: a silently-empty
/// entry would set `has_api_key: true` while every downstream
/// call 401s. After this returns Ok, the next request resolving
/// to this provider picks up the new key without rewiring.
#[derive(Debug, Deserialize)]
pub struct SetProviderApiKeyArgs {
	pub id: String,
	pub key: String,
}

#[tauri::command]
pub async fn coder_set_provider_api_key(
	state: State<'_, AppState>,
	args: SetProviderApiKeyArgs,
) -> Result<(), MoonError> {
	state
		.coder
		.set_provider_api_key(&args.id, &args.key)
		.map_err(MoonError::from)
}

/// Drop the keyring entry for a provider. Idempotent — fine to
/// call on a provider that never had a key (the local-vLLM case
/// where the user is just removing a stale entry).
#[tauri::command]
pub async fn coder_clear_provider_api_key(state: State<'_, AppState>, id: String) -> Result<(), MoonError> {
	state.coder.clear_provider_api_key(&id).map_err(MoonError::from)
}

/// Flat `/v1/models` catalog for a user-added provider. The
/// picker uses this instead of `coder_list_models` when a user
/// provider is active. Returns the OpenAI-compat `{id, owned_by}`
/// rows; the picker renders them as a flat searchable list
/// (no pricing / throughput — those aren't uniform across
/// OpenAI-compat servers).
///
/// Errors on network / 4xx / 5xx propagate verbatim. A 404 means
/// the server doesn't expose the catalog endpoint; the picker
/// shows "Catalog unavailable" and the user can still type a
/// model slug directly into the field.
#[tauri::command]
pub async fn coder_list_provider_models(
	state: State<'_, AppState>,
	id: String,
) -> Result<Vec<ProviderModelSummary>, MoonError> {
	state.coder.list_provider_models(&id).await.map_err(MoonError::from)
}

/// Side-channel persist of a brand-new provider entry. Used by
/// the `Add provider` modal so the new provider lands in
/// `AppState` and the runtime view *before* the user has had a
/// chance to flip it to active. The picker can call
/// `coder_set_model_settings` later with the full state, but
/// this commit-per-action shape keeps the "I clicked Save in
/// the Add modal, but cancelled the outer modal" path from
/// losing the provider.
///
/// The keyring entry, if any, was already written via
/// `coder_set_provider_api_key` against `config.id` before this
/// call — we don't take the key as a parameter here.
#[tauri::command]
pub async fn coder_save_provider(state: State<'_, AppState>, config: CoderProviderConfig) -> Result<(), MoonError> {
	let (providers, active) = app_state_store::mutate(&state.config_dir, move |s| {
		let mut cfg = config.clone();
		// `has_api_key` is keyring-derived; never trust the caller.
		cfg.has_api_key = false;
		if let Some(existing) = s.coder.providers.iter_mut().find(|p| p.id == cfg.id) {
			*existing = cfg;
		} else {
			s.coder.providers.push(cfg);
		}
		(s.coder.providers.clone(), s.coder.active_provider.clone())
	})
	.await?;
	state.coder.set_providers(providers, active).await;
	Ok(())
}

/// Drop a provider entry from `AppState` and its keyring slot.
/// If the deleted provider was active, the runner falls back to
/// HF (the only always-available route). Idempotent: deleting an
/// unknown id is a no-op.
#[tauri::command]
pub async fn coder_delete_provider(state: State<'_, AppState>, id: String) -> Result<(), MoonError> {
	// Drop the keyring entry first; even if the AppState write
	// fails, we don't want the credential to outlive the config.
	let _ = state.coder.clear_provider_api_key(&id);
	let id_for_state = id.clone();
	let (providers, active) = app_state_store::mutate(&state.config_dir, move |s| {
		s.coder.providers.retain(|p| p.id != id_for_state);
		if s.coder.active_provider.as_deref() == Some(id_for_state.as_str()) {
			s.coder.active_provider = None;
		}
		(s.coder.providers.clone(), s.coder.active_provider.clone())
	})
	.await?;
	state.coder.set_providers(providers, active).await;
	Ok(())
}

/// `true` iff a Tavily API key is stored in the OS keyring. The
/// model-settings popover reads this on mount so it can render the
/// "set a key" / "key configured" state correctly. Cheap sync read
/// of the in-memory cache — no keyring round-trip.
#[tauri::command]
pub async fn coder_web_search_configured(state: State<'_, AppState>) -> Result<bool, MoonError> {
	Ok(state.coder.web_search_configured())
}

/// Persist a new Tavily API key. Trimmed at the runner; empty
/// values are rejected. After this returns Ok, the next agent
/// turn will see `web_search` in its tool list.
#[tauri::command]
pub async fn coder_set_web_search_key(state: State<'_, AppState>, key: String) -> Result<(), MoonError> {
	state.coder.set_web_search_key(&key).map_err(MoonError::from)
}

/// Drop the Tavily key from the keyring. Idempotent.
#[tauri::command]
pub async fn coder_clear_web_search_key(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.coder.clear_web_search_key().map_err(MoonError::from)
}
