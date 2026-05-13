//! Persisted, machine-local app state. Owned by moon-core's
//! [`moon_core::app_state`] storage layer, but the *shape* lives here so
//! the frontend and the backend agree on it byte-for-byte over IPC.
//!
//! There is deliberately no `Settings` type. Project-level code style
//! (indentation, EOL, charset) is delegated to `.editorconfig` from
//! Phase 1.5 onward; everything else moon-ide stores about a user is
//! per-machine and lives here. Per AGENTS.md "no premature migrations":
//! we change this struct freely until the roadmap is done — there are no
//! aliases or fallbacks.
//!
//! Per-workspace UI state (folders, tabs, splits, focused folder, SCM
//! filters) does **not** live here — it lives in
//! `<workspaces_dir>/<id>/session.json`, owned by
//! [`moon_core::session`]. Splitting the two means a multi-workspace
//! launch doesn't fight over a single session slot.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::coder_models::CoderProviderConfig;
use crate::next_edit::NextEditAppState;
use crate::slack::SlackBotProfile;
use crate::theme::ThemeMode;
use crate::workspace::WorkspaceMeta;

// On-disk persisted state — explicitly **not** `deny_unknown_fields`.
// When we delete a field in a later version, the next launch's
// deserializer should silently drop the obsolete key instead of
// rejecting the whole file (which throws away every other piece of
// persisted state in the process: open folders, panel sizes, last
// session, etc.). The pre-stable-schema rule in AGENTS.md (delete
// fields freely; don't write migrations) only works if the reader is
// permissive about *extra* fields. Wire protocols like
// `crate::workspace::WorkspaceMeta` still use `deny_unknown_fields`
// because there a typo in either party's struct is a real bug.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct AppState {
	/// Catalog of every workspace the user has on this machine
	/// (Phase 7.2). Empty until the user names their first
	/// workspace in preboot mode. Mutated through the
	/// `workspace_create` / `workspace_delete` /
	/// `workspace_rename` IPC; the launcher reads it to pick
	/// the most-recently-active slug to spawn (Phase 7.9).
	#[serde(default)]
	pub workspaces: Vec<WorkspaceMeta>,
	/// Active UI theme. Per-machine; survives workspace switches.
	pub theme: ThemeMode,
	/// Per-machine, non-secret Slack panel state. The `xoxp-` token
	/// itself never lives here — it stays in the OS keyring (see
	/// `specs/slack-chat.md`).
	pub slack: SlackAppState,
	/// Bottom-panel visibility + height. Hosts service-log streams and
	/// (Phase 5) terminals, so it's worth restoring across launches —
	/// users tend to live with it open or closed and resent the panel
	/// re-jumping to a default height every restart.
	pub bottom_panel: BottomPanelAppState,
	/// Which surface — chat or coder — is mounted in the single
	/// right-side panel slot. `None` means the slot is closed. Chat
	/// and coder are mutually exclusive: opening one swaps the other
	/// out rather than stacking. Persisted so the user lands back in
	/// whichever surface they had open at last shutdown. Defaults to
	/// `None` (closed) for first-run users.
	pub right_panel: Option<RightPanelKind>,
	/// Per-machine coder state — picks up where the user left off
	/// without forcing them to navigate the sessions list again.
	pub coder: CoderAppState,
	/// Local llama.cpp autocomplete: managed `llama-server` spawn fields + optional external HTTP base.
	#[serde(default)]
	pub next_edit: NextEditAppState,
}

/// Surface mounted in the right-side panel. Chat and coder are
/// mutually exclusive; this enum encodes the pick. The slot can also
/// be closed entirely (`None` on `AppState::right_panel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RightPanelKind {
	Chat,
	Coder,
}

/// Slack-specific slice of [`AppState`]. Only stores derived,
/// non-secret pointers so we can reload the chat panel on launch
/// without re-running the bot picker. Panel visibility lives at the
/// top level on [`AppState::right_panel`] — chat and coder share
/// one slot.
// See the note on [`AppState`] re: `deny_unknown_fields` and on-disk
// state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct SlackAppState {
	/// Bot the user picked from the DM list. `None` means "show the
	/// picker on next chat-panel render". Cleared by an explicit "Pick
	/// a different bot" gesture or when `auth.test` reports the token
	/// is dead.
	pub active_bot: Option<SlackBotProfile>,
	/// `thread_ts` of the session the user last had open in the chat
	/// panel. Restored on launch so reopening the panel jumps back
	/// into the same conversation. Cleared on bot switch and on
	/// disconnect — bot pick and active thread are coupled (the
	/// thread lives inside the bot's DM channel, ID encoded in
	/// `active_bot.dm_channel_id`).
	pub active_thread_ts: Option<String>,
}

