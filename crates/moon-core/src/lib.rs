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
pub mod forge;
pub mod format;
pub mod host;
pub mod inbox;
pub mod lint_staged;
pub mod logs;
pub mod lsp;
pub mod pre_save;
pub mod pull_request;
pub mod search;
pub mod session;
pub mod shell;
pub mod workspace;
pub mod worktree;

#[cfg(test)]
pub mod test_util;

pub use host::{read_host_bytes, read_host_file, write_host_file, LocalHost, WorkspaceHost};
pub use logs::LogSink;
pub use shell::{AlwaysHostResolver, ShellResolver, ShellResolverHandle, ShellTarget};
pub use workspace::{WorkspaceFolderEntry, WorkspaceRegistry};
pub use worktree::{worktree_container_path, WORKTREES_DIR_NAME};

pub use moon_protocol as protocol;
