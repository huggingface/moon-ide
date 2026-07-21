//! Wire types pushed from the loop to the UI.
//!
//! Mirrored 1:1 in `src/lib/protocol.ts`. A change here is a protocol
//! change — bump `moon_protocol::PROTOCOL_VERSION` if it's a breaking
//! shape edit.

use serde::{Deserialize, Serialize};

/// Outer envelope carrying a (folder, session_id) tag alongside
/// the inner [`CoderEvent`]. Every event the runner emits goes
/// through this shape on the wire so the multi-session frontend
/// can route updates to the right per-(folder, session) UI bucket
/// — multiple sessions can run concurrently in the same folder
/// (see [ADR 0016](../../../specs/decisions/0016-coder-concurrent-sessions.md)),
/// so the folder alone isn't enough to disambiguate.
///
/// `folder` is the absolute path of the **session's bound folder**
/// (matches `WorkspaceFolder.path`), even for sub-agent events:
/// sub-agents belong to whichever project originated them, so a
/// `SubagentEvent` arrives tagged with the **parent's** folder
/// regardless of which folder the sub-agent's tools operate on
/// (`target_folder` lives inside `SubagentSpawned.target_folder`).
///
/// `session_id` is the session whose runtime emitted the event.
/// For sub-agent events that's the **parent's** session id (same
/// rationale as `folder`). A handful of event variants are
/// genuinely folder-scoped, not session-scoped
/// ([`CoderEvent::FolderSummaryReady`], [`CoderEvent::HubSyncStarted`],
/// [`CoderEvent::HubSyncFinished`]); those carry an empty string in
/// this field and the frontend routes them to the folder-level
/// handler rather than a specific session's bucket. Empty-string
/// sentinel rather than `Option<String>` keeps the wire shape
/// non-optional on the hot path and the TS mirror trivial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoderEventEnvelope {
	pub folder: String,
	#[serde(default)]
	pub session_id: String,
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
	///
	/// `queued: true` marks a steer that arrived while a turn was
	/// already running and is now sitting in the pending-steers
	/// queue, *not yet* in `session.messages`. The runner flips
	/// the state by emitting a matching [`SteerDrained`] event the
	/// moment the steer is moved into the chat at the top of the
	/// next iteration. The UI uses the flag to render the row in
	/// a muted "queued" style and to know whether
	/// `coder_unqueue_steer` can still pop it back into the
	/// composer.
	UserMessage {
		id: String,
		text: String,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		images: Vec<crate::inference::ImageAttachment>,
		#[serde(default, skip_serializing_if = "std::ops::Not::not")]
		queued: bool,
		/// Unix-ms creation time. Stamped `now` on a live turn and
		/// carried verbatim from the persisted record on replay, so
		/// a reopened session shows real per-message times. `None`
		/// only for pre-timestamp sessions; the panel then falls
		/// back to wall-clock receive time.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		created_at_ms: Option<i64>,
	},

	/// A previously-queued steer has been drained into the chat
	/// (or unqueued / aborted away — same effect from the UI's
	/// point of view). Carries the original [`UserMessage::id`] so
	/// the panel can flip the matching row out of "queued" mode.
	SteerDrained { id: String },

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
		/// Unix-ms creation time, same contract as
		/// [`UserMessage::created_at_ms`]: `now` live, persisted on
		/// replay. The panel pins it onto the assistant row so the
		/// header time survives a reopen.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		created_at_ms: Option<i64>,
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
		/// Wall-clock execution time in milliseconds, measured by
		/// the dispatcher around the tool run. Carried live *and*
		/// on replay (from the persisted record) so the panel's
		/// per-row duration survives a session reopen instead of
		/// collapsing to the replay's back-to-back event spacing.
		/// `None` for synthetic results (interrupted-tool
		/// sentinels) and records from before the field shipped.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		duration_ms: Option<u64>,
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
	/// render without a separate IPC round trip — including the
	/// ADR 0028 / ADR 0030 optional fields the header badges off
	/// (`worktree_branch`, `committed_branch`, `mode`), so a
	/// reopened worktree / coordinator session keeps its badge /
	/// chip / hint instead of losing them until the next list
	/// refresh. All three elide for an ordinary session, keeping
	/// the wire shape compact on the hot path.
	SessionLoaded {
		id: String,
		title: String,
		created_at_ms: i64,
		updated_at_ms: i64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		worktree_root: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		worktree_branch: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		committed_branch: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		mode: Option<String>,
	},

	/// A batch of events delivered as one envelope. Used by session
	/// replay (`open_session`): a long transcript fans out into
	/// hundreds-to-thousands of individual events, and Tauri
	/// delivers each as its own event-loop task on the frontend —
	/// reducing ~1 ms/event in pure IPC dispatch, which is seconds
	/// of jank on a large session. Replaying through a single
	/// `Replay` payload collapses that to one IPC crossing + one
	/// frontend reduce pass. The frontend unpacks `events` and
	/// applies each through the same per-event reducer a live turn
	/// uses, so ordering and semantics are identical. Not used for
	/// live turns — those stay one-event-per-emit so streaming
	/// deltas land as they're produced.
	///
	/// `in_flight` is `true` when the reopened session still has a
	/// turn streaming in the background (the user clicked into a
	/// running session and is about to back out again). The batch
	/// always ends with a `TurnComplete` terminator so a *settled*
	/// session's replayed `UserMessage` events don't leave a phantom
	/// Stop button — but that terminator also clears the busy pip,
	/// which would drop the sessions-list "running" badge on a
	/// session whose turn is genuinely still running. The frontend
	/// re-asserts the pip from this flag after applying the batch.
	Replay { events: Vec<CoderEvent>, in_flight: bool },

	/// The active session's title was rewritten — either by the
	/// auto-rename pass after the first turn, or (Phase 6.4+) by
	/// an explicit user rename. Frontend updates the sticky
	/// header + the row in the sessions list. The new title is
	/// also persisted as a
	/// [`crate::sessions::SessionRecord::TitleUpdate`] on disk so
	/// re-opening sees it.
	SessionTitleUpdated { id: String, title: String },

	/// A session's worktree routing was cleared — its `worktree_root`
	/// and `worktree_branch` were set to `None` (e.g. after merging
	/// the worktree's branch and removing the checkout). The frontend
	/// patches `activeSession` + the sessions-list row without a full
	/// reload, mirroring `SessionTitleUpdated`. No worktree fields
	/// means the session now drives its parent folder's main tree.
	SessionWorktreeCleared { id: String },

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
		/// Set when the spawned entity is a coordinator worker
		/// (ADR 0030 `spawn_worker`) — carries the worktree folder
		/// path so the frontend can navigate to the worker's
		/// session (a real top-level session) instead of the
		/// sub-agent pop-out. Absent for `task` sub-agents.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		worktree_root: Option<String>,
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

	/// Progress heartbeat while the compaction summary is being
	/// written. `summary_tokens` is the estimated token count
	/// (bytes/4) of summary text the model has streamed out so
	/// far, cumulative across the pass's calls — the common
	/// single-chunk case still gets a visibly moving number.
	/// `chunks_done` / `chunks_total` cover the chunked case (the
	/// prefix is split so each summary call fits the model's
	/// window): emitted once right after chunking (`0/N`) and
	/// again as each chunk settles; `chunks_done == chunks_total`
	/// on a multi-chunk run means the final merge pass is in
	/// flight. A re-chunking recursion (partials still too big)
	/// restarts the counters with a new total. Not persisted /
	/// replayed — purely a live-progress signal for the running
	/// transcript row.
	CompactionProgress {
		chunks_done: u32,
		chunks_total: u32,
		summary_tokens: u32,
	},

	/// Compaction finished. `summary` is the fast-model output
	/// that's now standing in for the old prefix; `prompt_tokens_after`
	/// is the next round-trip's expected prompt size (system
	/// prompt + summary + retained recent turns). The frontend
	/// flips the row from "compacting…" to a collapsible
	/// disclosure showing the summary.
	CompactionComplete { summary: String, prompt_tokens_after: u32 },

	/// A push to the workspace's HF Hub bucket has started for
	/// `session_id`. The frontend flips the matching session
	/// row's cloud icon into the "syncing" state (spinning
	/// ring); a matching [`HubSyncFinished`] event flips it back
	/// to idle (`ok: true`) or failed (`ok: false`). Per-folder
	/// via [`CoderEventEnvelope`] — the row decoration lives in
	/// the session list, which itself is per-folder.
	HubSyncStarted { session_id: String },

	/// The push that started with the matching
	/// [`HubSyncStarted`] is done. `ok` is `false` on any
	/// failure (token refresh failed, Xet rejected, batch 4xx,
	/// disk I/O); `error` carries the displayable reason for
	/// the tooltip when `ok` is `false`.
	HubSyncFinished {
		session_id: String,
		ok: bool,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		error: Option<String>,
	},
	/// Per-turn working-tree diff (ADR 0030). Emitted alongside
	/// [`CoderEvent::TurnComplete`] when the agent's tools changed
	/// files during the turn. `files` is the format-queue's file
	/// set (the files `write_file` / `edit_file` touched); `diff`
	/// is the unified diff against the baseline SHA captured at turn
	/// start. Empty `diff` when nothing changed. Any client can
	/// render this — the desktop panel as a collapsible diff row,
	/// the companion as a compact summary, an orchestrator as a
	/// dispatch-packet artifact via `observe_worker`.
	TurnDiff { files: Vec<String>, diff: String },
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
	/// Where the `bash` tool will run for the active folder's
	/// **visible session** — `"host"` or `"container"`. Reflects
	/// that session's per-session force-host override (so a forced
	/// session reads `"host"` even with the container running).
	/// `None` when no folder is active (the panel still works for
	/// chat without a folder; tool calls just fail with
	/// `NoActiveFolder`). Mirrors the `target` field emitted in
	/// `bash` tool results.
	pub bash_target: Option<String>,
	/// `true` when the active folder's visible session has the
	/// force-host override engaged. Distinct from
	/// `bash_target == "host"`: a session can resolve to host
	/// simply because the container is down (auto), which is *not*
	/// an override. The panel uses this to render the "off-default"
	/// badge on the target pip and pre-select the radio in the
	/// popover.
	pub force_host_override: bool,
}
