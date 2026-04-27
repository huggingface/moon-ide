//! moon-slack — Slack Web API client + token storage for the chat panel.
//!
//! Phase 11.0 surface (foundation): authenticate, scan the user's DMs
//! for bots, persist the user's `xoxp-` token in the OS keyring.
//! Sessions / threads / sending messages join in 11.1+.
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

pub use client::{SlackClient, DM_SCAN_LIMIT};
pub use error::SlackError;
pub use storage::TokenStore;
