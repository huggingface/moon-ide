//! moon-remote — headless moon-ide for remote machines.
//!
//! Two things live here:
//!
//! - **The library** (`rpc`, `relay`, `settings`): the companion RPC
//!   dispatcher and the outbound relay client (ADR 0031), shared by
//!   the desktop IDE (`src-tauri` links this crate) and the headless
//!   binary so the phone-facing surface is one implementation.
//! - **The binary** (`main.rs`): an enrolled IDE without a webview.
//!   Boots the coder + workspace registry on a remote machine, dials
//!   out to a `moon-bridge` relay over WSS, and serves the same
//!   coder+git surface the desktop serves — so a phone can drive
//!   sessions on that machine. The coder loop stays co-located with
//!   the filesystem (ADR 0031's invariant); the relay still only
//!   relays.
//!
//! Renamed from `moon-agent` per ADR 0011; re-chartered from the
//! stub "future RemoteHost server" to the headless enrolled IDE —
//! see the headless ADR. The RemoteHost (SSH/Codespaces WorkspaceHost
//! server) story remains future work and would live here too.

pub mod relay;
pub mod rpc;
pub mod settings;
