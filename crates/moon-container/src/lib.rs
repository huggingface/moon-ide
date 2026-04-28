//! moon-container — Phase 2 workspace container plumbing.
//!
//! This crate owns the "is this workspace running in a moon-base
//! container?" lifecycle: compose-file discovery, project naming,
//! `.moon/compose.yaml` generation, and (in later commits) the
//! `docker compose` lifecycle commands the Tauri shell exposes
//! to the UI.
//!
//! The architectural backdrop is in
//! [`specs/containers.md`](../../../specs/containers.md); the
//! decision to use a host-shared Docker daemon with compose
//! `include:` (rather than nesting Docker inside the workspace
//! container) is [ADR 0008](../../../specs/decisions/0008-host-shared-daemon.md).
//!
//! This first slice of the crate is pure logic — no Docker
//! shell-out, no Tauri, no I/O beyond directory scans. The
//! orchestration that wires `docker compose up`/`pause`/etc.
//! lands on top in a follow-up commit.

pub mod compose;
pub mod discovery;
pub mod project;

pub use compose::{generate_compose, ComposeRender, ComposeRenderOptions};
pub use discovery::{discover_compose_files, ComposeDiscovery, DiscoveredCompose};
pub use project::{project_name_for, ProjectName};
