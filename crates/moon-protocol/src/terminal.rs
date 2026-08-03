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
///
/// Process-per-workspace: there's no `workspace_id` field on
/// `Container` because each process is pinned to one
/// workspace. The Tauri command derives the
/// `moon-ws-<id>-dev-1` container name from
/// `state.workspace_id()`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TerminalTarget {
	Host {
		cwd: Option<String>,
	},
	Container {
		/// In-container working directory. Required.
		cwd: String,
	},
}

/// Why a terminal session ended. Classified by the supervisor
/// at close time so the frontend can tell "the user exited the
/// shell" (Ctrl+D, `exit` — code 0) from "the environment went
/// away" (container stopped mid-session, `docker exec` refused
/// because the container wasn't running yet). Drives the
/// auto-close / auto-respawn policy in `terminal.svelte.ts`:
/// shell exits close the tab, container losses keep it and
/// offer to respawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum TerminalCloseReason {
	/// The host shell process itself exited (any code — Ctrl+D
	/// and `exit` both land here with code 0).
	ShellExited,
	/// The `docker exec` child exited, and the workspace
	/// container was still running afterwards — so the exit came
	/// from the in-container shell (or command), not from the
	/// environment going away.
	ContainerShellExited,
	/// The `docker exec` child exited and the workspace container
	/// is *not* running any more (user stopped it, Recreate,
	/// crash) — the terminal died because its environment did.
	ContainerStopped,
	/// `docker exec` never started the remote process (container
	/// still booting after an IDE relaunch / recreate, name not
	/// found, daemon unreachable). Same UX treatment as
	/// `ContainerStopped`: keep the tab, respawn when the
	/// container is back.
	ContainerNotRunning,
	/// portable-pty couldn't translate the exit (supervisor
	/// cancelled, signal it can't map).
	Unknown,
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
	/// Command to prefill at the fresh shell's prompt right after
	/// spawn — typed in as if the user had typed it, but NOT
	/// executed (no trailing newline is sent; the user reviews it
	/// and presses Enter). Used by "restart" on an exited tab and
	/// by session replay on IDE launch — the frontend seeds it
	/// from the shell-history line it captured when the terminal
	/// was last used. Once the user runs it, the line lands in
	/// the shell's own history and up-arrow keeps walking the
	/// same session.
	#[serde(default)]
	pub command: Option<String>,
	/// Absolute **host** path of the bound folder this terminal is
	/// being opened for — the project the user was in when they hit
	/// `+ Terminal`. Recorded in
	/// [`moon_terminal::TerminalRegistry`] so the coder's
	/// `list_terminals` / `read_terminal` tools can scope
	/// themselves to the session's own project or worktree
	/// (ADR 0048).
	///
	/// Passed explicitly rather than derived from `cwd`: a worktree
	/// rides its parent's bind mount, so its container `cwd` is a
	/// path *under* the parent's folder, and any user `cd` makes
	/// the shell's live cwd a lie about which project the terminal
	/// belongs to. `None` for a terminal with no project behind it
	/// (a `$HOME` shell in a folder-less workspace); those never
	/// match a folder-scoped listing.
	#[serde(default)]
	pub folder: Option<String>,
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
/// frontend reacts per [`TerminalCloseReason`]: shell exits
/// (the user's own Ctrl+D / `exit`) close the tab, environment
/// losses keep it and offer to respawn. Subsequent
/// `terminal_close` calls for this id are no-ops.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TerminalClosed {
	pub stream_id: String,
	/// Process exit code if portable-pty surfaced one.
	/// `None` for signals it couldn't translate or for
	/// supervisor-cancelled streams.
	pub code: Option<i32>,
	pub reason: TerminalCloseReason,
}
