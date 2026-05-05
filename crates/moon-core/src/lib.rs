//! moon-core — workspace brain.
//!
//! Hosts the `WorkspaceHost` trait + implementations and the workspace registry.
//! Linked into both the Tauri app (local mode) and `moon-remote` (the future
//! remote-host runtime). The in-process AI agent (`moon-coder`) drives this
//! same trait via the host of the active folder.
//!
//! See [specs/architecture.md](../../../specs/architecture.md).

pub mod app_state;
pub mod editorconfig;
pub mod format;
pub mod host;
pub mod lint_staged;
pub mod lsp;
pub mod pre_save;
pub mod search;
pub mod workspace;

pub use host::{LocalHost, WorkspaceHost};
pub use workspace::{WorkspaceFolderEntry, WorkspaceRegistry, DEFAULT_WORKSPACE_ID};

pub use moon_protocol as protocol;
