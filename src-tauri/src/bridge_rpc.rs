//! Bridge RPC handler — the workspace process's external method
//! surface for `moon-bridge` (Phase 13, mobile companion).
//!
//! The coder + git surface is otherwise reachable only as
//! `#[tauri::command]`s, i.e. from the webview inside this process.
//! `moon-bridge` runs in a *separate* process and reaches us over
//! the per-workspace `instance.sock` (ADR 0014). This handler is
//! what an `R\n<json>\n` request on that socket dispatches to (see
//! [`crate::focus_socket::BridgeRpcHandler`]).
//!
//! The method set is intentionally small and grows as the companion
//! PWA's screens need it. It is **not** a security boundary —
//! pairing is (a paired device can drive the coder, which can run
//! anything via `bash`; same threat model as the desktop, see
//! `specs/coder.md` § Permissions). It's a scope decision: only wire
//! up what something actually calls.
//!
//! Today's methods are all read-only snapshots, enough to prove the
//! relay end to end and back the PWA's workspace + session list:
//!
//! - `coder_status` → [`CoderStatus`]
//! - `coder_list_sessions` → `Vec<SessionSummary>`
//! - `coder_active_session` → `Option<SessionSummary>`
//! - `workspace_snapshot` → the folder list + active folder
//!
//! Mutating methods (`coder_send`, commit, …) land here when the
//! PWA wires the screen that calls them.

use std::sync::Arc;

use moon_coder::CoderHandle;
use moon_core::WorkspaceRegistry;
use serde_json::Value;

use crate::focus_socket::BridgeRpcHandler;

/// Concrete [`BridgeRpcHandler`] holding the handles the methods
/// dispatch against. One per process, built in `lib::run`'s setup
/// and handed to the focus listener.
pub struct BridgeRpc {
	coder: CoderHandle,
	workspaces: Arc<WorkspaceRegistry>,
}

impl BridgeRpc {
	pub fn new(coder: CoderHandle, workspaces: Arc<WorkspaceRegistry>) -> Self {
		Self { coder, workspaces }
	}
}

#[async_trait::async_trait]
impl BridgeRpcHandler for BridgeRpc {
	async fn dispatch(&self, method: &str, _params: Value) -> Result<Value, String> {
		match method {
			"coder_status" => {
				let status = self.coder.status().await.map_err(|e| e.to_string())?;
				to_value(&status)
			}
			"coder_list_sessions" => {
				let sessions = self.coder.list_sessions().await.map_err(|e| e.to_string())?;
				to_value(&sessions)
			}
			"coder_active_session" => {
				let active = self.coder.active_session().await;
				to_value(&active)
			}
			"workspace_snapshot" => {
				let snapshot = self.workspaces.snapshot().await;
				to_value(&snapshot)
			}
			"bridge_methods" => Ok(serde_json::json!({ "methods": SUPPORTED_METHODS })),
			other => Err(format!("unknown bridge rpc method `{other}`")),
		}
	}
}

/// Serialise a method result into the response's `ok` payload,
/// mapping any (unexpected) serialisation failure to an error
/// string so the dispatcher stays infallible at its boundary.
fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, String> {
	serde_json::to_value(value).map_err(|e| format!("failed to serialise rpc result: {e}"))
}

/// Methods this build serves. Exposed so a future `bridge_methods`
/// introspection call (and tests) can assert the set without
/// duplicating the match arms.
pub const SUPPORTED_METHODS: &[&str] = &[
	"coder_status",
	"coder_list_sessions",
	"coder_active_session",
	"workspace_snapshot",
	"bridge_methods",
];

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn supported_methods_are_unique() {
		let mut seen = std::collections::HashSet::new();
		for m in SUPPORTED_METHODS {
			assert!(seen.insert(*m), "duplicate method in SUPPORTED_METHODS: {m}");
		}
	}
}
