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

pub mod anthropic;
pub mod auth;
pub mod compaction;
pub mod defaults;
pub mod error;
pub mod event;
pub mod folder_summary;
pub mod hub_sync;
pub mod inference;
pub mod models;
pub mod prompts;
pub mod providers;
pub mod runner;
pub mod sessions;
pub mod subagent;
pub mod todo;
pub mod tools;
pub mod web;

pub use auth::{Authenticator, DeviceCode, HfIdentity, HfOrg, TokenStore};
pub use defaults::{DEFAULT_CHEAP_MODEL, DEFAULT_STANDARD_MODEL, HF_OAUTH_CLIENT_ID, HF_OAUTH_SCOPES};
pub use error::CoderError;
pub use event::{CoderEvent, CoderEventEnvelope, CoderStatus};
pub use folder_summary::{FolderSummary, FolderSummaryService};
pub use inference::{ImageAttachment, InferenceClient};
pub use models::CoderModels;
pub use prompts::{PromptOutcome, PromptResponse, QuestionAnswer};
pub use providers::{new_provider_id, probe_provider, ProviderKeyring};
pub use runner::{Coder, CoderHandle, RerunToolOutcome, RevertedMessage, TerminalCommandContext, UnqueuedSteer};
pub use sessions::SessionSummary;
pub use subagent::{Subagent, SubagentReport};
pub use todo::{merge_todos, TodoItem, TodoStatus};
pub use tools::{CoderMode, ToolContext, ToolRegistry};
pub use web::{WebClient, WebFetchResult, WebSearchResult};
