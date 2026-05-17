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
	/// echoing what it just sent. `images` is the data-URL form of
	/// any pictures pasted into the composer; empty for the vast
	/// majority of turns and elided from the wire shape in that
	/// case to keep the common-path payload small.
	UserMessage {
		id: String,
		text: String,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		images: Vec<crate::inference::ImageAttachment>,
	},

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
	/// summary card under the `task` tool row. `mode` is the
	/// wire string ("research" / "agent") so the UI badge renders
	/// without re-deriving the enum.
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
	/// the parent's view of the sub-agent's spend — precise when
	/// the provider emitted a streaming `usage` chunk, falls back
	/// to a bytes/4 estimate otherwise (`source` distinguishes the
	/// two so the UI can mark the latter with a `≈`).
	SubagentFinished {
		subagent_id: String,
		tokens_used_estimate: u32,
		was_error: bool,
	},

	/// Per-iteration token-usage report. Fires once after every
	/// LLM round-trip in the parent loop with the round-trip's
	/// own `prompt_tokens` / `completion_tokens` / `total_tokens`
	/// and the model's hardcoded `context_window`.
	///
	/// `source` is `"provider"` when the usage came from the
	/// provider's streaming `usage` chunk, `"estimate"` when we
	/// fell back to a bytes/4 approximation (some providers don't
	/// emit usage even with `stream_options.include_usage`). The
	/// ring uses the same numbers either way; the `≈` marker
	/// distinguishes accuracy.
	///
	/// `prompt_tokens` is the load-bearing field for compaction:
	/// once it crosses ~80% of `context_window`, the runner
	/// schedules a compaction pass before the next user prompt
	/// goes out.
	///
	/// `cache_read_tokens` / `cache_creation_tokens` are the
	/// Anthropic prompt-caching breakdown (only emitted by
	/// OpenRouter routes that use `cache_control: ephemeral`
	/// markers; `0` everywhere else). Subset of `prompt_tokens`,
	/// not in addition — `cache_read_tokens` is "of the prompt,
	/// X tokens hit the 90 %-off cache"; `cache_creation_tokens`
	/// is "of the prompt, Y tokens got written to the cache at
	/// a 25 % surcharge". The panel's usage ring uses them
	/// purely for the tooltip; the compaction trigger still
	/// keys off `prompt_tokens` because that's what eats
	/// context-window space regardless of how it's billed.
	TokenUsage {
		prompt_tokens: u32,
		completion_tokens: u32,
		total_tokens: u32,
		context_window: u32,
		source: TokenUsageSource,
		#[serde(default)]
		cache_read_tokens: u32,
		#[serde(default)]
		cache_creation_tokens: u32,
	},

	/// Auto-compaction is starting. Fires before the fast-model
	/// summary call goes out; the panel renders a "compacting…"
	/// row and dims the ring while this is running.
	CompactionStarted {
		/// How many older messages will be replaced by the
		/// summary. The frontend renders this in the row's
		/// disclosure so the user knows what's getting folded.
		messages_compacted: u32,
	},

	/// Compaction finished. `summary` is the fast-model output
	/// that's now standing in for the old prefix; `prompt_tokens_after`
	/// is the next round-trip's expected prompt size (system
	/// prompt + summary + retained recent turns). The frontend
	/// flips the row from "compacting…" to a collapsible
	/// disclosure showing the summary.
	CompactionComplete { summary: String, prompt_tokens_after: u32 },
}

/// Where the token numbers in [`CoderEvent::TokenUsage`] came from.
/// `Provider` is exact (the OpenAI-compatible `usage` chunk);
/// `Estimate` is the bytes/4 fallback used when the provider
/// doesn't emit one. The frontend tints the ring identically but
/// adds a `≈` marker on the tooltip in the estimate case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenUsageSource {
	Provider,
	Estimate,
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
