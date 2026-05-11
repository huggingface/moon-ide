//! Diagnostic logs.
//!
//! A simple keyed event bus used by everything that wants to surface
//! "here's what I'm doing" to the user without going through `tracing`
//! (which only lands in the launcher's terminal). The bottom-panel
//! logs view subscribes to this stream and per-source replays the
//! recent ring buffer when first opened.
//!
//! Keep this surface small on purpose — the goal is fast triage of
//! moon-ide internals when a feature looks broken (LSP went quiet,
//! Ctrl+S did nothing, fs-watcher mis-fired), not a full structured
//! logging story.
//!
//! Sources are free-form strings. The convention is
//! `<area>.<sub-area>` so the picker can group related entries:
//!
//! - `lsp.typescript` / `lsp.rust` / … — one per language server
//! - `format-on-save` — the save pipeline
//! - `editor.completion` — explicit-completion (Ctrl+Space) triage
//!
//! Anything else is acceptable; the panel just shows whatever the
//! backend has emitted into.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Severity tag attached to a [`LogEntry`]. Maps 1:1 to `tracing`'s
/// `Level` minus `TRACE`: noise that low never makes it into a
/// human-facing buffer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
	Debug,
	Info,
	Warn,
	Error,
}

/// One line of diagnostic log output. `seq` is a process-wide
/// monotonically-increasing counter so the frontend can tell
/// "appended" from "back-fill" when it merges the snapshot returned
/// by `logs_snapshot` with the live stream from the `logs:entry`
/// event.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
	/// Bucket key (see module docs). Free-form; the frontend uses
	/// it as both grouping key and tab title.
	pub source: String,
	pub level: LogLevel,
	pub message: String,
	/// Wall-clock unix epoch milliseconds at emit time.
	pub ts_ms: u64,
	/// Monotonic per-process counter starting at 1.
	pub seq: u64,
}
