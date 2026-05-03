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
//! The crate is deliberately thin: it owns the spawn + the
//! PTY handles, and that's it. The supervisor / event-pump /
//! Tauri-emitting glue lives in `src-tauri/src/commands/terminal.rs`,
//! mirroring how `moon-container`'s lifecycle is consumed by
//! `commands/container.rs`.

mod pty;
mod target;

pub use pty::{spawn, PtyError, PtySession};
pub use target::{container_name_for_workspace, TerminalShell, TerminalTarget};
