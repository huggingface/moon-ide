//! LSP multiplexer living inside `moon-core`.
//!
//! See [specs/lsp.md](../../../../specs/lsp.md) for the architecture
//! and `specs/architecture.md` for how this slot fits the overall
//! "nothing in the UI touches LSP directly" invariant.

pub mod broker;
pub mod client;
pub mod framing;
pub mod server;
pub mod spawn;
pub mod translate;

pub use broker::{LspBroker, LspEventRx};
pub use client::LspClientError;
pub use server::LspServerEvent;
pub use spawn::LspSpawner;
