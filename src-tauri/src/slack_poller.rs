//! Background `conversations.replies` poller for the active Slack
//! thread. Drives push-style updates to the chat panel without
//! Slack's real-time APIs (Events / Socket Mode / RTM all require a
//! bot token or a hosted endpoint we don't have).
//!
//! ## Why poll
//!
//! The panel uses a *user* token (`xoxp-…`) — Events API needs a
//! verified webhook target, Socket Mode needs an app token, RTM is
//! deprecated. Polling is the only path that works against a desktop
//! client speaking solely the Web API. See the trade-off discussion
//! in `specs/slack-chat.md`.
//!
//! ## Cadence ladder
//!
//! Per-thread frequency derived from "time since the last
//! new-message-or-edit on this thread":
//!
//! | Elapsed                  | Tick    |
//! | ------------------------ | ------- |
//! | < 30 s   ("hot")         | 3 s     |
//! | 30 s – 2 min ("warm")    | 5 s     |
//! | 2 min – 10 min           | 15 s    |
//! | 10 min – 1 h             | 60 s    |
//! | > 1 h ("cold")           | paused  |
//!
//! Cold means we only re-poll on user interaction (panel toggled,
//! session switched, OS focus regained). The whole loop is paused
//! whenever the panel is hidden, no session is selected, or the
//! Slack client isn't connected — there's no point spending Slack
//! quota on a UI the user can't see.
//!
//! ## Read receipts
//!
//! After a successful poll, if the panel is visible **and** the OS
//! window is focused **and** the latest visible message changed
//! since we last marked, fire `conversations.mark`. The "focused"
//! gate matters: an unfocused-but-visible panel has not actually
//! been read by the human; clearing the unread badge silently would
//! lose information. The frontend is responsible for the on-view
//! and on-session-switch marks (see `slack_mark_read` command);
//! this task only handles the on-poll-tick case.
//!
//! ## Errors
//!
//! - **Auth failure** (`invalid_auth`, `token_revoked`, …): emit
//!   `slack:disconnected`, drop the cached client, clear the keyring
//!   entry and the persisted bot pick. Frontend resets to the
//!   "Connect Slack" empty state.
//! - **Transport / 5xx / rate limit**: log + retry at the next
//!   cadence tick. No exponential backoff yet — the cadence ladder
//!   already throttles to 60 s for cold threads, which is plenty of
//!   headroom for transient outages.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use moon_core::app_state as app_state_store;
use moon_protocol::slack::SlackMessage;
use moon_slack::{SlackClient, TokenStore};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::select;
use tokio::sync::{Notify, RwLock};
use tokio::time::{sleep, Instant};

/// Tauri event name pushed when the active thread's contents change
/// (additions or `edited.ts` edits). Payload: [`ThreadUpdatePayload`].
pub const THREAD_UPDATE_EVENT: &str = "slack:thread-update";

/// Tauri event name pushed when the cached token has been rejected
/// mid-poll. Carries no payload — the frontend resets to empty state
/// and re-runs `slack_status` on its own.
pub const DISCONNECTED_EVENT: &str = "slack:disconnected";

/// Snapshot pushed to the frontend on every detected change.
///
/// We send the **full thread** rather than a delta on purpose:
/// reconciliation against Slack's `(ts, edited_ts)` shape is fiddly
/// (especially with deletes, which Slack reports as a separate
/// `subtype: "message_deleted"` we'd have to track), and threads are
/// short enough that re-rendering 50 KB of JSON every 3 s during a
/// hot exchange is fine. The frontend filters by `(channel,
/// thread_ts)` matching the open session before applying.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadUpdatePayload {
	pub channel: String,
	pub thread_ts: String,
	pub messages: Vec<SlackMessage>,
}

#[derive(Debug, Default, Clone)]
struct PollerInputs {
	panel_visible: bool,
	os_focused: bool,
	active_channel: Option<String>,
	active_thread_ts: Option<String>,
}

/// Cheap-clone handle to the running poll task. Held by `AppState`
/// and by every Tauri command that needs to feed inputs in.
#[derive(Clone)]
pub struct PollerHandle {
	inputs: Arc<Mutex<PollerInputs>>,
	wakeup: Arc<Notify>,
}

impl PollerHandle {
	fn new() -> Self {
		Self {
			inputs: Arc::new(Mutex::new(PollerInputs::default())),
			wakeup: Arc::new(Notify::new()),
		}
	}

	pub fn set_panel_visible(&self, visible: bool) {
		self.update(|i| i.panel_visible = visible);
	}

	pub fn set_os_focused(&self, focused: bool) {
		self.update(|i| i.os_focused = focused);
	}

	pub fn set_active_channel(&self, channel: Option<String>) {
		self.update(|i| {
			i.active_channel = channel;
			// Bot change clears any pending thread — sessions live
			// inside one bot's DM channel and the new bot inherits
			// nothing.
			i.active_thread_ts = None;
		});
	}

