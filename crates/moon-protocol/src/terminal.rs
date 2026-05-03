//! Terminal session shapes for the Tauri commands and events.
//!
//! See [ADR 0009](../../../specs/decisions/0009-terminal-pty-and-targets.md)
//! and [phase-03-terminal.md](../../../specs/roadmaps/phase-03-terminal.md).
//!
//! The wire format is intentionally tiny: one open call, three
//! mutators (write / resize / close), two events. Bytes
//! crossing the IPC boundary are base64 because Tauri's payload
//! codec is JSON and PTY output is arbitrary 8-bit (escape
//! sequences, partial UTF-8 codepoints split across reads).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where a terminal's shell process runs. Matches
/// `moon_terminal::TerminalTarget` 1:1 in shape; we keep this
/// copy in `moon-protocol` to avoid leaking that crate's
/// internals through `ts-rs` bindings.
///
/// `Host` shells start in `cwd` (or the user's `$HOME` if
/// `cwd` is `None`). `Container` shells start in the
/// in-container path under `/workspace/<basename>` for the
/// active folder, picked by the frontend at open time so the
/// backend doesn't have to know about workspace layout.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TerminalTarget {
	Host {
		cwd: Option<String>,
	},
	Container {
		/// Workspace id (`default` until multi-workspace
		/// ships). The Tauri command derives the actual
		/// `moon-ws-<id>-dev-1` container name from this.
		workspace_id: String,
		/// In-container working directory. Required.
		cwd: String,
	},
}

/// Open request payload for `terminal_open`. Cols/rows match
/// xterm.js's initial fit; the supervisor sends them straight
/// through to `PtySize` on the backend.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TerminalOpenRequest {
	pub target: TerminalTarget,
	pub cols: u16,
	pub rows: u16,
}

/// One chunk of terminal output. `data` is base64-encoded
/// bytes — feed straight into xterm.js's `write` after
/// decoding. Keyed on `stream_id` so multiple terminals don't
/// interleave (each tab subscribes to the bus and filters by
/// id). Emitted on the `terminal:output` Tauri event.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TerminalOutput {
	pub stream_id: String,
	/// Base64-encoded raw bytes from the PTY master.
	pub data: String,
}

/// Final event for a terminal session, emitted exactly once
/// on `terminal:closed` when the underlying child exits. The
/// frontend marks the tab as no-longer-streaming and disables
/// input on receipt; subsequent `terminal_close` calls for
/// this id are no-ops.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TerminalClosed {
	pub stream_id: String,
	/// Process exit code if portable-pty surfaced one.
	/// `None` for signals it couldn't translate or for
	/// supervisor-cancelled streams.
	pub code: Option<i32>,
}
