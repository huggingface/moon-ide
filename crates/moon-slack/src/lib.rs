//! moon-slack — Slack Web API client + token storage for the chat panel.
//!
//! Phase 11.0–11.1 surface: authenticate, scan the user's DMs for
//! bots, persist the `xoxp-` token in the OS keyring, list sessions
//! (top-level DM messages), and read a thread's messages. Sending
//! and polling join in 11.2+.
//!
//! See [`specs/slack-chat.md`](../../specs/slack-chat.md).
//!
//! ## Why a hand-rolled client
//!
//! `slack-morphism` and friends pull in a non-trivial dependency
//! cone (signing, OAuth flows, full type universe). We need a handful
//! of Web API methods. A few-hundred-line client beats any of them on
//! compile-time, audit cost, and change cost.

mod client;
mod error;
mod storage;

pub use client::{SlackClient, DM_SCAN_LIMIT, PREVIEW_MAX_CHARS, SESSION_HISTORY_LIMIT, THREAD_REPLY_LIMIT};
pub use error::SlackError;
pub use storage::TokenStore;
