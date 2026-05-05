//! Wire types pushed from the loop to the UI.
//!
//! Mirrored 1:1 in `src/lib/protocol.ts`. A change here is a protocol
//! change — bump `moon_protocol::PROTOCOL_VERSION` if it's a breaking
//! shape edit.

use serde::{Deserialize, Serialize};

/// One push event the loop sends to the panel. Tagged enum so the
/// UI can `switch (event.kind)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoderEvent {
	/// The user's prompt is in the session and a turn just started.
	/// Carries the message verbatim so the UI can render it without
	/// echoing what it just sent.
	UserMessage { id: String, text: String },

	/// One assistant message landed. In 6.0 this fires once per
	/// completion (non-streaming); in 6.1 it'll fire as deltas
	/// roll in. `text` is the full content for the message — the
	/// UI replaces, not appends.
	AssistantMessage { id: String, text: String },

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
