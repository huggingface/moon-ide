//! moon-protocol — single source of truth for the JSON-RPC schema between
//! the UI, the local core, and the in-container agent.
//!
//! See [specs/protocol.md](../../specs/protocol.md).

pub mod app_state;
pub mod editorconfig;
pub mod error;
pub mod fs;
pub mod search;
pub mod session;
pub mod theme;
pub mod workspace;

pub use error::MoonError;

/// Protocol version. Bumped on breaking changes; UI and agent must match.
pub const PROTOCOL_VERSION: u32 = 0;

/// Result alias used everywhere the protocol surfaces errors.
pub type MoonResult<T> = std::result::Result<T, MoonError>;
