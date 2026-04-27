//! Tauri commands wrapping `moon-slack`.
//!
//! Phase 11.0 surface: connect (paste token + validate), status check,
//! disconnect, scan the user's DMs for bots, persist the picked bot.
//! See `specs/slack-chat.md` and
//! `specs/test-plans/0008-slack-foundation.md`.
//!
//! All token I/O goes through the OS keyring; nothing about the token
//! ever lands in `app_state.json` or the session blob. The picked bot
//! profile *does* live in `app_state.json` (it's just IDs + display
//! metadata, no secrets) so the picker doesn't reappear on every launch.

use moon_core::app_state as app_state_store;
use moon_protocol::slack::{SlackBotProfile, SlackIdentity, SlackStatus};
use moon_protocol::MoonError;
use moon_slack::SlackClient;
use tauri::State;

use crate::state::AppState;

/// Validate the user-pasted `xoxp-` token via `auth.test`. On success
/// the token is stored in the OS keyring and cached as a [`SlackClient`]
/// in app state, then [`SlackIdentity`] is returned. On any failure
/// nothing is persisted and the error surfaces unchanged so the
/// connect modal can render Slack's own message.
#[tauri::command]
pub async fn slack_set_token(state: State<'_, AppState>, token: String) -> Result<SlackIdentity, MoonError> {
	let token = token.trim().to_string();
	if !token.starts_with("xoxp-") {
		return Err(MoonError::invalid(
			"token must start with 'xoxp-' (Slack User OAuth Token)",
		));
	}
	let client = SlackClient::new(token.clone()).map_err(MoonError::from)?;
	let identity = client.auth_test().await.map_err(MoonError::from)?;
	state.slack.tokens.save(&token).map_err(MoonError::from)?;
	state.slack.set_client(client).await;
	Ok(identity)
}

/// Cheap connectivity probe. Returns `connected: false` whenever no
/// token is cached or the cached token's `auth.test` fails — and
/// when it fails for an auth reason, the keyring entry is purged so
/// the next launch shows the empty state instead of failing again.
#[tauri::command]
pub async fn slack_status(state: State<'_, AppState>) -> Result<SlackStatus, MoonError> {
	let Some(client) = state.slack.client().await else {
		return Ok(SlackStatus {
			connected: false,
			identity: None,
		});
	};
	match client.auth_test().await {
		Ok(identity) => Ok(SlackStatus {
			connected: true,
			identity: Some(identity),
		}),
		Err(err) => {
			if err.is_auth_failure() {
				tracing::warn!(error = %err, "slack token rejected; clearing keyring entry");
				let _ = state.slack.tokens.clear();
				state.slack.clear_client().await;
				clear_active_bot_on_disk(&state).await;
			}
			Ok(SlackStatus {
				connected: false,
				identity: None,
			})
		}
	}
}

/// Drop the keyring entry, the cached client, and any persisted bot
/// pick. Idempotent.
#[tauri::command]
pub async fn slack_clear_token(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.slack.tokens.clear().map_err(MoonError::from)?;
	state.slack.clear_client().await;
	clear_active_bot_on_disk(&state).await;
	Ok(())
}

/// Scan the user's open DMs and return every DM partner that's a bot.
/// See `specs/slack-chat.md#bot-resolution` for the rationale (no
/// public user-search endpoint, `users.list` doesn't scale).
///
/// Order matches Slack's DM ordering (newest activity first), which
/// surfaces the bot the user has talked to most recently at the top
/// of the picker.
#[tauri::command]
pub async fn slack_list_dm_bots(state: State<'_, AppState>) -> Result<Vec<SlackBotProfile>, MoonError> {
	let Some(client) = state.slack.client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client.list_dm_bots().await.map_err(MoonError::from)
}

/// Record the bot the user picked. Stored verbatim in `app_state.json`
/// so the picker doesn't run again on next launch.
#[tauri::command]
pub async fn slack_select_bot(state: State<'_, AppState>, profile: SlackBotProfile) -> Result<(), MoonError> {
	let mut current = app_state_store::load(&state.config_dir).await?;
	current.slack.active_bot = Some(profile);
	app_state_store::save(&state.config_dir, &current).await
}

/// Drop the persisted bot pick. Triggers the picker on next chat-panel
/// render. Idempotent.
#[tauri::command]
pub async fn slack_clear_bot(state: State<'_, AppState>) -> Result<(), MoonError> {
	clear_active_bot_on_disk(&state).await;
	Ok(())
}

/// Read the persisted bot pick, if any. Called once on chat-panel mount
/// (and after disconnect/reconnect cycles) so the panel can show the
/// active bot's card without rerunning discovery.
#[tauri::command]
pub async fn slack_get_active_bot(state: State<'_, AppState>) -> Result<Option<SlackBotProfile>, MoonError> {
	let current = app_state_store::load(&state.config_dir).await?;
	Ok(current.slack.active_bot)
}

/// Persist whether the chat panel is currently shown. Called from the
/// frontend on every show/hide so a relaunch lands the user back in the
/// same layout. Frontend-owned state (no token / Slack work involved)
/// but we keep it in the slack slice because it's conceptually part of
/// the chat panel's session.
#[tauri::command]
pub async fn slack_set_panel_visible(state: State<'_, AppState>, visible: bool) -> Result<(), MoonError> {
	let mut current = app_state_store::load(&state.config_dir).await?;
	if current.slack.panel_visible == visible {
		return Ok(());
	}
	current.slack.panel_visible = visible;
	app_state_store::save(&state.config_dir, &current).await
}

async fn clear_active_bot_on_disk(state: &AppState) {
	match app_state_store::load(&state.config_dir).await {
		Ok(mut current) => {
			if current.slack.active_bot.is_none() {
				return;
			}
			current.slack.active_bot = None;
			if let Err(err) = app_state_store::save(&state.config_dir, &current).await {
				tracing::warn!(error = %err, "failed to clear persisted slack bot");
			}
		}
		Err(err) => {
			tracing::warn!(error = %err, "failed to load app state while clearing slack bot");
		}
	}
}
