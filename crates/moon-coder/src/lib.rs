//! moon-coder — in-process AI coding agent for moon-ide.
//!
//! Owns the agent loop, the Hugging Face OAuth + inference client,
//! the tool registry, and the in-memory session. Driven by the Tauri
//! layer through [`Coder`]; emits structured events for the UI to
//! render the running turn.
//!
//! Phase 6.0 scope: device-flow auth + non-streaming chat completions,
//! read-only tools (`read_file`, `list_dir`, `grep`) plus a host-side
//! `bash`, and a single in-memory session. Streaming, mutating tools,
//! session persistence, and bucket sync land in 6.1+.
//!
//! See:
//! - [`specs/coder.md`](../../../specs/coder.md) — architectural spec
//! - [`specs/decisions/0010-coder-rewrite-not-acp.md`](../../../specs/decisions/0010-coder-rewrite-not-acp.md)
//! - [`specs/roadmaps/phase-06-coder.md`](../../../specs/roadmaps/phase-06-coder.md)

pub mod auth;
pub mod defaults;
pub mod error;
pub mod event;
pub mod inference;
pub mod runner;
pub mod tools;

pub use auth::{Authenticator, DeviceCode, HfIdentity, TokenStore};
pub use defaults::{DEFAULT_FAST_MODEL, DEFAULT_LARGE_MODEL, HF_OAUTH_CLIENT_ID, HF_OAUTH_SCOPES};
pub use error::CoderError;
pub use event::{CoderEvent, CoderStatus};
pub use inference::InferenceClient;
pub use runner::{Coder, CoderHandle};
pub use tools::ToolRegistry;