	pub fn set_active_thread_ts(&self, thread_ts: Option<String>) {
		self.update(|i| i.active_thread_ts = thread_ts);
	}

	/// Nudge the loop without changing inputs — used by paths that
	/// affect external state the loop reads but doesn't observe
	/// directly (e.g. the cached `SlackClient` flips from `None` to
	/// `Some` after a successful reconnect). Cheap.
	pub fn poke(&self) {
		self.wakeup.notify_one();
	}

	/// Note: `last_marked_ts` lives inside the loop's local state,
	/// so this is purely a hint to the loop ("the frontend just
	/// fired `conversations.mark` on its own"). The loop won't
	/// actually deduplicate against it — frontend marks are rare
	/// (view + session-switch) and the server-side call is
	/// idempotent.
	fn update<F: FnOnce(&mut PollerInputs)>(&self, f: F) {
		{
			let mut guard = self.inputs.lock().expect("poller inputs mutex poisoned");
			f(&mut *guard);
		}
		self.wakeup.notify_one();
	}

	fn snapshot(&self) -> PollerInputs {
		self.inputs.lock().expect("poller inputs mutex poisoned").clone()
	}

	async fn wait_for_change(&self) {
		self.wakeup.notified().await;
	}
}

/// Spawn the poll loop and return a handle bundling input setters.
/// Call once at app startup. The task lives until the Tauri runtime
/// shuts down (no clean cancellation needed — at process exit the
/// in-flight HTTP request gets dropped, which is harmless).
pub fn spawn(
	app: AppHandle,
	client: Arc<RwLock<Option<SlackClient>>>,
	tokens: TokenStore,
	config_dir: Utf8PathBuf,
) -> PollerHandle {
	let handle = PollerHandle::new();
	let task = PollTask {
		app,
		client,
		tokens,
		config_dir,
		handle: handle.clone(),
		current_active: None,
		last_seen: Vec::new(),
		last_activity_at: Instant::now(),
		last_marked_ts: None,
	};
	tokio::spawn(task.run());
	handle
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTarget {
	channel: String,
	thread_ts: String,
}

struct PollTask {
	app: AppHandle,
	client: Arc<RwLock<Option<SlackClient>>>,
	tokens: TokenStore,
	config_dir: Utf8PathBuf,
	handle: PollerHandle,
	current_active: Option<ActiveTarget>,
	last_seen: Vec<MessageFingerprint>,
	last_activity_at: Instant,
	last_marked_ts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageFingerprint {
	ts: String,
	edited_ts: Option<String>,
}

impl PollTask {
	async fn run(mut self) {
		loop {
			let inputs = self.handle.snapshot();

			// Active-thread transitions (including disconnect) reset
			// the per-thread bookkeeping so a stale `last_seen` from
			// a previous session can't suppress the first emit on
			// the new one.
			let next_active = inputs
				.active_channel
				.as_ref()
				.zip(inputs.active_thread_ts.as_ref())
				.map(|(c, t)| ActiveTarget {
					channel: c.clone(),
					thread_ts: t.clone(),
				});
			if next_active != self.current_active {
				self.reset_active(next_active);
				// Fall through to the should_poll gate; if the new
				// target is Some + visible + connected, the loop
				// polls immediately on this iteration.
			}

			let should_poll = self.current_active.is_some() && inputs.panel_visible && self.has_client().await;
			if !should_poll {
				self.handle.wait_for_change().await;
				continue;
			}

			let elapsed = Instant::now().duration_since(self.last_activity_at);
			match cadence(elapsed) {
				None => {
					// Cold thread: pause until something changes.
					tracing::trace!(elapsed_secs = elapsed.as_secs(), "slack poller cold; pausing");
					self.handle.wait_for_change().await;
					continue;
				}
				Some(tick) => {
					select! {
						() = sleep(tick) => self.poll_once(&inputs).await,
						() = self.handle.wait_for_change() => continue,
					}
				}
			}
		}
	}

	fn reset_active(&mut self, next: Option<ActiveTarget>) {
		self.current_active = next;
		self.last_seen.clear();
		self.last_activity_at = Instant::now();
		self.last_marked_ts = None;
	}

	async fn has_client(&self) -> bool {
		self.client.read().await.is_some()
	}

	async fn poll_once(&mut self, inputs: &PollerInputs) {
		let Some(active) = self.current_active.clone() else {
			return;
		};
		let Some(client) = self.client.read().await.clone() else {
			return;
		};

		let messages = match client.get_thread(&active.channel, &active.thread_ts).await {
			Ok(m) => m,
			Err(err) if err.is_auth_failure() => {
				tracing::warn!(error = %err, "slack poller saw auth failure; disconnecting");
				self.handle_auth_failure().await;
				return;
			}
			Err(err) => {
				tracing::debug!(error = %err, channel = %active.channel, thread_ts = %active.thread_ts, "slack poller transient error");
				return;
			}
		};

		let fingerprints: Vec<MessageFingerprint> = messages
			.iter()
			.map(|m| MessageFingerprint {
				ts: m.ts.clone(),
				edited_ts: m.edited_ts.clone(),
			})
			.collect();
		let changed = fingerprints != self.last_seen;
		if changed {
			self.last_seen = fingerprints;
			self.last_activity_at = Instant::now();
			self.emit_thread_update(&active, messages.clone());
		}

		// Read-receipt gate: only mark when the user is actually
		// looking at the panel (panel_visible + os_focused) and the
		// last visible message changed since we last marked.
		if !changed || !inputs.panel_visible || !inputs.os_focused {
			return;
		}
		let Some(last) = messages.last() else {
			return;
		};
		if Some(&last.ts) == self.last_marked_ts.as_ref() {
			return;
		}
		match client.mark_as_read(&active.channel, &last.ts).await {
			Ok(()) => {
				self.last_marked_ts = Some(last.ts.clone());
			}
			Err(err) if err.is_auth_failure() => {
				tracing::warn!(error = %err, "slack mark_as_read auth failure; disconnecting");
				self.handle_auth_failure().await;
			}
			Err(err) => {
				tracing::debug!(error = %err, "slack mark_as_read transient error");
			}
		}
	}

	fn emit_thread_update(&self, active: &ActiveTarget, messages: Vec<SlackMessage>) {
		let payload = ThreadUpdatePayload {
			channel: active.channel.clone(),
			thread_ts: active.thread_ts.clone(),
			messages,
		};
		if let Err(err) = self.app.emit(THREAD_UPDATE_EVENT, &payload) {
			tracing::warn!(error = %err, "failed to emit slack:thread-update");
		}
	}

	async fn handle_auth_failure(&mut self) {
		// Drop the live client so other commands surface "not
		// connected" immediately, and purge the keyring + persisted
		// bot pick so the next launch lands at the connect modal
		// instead of looping on a dead token.
		*self.client.write().await = None;
		if let Err(err) = self.tokens.clear() {
			tracing::warn!(error = %err, "failed to clear keyring entry after auth failure");
		}
		if let Err(err) = clear_bot_on_disk(&self.config_dir).await {
			tracing::warn!(error = %err, "failed to clear persisted bot after auth failure");
		}
		self.handle.set_active_channel(None);
		self.reset_active(None);
		if let Err(err) = self.app.emit(DISCONNECTED_EVENT, ()) {
			tracing::warn!(error = %err, "failed to emit slack:disconnected");
		}
	}
}

/// The cadence ladder. Returns `None` for "paused" (cold threads —
/// only re-poll on user interaction). The frontend's manual refresh
/// affordances still work via the regular `slack_get_thread` path.
fn cadence(elapsed: Duration) -> Option<Duration> {
	const HOT: Duration = Duration::from_secs(30);
	const WARM: Duration = Duration::from_secs(120);
	const COOL: Duration = Duration::from_secs(600);
	const COLD: Duration = Duration::from_secs(3600);
	if elapsed < HOT {
		Some(Duration::from_secs(3))
	} else if elapsed < WARM {
		Some(Duration::from_secs(5))
	} else if elapsed < COOL {
		Some(Duration::from_secs(15))
	} else if elapsed < COLD {
		Some(Duration::from_secs(60))
	} else {
		None
	}
}

async fn clear_bot_on_disk(config_dir: &Utf8PathBuf) -> Result<(), moon_protocol::MoonError> {
	let mut current = app_state_store::load(config_dir).await?;
	if current.slack.active_bot.is_none() && current.slack.active_thread_ts.is_none() {
		return Ok(());
	}
	current.slack.active_bot = None;
	current.slack.active_thread_ts = None;
	app_state_store::save(config_dir, &current).await
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cadence_ladder_matches_spec() {
		assert_eq!(cadence(Duration::from_secs(0)), Some(Duration::from_secs(3)));
		assert_eq!(cadence(Duration::from_secs(29)), Some(Duration::from_secs(3)));
		assert_eq!(cadence(Duration::from_secs(30)), Some(Duration::from_secs(5)));
		assert_eq!(cadence(Duration::from_secs(119)), Some(Duration::from_secs(5)));
		assert_eq!(cadence(Duration::from_secs(120)), Some(Duration::from_secs(15)));
		assert_eq!(cadence(Duration::from_secs(599)), Some(Duration::from_secs(15)));
		assert_eq!(cadence(Duration::from_secs(600)), Some(Duration::from_secs(60)));
		assert_eq!(cadence(Duration::from_secs(3599)), Some(Duration::from_secs(60)));
		assert_eq!(cadence(Duration::from_secs(3600)), None);
		assert_eq!(cadence(Duration::from_secs(10_000)), None);
	}
}
