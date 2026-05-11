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
pub mod logs;
pub mod lsp;
pub mod next_edit;
pub mod next_edit_server;
pub mod pre_save;
pub mod search;
pub mod session;
pub mod shell;
pub mod workspace;

pub use host::{read_host_file, write_host_file, LocalHost, WorkspaceHost};
pub use logs::LogSink;
pub use shell::{AlwaysHostResolver, ShellResolver, ShellResolverHandle, ShellTarget};
pub use workspace::{WorkspaceFolderEntry, WorkspaceRegistry};

pub use moon_protocol as protocol;
pub use next_edit_server::NextEditServerSupervisor;
