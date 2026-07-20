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
//! Current methods:
//! - `coder_status` → [`CoderStatus`]
//! - `coder_list_sessions` → `Vec<SessionSummary>`
//! - `coder_active_session` → `Option<SessionSummary>`
//! - `workspace_snapshot` → the folder list + active folder (the
//!   phone's project switcher)
//! - `coder_open_session` / `coder_new_session` /
//!   `coder_delete_session` — session lifecycle, folder-targeted via
//!   an optional `folder` param so the phone drives any bound folder
//!   without touching the desktop's active-folder selection.
//! - `coder_send` / `coder_abort` — session-targeted via an optional
//!   `session_id` (the session the phone has open), falling back to
//!   the active folder's visible session.
//! - `coder_respond_to_prompt` — answer an `ask_user` tool call
//!   (Phase 14; the companion can now fully attend a coordinator
//!   session that raises a prompt).

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
	async fn dispatch(&self, method: &str, params: Value) -> Result<Value, String> {
		match method {
			"coder_status" => {
				let status = self.coder.status().await.map_err(|e| e.to_string())?;
				to_value(&status)
			}
			"coder_list_sessions" => {
				let p: FolderParams = parse_params(params)?;
				let sessions = self
					.coder
					.list_sessions_in(p.folder.as_deref())
					.await
					.map_err(|e| e.to_string())?;
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
			// --- Mutating: session commands. Folder-targeted — an
			// optional `folder` param (a bound folder's path from
			// `workspace_snapshot`) scopes the command to that
			// folder's session list, so the phone's project switcher
			// drives any folder without touching the desktop's
			// active-folder selection. Absent `folder` falls back to
			// the active folder.
			"coder_open_session" => {
				let p: OpenSessionParams = parse_params(params)?;
				// Observe-open: mounts the runtime and returns
				// `{ summary, events, in_flight }` — the replay rides
				// in this response instead of the event channel, and
				// the desktop's visible-session state is untouched, so
				// a phone opening a session doesn't switch the desktop
				// panel or light background-attention badges.
				let observed = self
					.coder
					.observe_session_in(p.folder.as_deref(), p.id)
					.await
					.map_err(|e| e.to_string())?;
				to_value(&observed)
			}
			"coder_send" => {
				let p: SendParams = parse_params(params)?;
				// Images aren't part of the phone composer yet.
				// `session_id` (the session the phone has open) routes
				// via `send_to` so the message can't land in whatever
				// session the desktop happens to have visible.
				match p.session_id {
					Some(sid) => self
						.coder
						.send_to(&sid, p.text, Vec::new())
						.await
						.map_err(|e| e.to_string())?,
					None => {
						self.coder.send(p.text, Vec::new()).await.map_err(|e| e.to_string())?;
					}
				}
				Ok(Value::Null)
			}
			"coder_abort" => {
				let p: AbortParams = parse_params(params)?;
				match p.session_id {
					Some(sid) => self.coder.abort_session(&sid).await,
					None => self.coder.abort().await,
				}
				Ok(Value::Null)
			}
			// --- Phase 14: the companion drives sessions fully
			// (new, delete, answer ask_user prompts). These mirror the
			// desktop's `#[tauri::command]`s 1:1 — same coder handle,
			// same PromptResponse type.
			"coder_new_session" => {
				let p: FolderParams = parse_params(params)?;
				let summary = self
					.coder
					.new_session_in(p.folder.as_deref())
					.await
					.map_err(|e| e.to_string())?;
				to_value(&summary)
			}
			"coder_delete_session" => {
				let p: DeleteSessionParams = parse_params(params)?;
				self
					.coder
					.delete_session_in(p.folder.as_deref(), p.id)
					.await
					.map_err(|e| e.to_string())?;
				Ok(Value::Null)
			}
			"coder_respond_to_prompt" => {
				let p: RespondToPromptParams = parse_params(params)?;
				let accepted = self.coder.respond_to_prompt(&p.call_id, p.response).await;
				Ok(serde_json::json!({ "accepted": accepted }))
			}
			"bridge_methods" => Ok(serde_json::json!({
				"methods": SUPPORTED_METHODS,
				"streams": SUPPORTED_STREAMS,
			})),
			other => Err(format!("unknown bridge rpc method `{other}`")),
		}
	}

	async fn subscribe(&self, method: &str, _params: Value) -> Result<tokio::sync::mpsc::Receiver<Value>, String> {
		if method != "coder_events" {
			return Err(format!("unknown bridge stream `{method}`"));
		}
		// Bridge the coder's broadcast channel to an mpsc of JSON the
		// focus listener can forward without knowing CoderEventEnvelope.
		// One forwarding task per subscriber; it ends when either the
		// broadcast closes or the mpsc receiver is dropped (client gone).
		let mut events = self.coder.subscribe();
		let (tx, rx) = tokio::sync::mpsc::channel::<Value>(256);
		tauri::async_runtime::spawn(async move {
			loop {
				match events.recv().await {
					Ok(envelope) => {
						let Ok(value) = serde_json::to_value(&envelope) else {
							continue;
						};
						if tx.send(value).await.is_err() {
							return; // client disconnected
						}
					}
					Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
					Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
				}
			}
		});
		Ok(rx)
	}
}

/// Optional folder target shared by folder-scoped methods
/// (`coder_list_sessions`, `coder_new_session`). `folder` is a bound
/// folder's path from `workspace_snapshot`; absent = active folder.
#[derive(serde::Deserialize)]
struct FolderParams {
	#[serde(default)]
	folder: Option<String>,
}

#[derive(serde::Deserialize)]
struct OpenSessionParams {
	id: String,
	#[serde(default)]
	folder: Option<String>,
}

#[derive(serde::Deserialize)]
struct SendParams {
	text: String,
	/// Session to send into (routes via `send_to`). Absent = the
	/// active folder's visible session.
	#[serde(default)]
	session_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct AbortParams {
	/// Session whose turn to abort. Absent = the active folder's
	/// visible session.
	#[serde(default)]
	session_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct DeleteSessionParams {
	id: String,
	#[serde(default)]
	folder: Option<String>,
}

#[derive(serde::Deserialize)]
struct RespondToPromptParams {
	call_id: String,
	response: moon_coder::PromptResponse,
}

/// Parse a method's params object, mapping a shape mismatch to an
/// error string the phone surfaces.
fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, String> {
	serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))
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
	"coder_open_session",
	"coder_send",
	"coder_abort",
	"coder_new_session",
	"coder_delete_session",
	"coder_respond_to_prompt",
	"bridge_methods",
];

/// Stream methods served via the `Subscribe` request kind (distinct
/// from the unary `SUPPORTED_METHODS`). Today: `coder_events`, the
/// workspace's `coder:event` broadcast.
pub const SUPPORTED_STREAMS: &[&str] = &["coder_events"];

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
