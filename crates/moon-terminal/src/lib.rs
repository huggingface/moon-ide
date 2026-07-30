//! PTY-backed terminal sessions, host-side and inside the
//! workspace shell.
//!
//! Architecture: [ADR 0009](../../../specs/decisions/0009-terminal-pty-and-targets.md).
//! Roadmap: [phase-03](../../../specs/roadmaps/phase-03-terminal.md).
//!
//! A [`TerminalTarget`] picks where the shell process runs:
//! either directly on the user's host, or inside the workspace
//! container via `docker exec`. Both go through the same
//! [`portable_pty`] master so the IPC layer doesn't need to
//! care.
//!
//! The crate is deliberately thin: it owns the spawn, the PTY
//! handles, and a [`TerminalRegistry`] of what's currently open
//! (so the coder's terminal-reading tools have something to ask).
//! The supervisor / event-pump / Tauri-emitting glue lives in
//! `src-tauri/src/commands/terminal.rs`, mirroring how
//! `moon-container`'s lifecycle is consumed by
//! `commands/container.rs`.

mod pty;
mod registry;
mod target;

pub use pty::{spawn, PtyError, PtySession};
pub use registry::{
	TerminalInfo, TerminalKind, TerminalRead, TerminalRegistration, TerminalRegistry, DEFAULT_READ_LINES, MAX_READ_LINES,
	SCROLLBACK_BYTES,
};
pub use target::{
	container_name_for_workspace, editor_forward_env_for_workspace, moon_edit_path_map_for_bound_folders, TerminalShell,
	TerminalTarget, MOON_EDIT_CONTAINER_SOCK,
};
