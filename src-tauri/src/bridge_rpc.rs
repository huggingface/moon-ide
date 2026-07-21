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
///
/// `app` reaches the Tauri-managed [`crate::state::AppState`] for
/// the model-settings methods, which share their bodies with the
/// desktop's `#[tauri::command]`s. It's captured before
/// `app.manage(state)` runs, so those methods resolve it lazily via
/// `try_state` — by the time anything dispatches (focus listener /
/// remote bridge, both spawned after setup) the state is managed.
pub struct BridgeRpc {
	coder: CoderHandle,
	workspaces: Arc<WorkspaceRegistry>,
	app: tauri::AppHandle,
}

impl BridgeRpc {
	pub fn new(coder: CoderHandle, workspaces: Arc<WorkspaceRegistry>, app: tauri::AppHandle) -> Self {
		Self { coder, workspaces, app }
	}

	/// The Tauri-managed [`AppState`], or an error string for the
	/// (should-be-impossible) dispatch-before-setup window.
	fn app_state(&self) -> Result<tauri::State<'_, crate::state::AppState>, String> {
		use tauri::Manager;
		self
			.app
			.try_state::<crate::state::AppState>()
			.ok_or_else(|| "app state not ready yet".to_owned())
	}

	/// Resolve a bound folder by path (when the phone passes one) or
	/// fall back to the desktop's active folder. Shared by the SCM
	/// methods, which need a `WorkspaceFolderEntry` to call host
	/// methods on — the same resolution `folder_session_or_active`
	/// does in the coder runner.
	async fn resolve_folder(
		&self,
		p: &FolderParams,
	) -> Result<std::sync::Arc<moon_core::workspace::WorkspaceFolderEntry>, String> {
		match &p.folder {
			Some(path) => self
				.workspaces
				.folder_for_path(path)
				.await
				.ok_or_else(|| format!("no bound folder at `{path}`")),
			None => self.workspaces.require_active_folder().await.map_err(|e| e.to_string()),
		}
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
			// Truncate a session to just before the `user_ordinal`-th
			// user message and return the dropped prompt text — the
			// phone's "edit & resend" / "replay" gesture. Session-
			// targeted; the desktop's visible session is untouched.
			// The phone re-opens the session afterwards to repaint.
			"coder_revert_to_message" => {
				let p: RevertParams = parse_params(params)?;
				let reverted = self
					.coder
					.revert_to_message_in(&p.session_id, p.user_ordinal)
					.await
					.map_err(|e| e.to_string())?;
				Ok(serde_json::json!({ "text": reverted.text }))
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
			// Create a coordinator session (ADR 0030) in the named
			// folder. Same observe-open semantics as `new_session`:
			// the runtime mounts but the desktop's visible-session
			// pointer is untouched. The phone can then send a goal
			// prompt via `coder_send` (session-targeted).
			"coder_new_coordinator_session" => {
				let p: FolderParams = parse_params(params)?;
				let summary = self
					.coder
					.new_coordinator_session_in(p.folder.as_deref())
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
			// --- Model / provider settings. Same bodies as the
			// desktop picker's commands, so a provider switch from
			// the phone applies + persists identically (runner poke,
			// per-workspace lock in `session.json`, global default in
			// `state.json`).
			"coder_get_model_settings" => {
				let state = self.app_state()?;
				let settings = crate::commands::coder::get_model_settings_impl(&state)
					.await
					.map_err(|e| e.to_string())?;
				to_value(&settings)
			}
			// Launch a sibling workspace process on this host. The
			// phone asks the bridge to open a stopped workspace; the
			// bridge forwards to the owning IDE (this method), which
			// runs the same "focus or spawn" path as the desktop's
			// `window_open` command. Local-carrier launches never
			// reach here — the bridge handles those directly.
			"workspace_launch" => {
				let p: WorkspaceLaunchParams = parse_params(params)?;
				let state = self.app_state()?;
				crate::commands::window::window_open_impl(state.inner(), p.workspace_id)
					.await
					.map_err(|e| e.to_string())?;
				Ok(Value::Null)
			}
			"coder_set_model_settings" => {
				let p: SetModelSettingsParams = parse_params(params)?;
				let state = self.app_state()?;
				crate::commands::coder::set_model_settings_impl(&state, p.settings)
					.await
					.map_err(|e| e.to_string())?;
				Ok(Value::Null)
			}
			// --- SCM (git) status + commit. Same host methods the
			// desktop's SCM panel uses, exposed folder-targeted so
			// the phone can inspect + commit any bound folder.
			"workspace_scm_status" => {
				let p: FolderParams = parse_params(params)?;
				let folder = self.resolve_folder(&p).await?;
				let branch = folder.host.git_branch().await.unwrap_or_default();
				let entries = folder.host.git_status_entries(&[]).await.unwrap_or_default();
				// Fold untracked → added, conflicted → modified
				// (same as `fs_git_change_summary` / the
				// coordinator's `workspace_scm_status` tool).
				let mut added = 0u32;
				let mut modified = 0u32;
				let mut deleted = 0u32;
				let mut files: Vec<Value> = Vec::new();
				for e in &entries {
					if matches!(e.status, moon_protocol::git::GitFileStatus::Ignored) {
						continue;
					}
					match e.status {
						moon_protocol::git::GitFileStatus::Added | moon_protocol::git::GitFileStatus::Untracked => added += 1,
						moon_protocol::git::GitFileStatus::Modified | moon_protocol::git::GitFileStatus::Conflicted => {
							modified += 1
						}
						moon_protocol::git::GitFileStatus::Deleted => deleted += 1,
						moon_protocol::git::GitFileStatus::Ignored => {}
					}
					files.push(serde_json::json!({
						"path": e.path,
						"status": format!("{:?}", e.status).to_lowercase(),
					}));
				}
				Ok(serde_json::json!({
					"branch": {
						"name": branch.name,
						"head_short_sha": branch.head_short_sha,
						"has_upstream": branch.has_upstream,
						"ahead": branch.ahead,
						"behind": branch.behind,
						"default_branch_remote_ref": branch.default_branch_remote_ref,
						"default_branch_behind": branch.default_branch_behind,
					},
					"changes": {
						"added": added,
						"modified": modified,
						"deleted": deleted,
						"total": added + modified + deleted,
					},
					"files": files,
				}))
			}
			"workspace_scm_commit" => {
				let p: ScmCommitParams = parse_params(params)?;
				let folder = self.resolve_folder(&FolderParams { folder: p.folder }).await?;
				// Auto-suggest when no message supplied — same
				// fast-model prompt as the desktop's sparkle button
				// and the coordinator's `commit_worker_changes`.
				let message = if p.message.trim().is_empty() {
					let diff = folder.host.git_diff_patch().await.unwrap_or_default();
					self
						.coder
						.suggest_commit_message("", &diff)
						.await
						.map_err(|e| e.to_string())?
				} else {
					p.message
				};
				let result = folder
					.host
					.git_commit(&message, p.amend.unwrap_or(false))
					.await
					.map_err(|e| e.to_string())?;
				to_value(&result)
			}
			"workspace_scm_suggest_message" => {
				let p: FolderParams = parse_params(params)?;
				let folder = self.resolve_folder(&p).await?;
				let diff = folder.host.git_diff_patch().await.unwrap_or_default();
				let suggestion = self
					.coder
					.suggest_commit_message("", &diff)
					.await
					.map_err(|e| e.to_string())?;
				Ok(serde_json::json!({ "message": suggestion }))
			}
			// --- SCM push / pull / fetch. Thin wrappers over the same
			// `WorkspaceHost` methods the desktop's SCM panel uses.
			// Each refreshes branch info after the op so the phone's
			// ahead/behind indicators update immediately.
			// Switch the folder's working tree to a local branch by
			// name — the phone's "back to main" gesture. Errors
			// (dirty tree, unknown branch) propagate git's stderr
			// verbatim, same as the desktop's branch switcher.
			"workspace_scm_switch_branch" => {
				let p: SwitchBranchParams = parse_params(params)?;
				let folder = self.resolve_folder(&FolderParams { folder: p.folder }).await?;
				folder
					.host
					.branch_switch(&moon_protocol::git::BranchSwitchTarget::Local { name: p.name })
					.await
					.map_err(|e| e.to_string())?;
				let branch = folder.host.git_branch().await.unwrap_or_default();
				to_value(&branch)
			}
			"workspace_scm_sync" => {
				let p: FolderParams = parse_params(params)?;
				let folder = self.resolve_folder(&p).await?;
				// Same logic as the desktop's `sync()`: if behind,
				// pull (rebase) first; if ahead (or diverged after
				// the pull), push. A diverged branch only pulls on
				// the first click — the user reviews the rebased
				// history before the next click pushes.
				let branch = folder.host.git_branch().await.unwrap_or_default();
				if branch.behind > 0 {
					folder.host.git_pull().await.map_err(|e| e.to_string())?;
				}
				let after_pull = folder.host.git_branch().await.unwrap_or_default();
				if after_pull.ahead > 0 || (branch.has_upstream && !branch.upstream_tracked) {
					folder.host.git_push().await.map_err(|e| e.to_string())?;
				}
				let final_branch = folder.host.git_branch().await.unwrap_or_default();
				to_value(&final_branch)
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

#[derive(serde::Deserialize)]
struct SetModelSettingsParams {
	settings: moon_protocol::coder_models::CoderModelSettings,
}

#[derive(serde::Deserialize)]
struct WorkspaceLaunchParams {
	workspace_id: String,
}

#[derive(serde::Deserialize)]
struct SwitchBranchParams {
	name: String,
	#[serde(default)]
	folder: Option<String>,
}

#[derive(serde::Deserialize)]
struct RevertParams {
	session_id: String,
	user_ordinal: usize,
}

#[derive(serde::Deserialize)]
struct ScmCommitParams {
	#[serde(default)]
	message: String,
	#[serde(default)]
	amend: Option<bool>,
	#[serde(default)]
	folder: Option<String>,
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
	"coder_new_coordinator_session",
	"coder_delete_session",
	"coder_respond_to_prompt",
	"coder_revert_to_message",
	"coder_get_model_settings",
	"coder_set_model_settings",
	"workspace_launch",
	"workspace_scm_status",
	"workspace_scm_commit",
	"workspace_scm_suggest_message",
	"workspace_scm_sync",
	"workspace_scm_switch_branch",
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
