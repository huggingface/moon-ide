//! Tauri commands wrapping `moon-slack`.
//!
//! Phase 11.0 surface: connect (paste token + validate), status check,
//! disconnect, scan the user's DMs for bots, persist the picked bot.
//! Phase 11.1 surface: list sessions (top-level DM messages), read a
//! thread, persist the active thread.
//! See `specs/slack-chat.md`, `specs/test-plans/0008-slack-foundation.md`,
//! and `specs/test-plans/0009-slack-read-only-chat.md`.
//!
//! All token I/O goes through the OS keyring; nothing about the token
//! ever lands in `app_state.json` or the session blob. The picked bot
//! profile *does* live in `app_state.json` (it's just IDs + display
//! metadata, no secrets) so the picker doesn't reappear on every launch.

use moon_core::app_state as app_state_store;
use moon_protocol::slack::{SlackBotProfile, SlackIdentity, SlackMessage, SlackSession, SlackStatus, SlackUserSummary};
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
	// Wake the poller — its `has_client()` check turns true now,
	// so if the user was already mid-thread before the token went
	// bad it can resume polling without waiting for the next setter.
	state.slack.poller.poke();
	Ok(identity)
}

/// Cheap connectivity probe. Returns `connected: false` whenever no
/// token is cached or the cached token's `auth.test` fails — and
/// when it fails for an auth reason, the keyring entry is purged so
/// the next launch shows the empty state instead of failing again.
#[tauri::command]
pub async fn slack_status(state: State<'_, AppState>) -> Result<SlackStatus, MoonError> {
	let Some(client) = state.slack.current_client().await else {
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
				state.slack.poller.set_active_channel(None);
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
	state.slack.poller.set_active_channel(None);
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
	let Some(client) = state.slack.current_client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client.list_dm_bots().await.map_err(MoonError::from)
}

/// Record the bot the user picked. Stored verbatim in `app_state.json`
/// so the picker doesn't run again on next launch. Clears any
/// previously-active thread — sessions live inside one bot's DM
/// channel, so a different bot inherits a fresh "no session selected"
/// state.
#[tauri::command]
pub async fn slack_select_bot(state: State<'_, AppState>, profile: SlackBotProfile) -> Result<(), MoonError> {
	let dm_channel_id = profile.dm_channel_id.clone();
	app_state_store::mutate(&state.config_dir, move |s| {
		let bot_changed = s
			.slack
			.active_bot
			.as_ref()
			.is_none_or(|bot| bot.user_id != profile.user_id);
		s.slack.active_bot = Some(profile);
		if bot_changed {
			s.slack.active_thread_ts = None;
		}
	})
	.await?;
	// Feed the poller too. `set_active_channel` clears any pending
	// thread anyway, matching the persisted-state invariant above.
	state.slack.poller.set_active_channel(Some(dm_channel_id));
	Ok(())
}

/// Drop the persisted bot pick. Triggers the picker on next chat-panel
/// render. Idempotent.
#[tauri::command]
pub async fn slack_clear_bot(state: State<'_, AppState>) -> Result<(), MoonError> {
	clear_active_bot_on_disk(&state).await;
	state.slack.poller.set_active_channel(None);
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

/// OS-level focus tracking. Frontend listens to Tauri's
/// `tauri://focus` / `tauri://blur` events and forwards the boolean
/// here; the poller uses this to gate `conversations.mark` (we only
/// clear the unread badge when the user is actually looking at the
/// window — see `specs/slack-chat.md#read-receipts`). Not persisted.
#[tauri::command]
pub async fn slack_set_window_focused(state: State<'_, AppState>, focused: bool) -> Result<(), MoonError> {
	state.slack.poller.set_os_focused(focused);
	Ok(())
}

/// `conversations.history` of the bot's DM channel, filtered to
/// top-level messages. Returns at most
/// `moon_slack::SESSION_HISTORY_LIMIT` entries newest-first. The
/// channel ID comes from the picked [`SlackBotProfile`] — passed
/// explicitly (rather than read from `AppState`) so the command stays
/// stateless and the frontend's "wait, which bot?" race is impossible.
#[tauri::command]
pub async fn slack_list_sessions(state: State<'_, AppState>, channel: String) -> Result<Vec<SlackSession>, MoonError> {
	let Some(client) = state.slack.current_client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client.list_sessions(&channel).await.map_err(MoonError::from)
}

/// `conversations.replies` of one thread in the bot's DM channel.
/// Returns parent + replies, oldest-first (Slack's natural order, the
/// reading order the panel wants).
#[tauri::command]
pub async fn slack_get_thread(
	state: State<'_, AppState>,
	channel: String,
	thread_ts: String,
) -> Result<Vec<SlackMessage>, MoonError> {
	let Some(client) = state.slack.current_client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client.get_thread(&channel, &thread_ts).await.map_err(MoonError::from)
}

/// `conversations.mark` — clear the unread badge for the active
/// session up to `ts`. Frontend fires this on view + on session
/// switch; the polling loop handles the on-tick case automatically.
/// See `specs/slack-chat.md#read-receipts`.
#[tauri::command]
pub async fn slack_mark_read(state: State<'_, AppState>, channel: String, ts: String) -> Result<(), MoonError> {
	let Some(client) = state.slack.current_client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client.mark_as_read(&channel, &ts).await.map_err(MoonError::from)
}

/// Post a message as the connected user. `thread_ts = None` starts
/// a new top-level message (a new session in panel terms);
/// `thread_ts = Some(ts)` posts a reply inside the open thread.
/// Returns the freshly-created [`SlackMessage`] so the frontend can
/// pivot to the new session (top-level case) or reconcile its
/// optimistic UI without waiting for the next poll tick.
#[tauri::command]
pub async fn slack_post_message(
	state: State<'_, AppState>,
	channel: String,
	thread_ts: Option<String>,
	text: String,
) -> Result<SlackMessage, MoonError> {
	let Some(client) = state.slack.current_client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client
		.post_message(&channel, thread_ts.as_deref(), &text)
		.await
		.map_err(MoonError::from)
}

/// Resolve a single `<@U…>` mention to a [`SlackUserSummary`]. Wraps
/// `users.info`. Frontend caches the result per `user_id`, so this
/// command fires at most once per distinct mentioned user per
/// session — see `specs/slack-chat.md#mrkdwn-rendering`.
#[tauri::command]
pub async fn slack_get_user(state: State<'_, AppState>, user_id: String) -> Result<SlackUserSummary, MoonError> {
	let Some(client) = state.slack.current_client().await else {
		return Err(MoonError::HostUnavailable("slack: not connected".into()));
	};
	client.resolve_user(&user_id).await.map_err(MoonError::from)
}

/// Persist the `thread_ts` of the session the user has open. `None`
/// clears the pick (e.g. they hit "back to sessions"). Idempotent.
#[tauri::command]
pub async fn slack_set_active_thread(state: State<'_, AppState>, thread_ts: Option<String>) -> Result<(), MoonError> {
	state.slack.poller.set_active_thread_ts(thread_ts.clone());
	app_state_store::mutate(&state.config_dir, move |s| {
		s.slack.active_thread_ts = thread_ts;
	})
	.await
}

async fn clear_active_bot_on_disk(state: &AppState) {
	let result = app_state_store::mutate(&state.config_dir, |s| {
		s.slack.active_bot = None;
		s.slack.active_thread_ts = None;
	})
	.await;
	if let Err(err) = result {
		tracing::warn!(error = %err, "failed to clear persisted slack bot");
	}
}
