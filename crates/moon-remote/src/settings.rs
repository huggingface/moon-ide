//! Shared model/provider settings surface for the companion RPC
//! (`coder_get_model_settings` / `coder_set_model_settings`).
//!
//! Lives here (not in `src-tauri`) so the desktop's Tauri commands
//! and the headless `moon-remote serve` dispatcher execute the exact
//! same body: runner poke, per-workspace provider lock in
//! `session.json`, global defaults in `state.json`. The desktop
//! wraps these in `MoonError`; the bridge dispatcher stringifies.

use camino::Utf8PathBuf;
use moon_coder::CoderHandle;
use moon_core::app_state as app_state_store;
use moon_core::session as core_session;
use moon_protocol::coder_models::{CoderModelSettings, CoderProviderLock};
use moon_protocol::MoonError;

/// Where this process persists settings: the global `state.json`
/// (`config_dir`) and the per-workspace `session.json`
/// (`workspaces_dir` + `workspace_id`). `workspace_id` is `None` in
/// the desktop's preboot mode — provider-lock reads/writes no-op.
#[derive(Debug, Clone)]
pub struct SettingsContext {
	pub config_dir: Utf8PathBuf,
	pub workspaces_dir: Utf8PathBuf,
	pub workspace_id: Option<String>,
}

/// Read the per-workspace provider lock from `session.json`. `None`
/// for processes without a bound workspace and on any I/O / parse
/// failure (logged).
pub async fn workspace_provider_lock(ctx: &SettingsContext) -> Option<CoderProviderLock> {
	let id = ctx.workspace_id.as_deref()?;
	match core_session::load(&ctx.workspaces_dir, id).await {
		Ok(session) => session.coder_provider_lock,
		Err(err) => {
			tracing::warn!(error = %err, "could not load session for provider-lock read");
			None
		}
	}
}

/// Apply `lock` to this workspace's `session.json`. `Some(_)`
/// replaces the existing lock; `None` clears it. No-ops without a
/// bound workspace. Load-then-save round-trip preserves every other
/// session field.
pub async fn write_workspace_provider_lock(
	ctx: &SettingsContext,
	lock: Option<CoderProviderLock>,
) -> Result<(), MoonError> {
	let Some(id) = ctx.workspace_id.as_deref() else {
		return Ok(());
	};
	let mut session = core_session::load(&ctx.workspaces_dir, id).await?;
	if session.coder_provider_lock == lock {
		return Ok(());
	}
	session.coder_provider_lock = lock;
	core_session::save(&ctx.workspaces_dir, id, &session).await
}

/// Current model/provider settings: the runner's live view plus the
/// workspace's provider lock. The read/write payload of the picker
/// and the companion's provider card.
pub async fn get_model_settings(coder: &CoderHandle, ctx: &SettingsContext) -> Result<CoderModelSettings, MoonError> {
	let models = coder.current_models().await;
	let provider_lock = workspace_provider_lock(ctx).await;
	// Resolve before the fields move out of `models` below.
	let resolved_standard_model = models.standard().to_owned();
	Ok(CoderModelSettings {
		standard_model: models.standard,
		cheap_model: models.cheap,
		bill_to: models.bill_to.unwrap_or_default(),
		active_provider: models.active_provider,
		providers: models.providers,
		// Clone out of the `Arc<HashMap>`: the picker mutates the map
		// locally and round-trips it back through the set path;
		// sharing the `Arc` would risk a write through a stale clone.
		context_window_overrides: (*models.context_window_overrides).clone(),
		provider_lock,
		resolved_standard_model,
	})
}

/// Persist + apply new picker settings. Pokes the runner so the next
/// round-trip uses the new picks, persists the per-workspace lock to
/// `session.json` first (a transient failure must not promote a
/// workspace pin to the global default), then the global defaults to
/// `state.json`. API keys never travel through this path — they're
/// keyring-only via the per-id key commands.
pub async fn set_model_settings(
	coder: &CoderHandle,
	ctx: &SettingsContext,
	settings: CoderModelSettings,
) -> Result<(), MoonError> {
	let bill_to = if settings.bill_to.is_empty() {
		None
	} else {
		Some(settings.bill_to.clone())
	};
	let providers_for_runner = settings.providers.clone();
	let active_for_runner = settings.active_provider.clone();
	let overrides_for_runner = settings.context_window_overrides.clone();
	let provider_lock = settings.provider_lock.clone();
	coder
		.set_user_picks(settings.standard_model.clone(), settings.cheap_model.clone(), bill_to)
		.await;
	// Runner always gets the effective active provider (lock if
	// pinned, else the global). The picker pre-resolved this onto
	// `settings.active_provider`, so we forward verbatim.
	coder.set_providers(providers_for_runner, active_for_runner).await;
	coder.set_context_window_overrides(overrides_for_runner).await;

	write_workspace_provider_lock(ctx, provider_lock.clone()).await?;

	let lock_active_provider = match &provider_lock {
		Some(CoderProviderLock::Hf) => Some(None),
		Some(CoderProviderLock::User { id }) => Some(Some(id.clone())),
		None => None,
	};
	app_state_store::mutate(&ctx.config_dir, move |s| {
		s.coder.standard_model = settings.standard_model;
		s.coder.cheap_model = settings.cheap_model;
		s.coder.bill_to = settings.bill_to;
		// Only the unlocked path writes back to the global active
		// provider — locked saves keep the global frozen so other
		// workspaces aren't dragged along.
		if lock_active_provider.is_none() {
			s.coder.active_provider = settings.active_provider;
		}
		// Strip `has_api_key` before persisting — it's keyring-derived,
		// not state.
		s.coder.providers = settings
			.providers
			.into_iter()
			.map(|mut p| {
				p.has_api_key = false;
				p
			})
			.collect();
		// Drop `0`-valued caps on persist: already "no cap" at the
		// runtime boundary; keeping them would litter `state.json`.
		s.coder.context_window_overrides = settings
			.context_window_overrides
			.into_iter()
			.filter(|(_, v)| *v > 0)
			.collect();
	})
	.await?;
	Ok(())
}