/// Coder-specific slice of [`AppState`].
///
/// Only frontend-side affordance pointers — the actual session
/// content lives under
/// `<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/<id>.jsonl`,
/// not here. See [`crate::session`] / `crates/moon-coder/src/sessions.rs`
/// for the on-disk format.
// See the note on [`AppState`] re: `deny_unknown_fields` and on-disk
// state. The `default_provider` field that lived here in an earlier
// build is exactly the case this is meant to handle — old state.json
// files in the wild still carry it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct CoderAppState {
	/// Last-opened session id **per workspace folder**. Restored on
	/// launch when the user revisits a folder: the active folder's
	/// entry decides which session the panel mounts. Per the
	/// multi-session design, every project gets its own slot so a
	/// re-open of folder X resumes X's last session even if the
	/// user has worked in folder Y in between. Cleared per-folder
	/// when the matching session gets deleted; an `open_session`
	/// call updates that folder's entry.
	#[serde(default)]
	pub last_session_by_folder: std::collections::HashMap<String, String>,
	/// Slug of the "standard" model driving the main agent loop +
	/// every sub-agent. Empty means "use the hardcoded default"
	/// (`DEFAULT_STANDARD_MODEL` in `crates/moon-coder/src/defaults.rs`).
	/// Format mirrors what the HF Inference Providers router accepts
	/// in the request body — bare `Qwen/Qwen3.5-397B-A17B`, or
	/// suffixed with `:scaleway` / `:fastest` / etc.
	#[serde(default)]
	pub standard_model: String,
	/// Slug of the "cheap" model used for auto-rename, branch-name
	/// suggester, commit-message suggester, compaction summary, and
	/// folder-summary onboarding. Same format as `standard_model`.
	/// Empty = `DEFAULT_CHEAP_MODEL`.
	#[serde(default)]
	pub cheap_model: String,
	/// Organisation slug to send as the `X-HF-Bill-To` header on every
	/// inference call. Empty = bill the user's personal account.
	/// The user must be a paying member of the org and the org must
	/// have inference credits, otherwise the router rejects the
	/// request — we surface the router's error verbatim.
	#[serde(default)]
	pub bill_to: String,
	/// User-added OpenAI-compatible providers (OpenRouter, locally
	/// hosted vLLM / Ollama / llama.cpp, …). The HF route is
	/// always implicitly available and is **not** in this list —
	/// it's the default when [`active_provider`] is `None`.
	///
	/// Each entry carries its own `standard_model` / `cheap_model`
	/// because slugs aren't portable between hosts (an OpenRouter
	/// `anthropic/claude-3.5-sonnet` doesn't resolve on HF and vice
	/// versa). Switching the active provider swaps the picks the
	/// runner uses with it.
	///
	/// API keys do **not** live here. They're stored in the OS
	/// keyring under `service=moon-ide`, `account=coder-provider:<id>`
	/// — moving them out of `state.json` keeps secrets off disk in a
	/// file the user might commit by accident.
	///
	/// [`active_provider`]: Self::active_provider
	#[serde(default)]
	pub providers: Vec<CoderProviderConfig>,
	/// Id of the currently active provider — `None` for the
	/// implicit HF route. When `Some(id)`, must match one of
	/// [`providers`](Self::providers)`.id`; the runner falls back
	/// to HF on a mismatch (e.g. the entry was deleted out of
	/// band) and a `tracing::warn!` notes the orphan.
	#[serde(default)]
	pub active_provider: Option<String>,
}

/// Bottom-panel slice of [`AppState`].
///
/// Tab contents (open log streams, terminal sessions) are intentionally
/// not persisted: they're tied to running `docker compose logs -f`
/// processes that don't survive a launch, and re-spawning them blindly
/// on startup would surprise the user. Visibility + height are pure
/// chrome and safe to restore.
// See the note on [`AppState`] re: `deny_unknown_fields` and on-disk
// state.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct BottomPanelAppState {
	/// Whether the bottom panel was open at last shutdown. Defaults to
	/// `false` — first-run users shouldn't have an empty panel
	/// occupying screen real estate before they ask for it.
	pub visible: bool,
	/// Panel height in CSS pixels. Clamped to a sane range on the
	/// frontend so a saved 0 / huge value can't render the editor
	/// invisible.
	pub height: u32,
}

impl Default for BottomPanelAppState {
	fn default() -> Self {
		Self {
			visible: false,
			// Matches `DEFAULT_BOTTOM_PANEL_HEIGHT` in
			// `src/lib/bottomPanel.svelte.ts`. Tall enough to show ~12
			// lines of log output at the default editor font size on
			// a typical 1080p screen, without crowding the editor.
			height: 240,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// Pre-stable schema rule: deleting a field from `*AppState`
	// shouldn't reject the whole state file on the next launch. The
	// failure mode this prevents in practice is "user updates moon-ide,
	// loses every workspace / panel / session state because one stale
	// key was still in `state.json`". The structs explicitly opt **out**
	// of `deny_unknown_fields`; this test pins that decision.
	#[test]
	fn app_state_tolerates_obsolete_fields() {
		// Mix a real current key (`workspaces` is empty but valid)
		// with an obsolete one we know used to exist, plus a
		// completely made-up key for paranoia.
		let json = r#"{
			"workspaces": [],
			"obsolete_top_level_field": 42,
			"coder": {
				"standard_model": "Qwen/Qwen3.5-397B-A17B:scaleway",
				"default_provider": "scaleway",
				"some_future_field": "anything"
			}
		}"#;
		let parsed: AppState = serde_json::from_str(json).expect("parses despite obsolete keys");
		assert_eq!(parsed.coder.standard_model, "Qwen/Qwen3.5-397B-A17B:scaleway");
		assert!(parsed.coder.providers.is_empty());
		assert!(parsed.coder.active_provider.is_none());
	}

	#[test]
	fn app_state_round_trips_user_providers() {
		let parsed: AppState = serde_json::from_str(
			r#"{
				"coder": {
					"providers": [
						{
							"id": "or-1",
							"label": "OpenRouter",
							"base_url": "https://openrouter.ai/api/v1",
							"standard_model": "anthropic/claude-3.5-sonnet",
							"cheap_model": "openai/gpt-4o-mini"
						}
					],
					"active_provider": "or-1"
				}
			}"#,
		)
		.expect("parses provider entry");
		assert_eq!(parsed.coder.providers.len(), 1);
		assert_eq!(parsed.coder.providers[0].id, "or-1");
		assert_eq!(parsed.coder.providers[0].base_url, "https://openrouter.ai/api/v1");
		assert!(!parsed.coder.providers[0].has_api_key); // never persisted on disk
		assert_eq!(parsed.coder.active_provider.as_deref(), Some("or-1"));
	}
}
