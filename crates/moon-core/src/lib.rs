//! moon-core — workspace brain.
//!
//! Hosts the `WorkspaceHost` trait + implementations and the workspace registry.
//! Linked into both the Tauri app (local mode) and `moon-agent` (in-container mode).
//!
//! See [specs/architecture.md](../../../specs/architecture.md).

pub mod app_state;
pub mod editorconfig;
pub mod host;
pub mod pre_save;
pub mod search;
pub mod workspace;

pub use host::{LocalHost, WorkspaceHost};
pub use workspace::{Workspace, WorkspaceRegistry};

pub use moon_protocol as protocol;
