//! Wire types pushed from the loop to the UI.
//!
//! Mirrored 1:1 in `src/lib/protocol.ts`. A change here is a protocol
//! change — bump `moon_protocol::PROTOCOL_VERSION` if it's a breaking
//! shape edit.

use serde::{Deserialize, Serialize};

/// Outer envelope carrying a folder tag alongside the inner
/// [`CoderEvent`]. Every event the runner emits goes through this
/// shape on the wire so the multi-session frontend can route
/// updates to the right per-folder UI bucket.
///
/// Folder is the absolute path of the **session's bound folder**
/// (matches `WorkspaceFolder.path`), even for sub-agent events:
/// sub-agents belong to whichever project originated them, so a
/// `SubagentEvent` arrives tagged with the **parent's** folder
/// regardless of which folder the sub-agent's tools operate on
/// (`target_folder` lives inside `SubagentSpawned.target_folder`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoderEventEnvelope {
	pub folder: String,
	pub event: CoderEvent,
}

/// One push event the loop sends to the panel. Tagged enum so the
/// UI can `switch (event.kind)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoderEvent {
	/// The user's prompt is in the session and a turn just started.
	/// Carries the message verbatim so the UI can render it without
	/// echoing what it just sent.
	UserMessage { id: String, text: String },

	/// A new assistant message bubble has started in the current
	/// turn — fires before the first `AssistantMessageDelta` for a
	/// given `id`. The UI inserts an empty bubble; subsequent
	/// deltas append. Splitting start from the first delta keeps
	/// the empty-message case (model emits only tool calls, no
	/// content) clean: no start, no deltas, no bubble.
	AssistantMessageStart { id: String },

	/// Append `delta` to the bubble identified by `id`. Fired per
	/// SSE chunk. The frontend creates the bubble lazily if a delta
	/// arrives without a prior `AssistantMessageStart` (defensive
	/// against future provider quirks).
	AssistantMessageDelta { id: String, delta: String },

	/// Append `delta` to the *thinking* trace of the message
	/// identified by `id`. Fires only when the underlying provider
	/// streams a reasoning trace (DeepSeek `reasoning_content`,
	/// some others under `reasoning`). The frontend renders this
	/// in a collapsible block above the message body; if no
	/// thinking deltas ever arrive, the block isn't shown at all.
	/// No matching `Start` event — the frontend lazy-creates the
	/// thinking section on the first delta.
	AssistantThinkingDelta { id: String, delta: String },

	/// The assistant message identified by `id` is complete. `text`
	/// is the canonical full content; `thinking` is the canonical
	/// full reasoning trace (`None` when the provider doesn't
	/// expose one). The frontend replaces its accumulated strings
	/// with these so any drift between concatenated deltas and the
	/// final assembly heals on close. The UI also (re)runs
	/// markdown rendering on the final text and auto-collapses the
	/// thinking block.
	AssistantMessageEnd {
		id: String,
		text: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		thinking: Option<String>,
	},

	/// The model issued a tool call. Fires before the tool runs so
	/// the panel can render an "in progress" block.
	ToolCall {
		id: String,
		name: String,
		args: serde_json::Value,
	},

	/// The tool finished. `is_error` is `true` when the tool
	/// returned an `Err(_)` — the model receives an `isError: true`
	/// content block in the next round and may retry or explain.
	ToolResult {
		id: String,
		result: serde_json::Value,
		is_error: bool,
	},

	/// The whole turn ended cleanly.
	TurnComplete,

	/// The user (or `Coder::abort`) cancelled the turn.
	Aborted,

	/// Non-recoverable error during the turn — auth gone bad mid-
	/// stream, decode error from the router, etc. The panel renders
	/// this as a system-level toast + error block.
	Error { message: String },

	/// A different session was just opened (or a fresh one
	/// created). Frontend clears its row list and starts replaying
	/// the new session's records into it. Carries a snapshot of
	/// the active session's metadata so the sticky header can
	/// render without a separate IPC round trip.
	SessionLoaded {
		id: String,
		title: String,
		created_at_ms: i64,
		updated_at_ms: i64,
	},

	/// The active session's title was rewritten — either by the
	/// auto-rename pass after the first turn, or (Phase 6.4+) by
	/// an explicit user rename. Frontend updates the sticky
	/// header + the row in the sessions list. The new title is
	/// also persisted as a
	/// [`crate::sessions::SessionRecord::TitleUpdate`] on disk so
	/// re-opening sees it.
	SessionTitleUpdated { id: String, title: String },

	/// The on-disk session list changed (new file, deleted file,
	/// title bump). Frontend re-fetches via `coder_list_sessions`
	/// rather than us pushing the full list — keeps the wire
	/// shape small at the cost of one extra round trip.
	SessionListChanged,

	/// A bound folder's cached summary just refreshed. The runner
	/// uses this to refresh the parent's "Bound folders" section in
	/// the system prompt on the next turn; the project bar can
	/// also re-fetch via `coder_folder_summary` to update tooltips
	/// without polling. Carries the absolute folder path (matches
	/// `WorkspaceFolder.path`) so the frontend can route by exact
	/// match without recomputing slugs.
	FolderSummaryReady { folder: String, description: String },

	/// A new sub-agent has been registered against a parent
	/// `tool_call_id`. Frontend uses this to insert a collapsed
	/// summary card under the spawn_subagent tool row. `mode` is
	/// the wire string ("research" / "coder") so the UI badge
	/// renders without re-deriving the enum.
	SubagentSpawned {
		tool_call_id: String,
		subagent_id: String,
		target_folder: String,
		mode: String,
	},

	/// One inner event from a sub-agent's runner. The frontend
	/// routes by `subagent_id` into the per-sub-agent transcript
	/// store and renders `inner` with the same row components the
	/// parent transcript uses. Boxed because `CoderEvent` is
	/// recursive — `SubagentEvent.inner` is itself a `CoderEvent`.
	SubagentEvent {
		subagent_id: String,
		inner: Box<CoderEvent>,
	},

	/// A sub-agent has finished (success or error). Frontend flips
	/// the collapsed card's status pip and stops accepting deltas
	/// for the matching `subagent_id`. `tokens_used_estimate` is
	/// approximated from message bytes today; precise tracking
	/// arrives when streaming `usage` is plumbed.
	SubagentFinished {
		subagent_id: String,
		tokens_used_estimate: u32,
		was_error: bool,
	},
}

/// Snapshot of the agent's auth + session state. Returned from
/// `coder_status`; the panel polls this on mount so reopens land in
/// the correct state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoderStatus {
	pub signed_in: bool,
	pub identity: Option<crate::auth::HfIdentity>,
	/// True while a turn is running. The panel uses this to keep the
	/// stop button visible across reloads (the event stream alone
	/// doesn't survive a webview refresh).
	pub busy: bool,
	/// Where the `bash` tool will run for the active folder —
	/// `"host"` or `"container"`. `None` when no folder is active
	/// (the panel still works for chat without a folder; tool calls
	/// just fail with `NoActiveFolder`). Mirrors the `target` field
	/// emitted in `bash` tool results.
	pub bash_target: Option<String>,
}
