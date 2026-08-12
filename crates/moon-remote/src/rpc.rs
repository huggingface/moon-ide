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
//! - `coder_unqueue_steer` / `coder_drain_steer_now` — steer
//!   management, session-targeted by a required `session_id` (the
//!   desktop's `#[tauri::command]`s hit the visible session instead).
//! - `coder_respond_to_prompt` — answer an `ask_user` tool call
//!   (Phase 14; the companion can now fully attend a coordinator
//!   session that raises a prompt).

use std::sync::Arc;

use moon_coder::CoderHandle;
use moon_core::WorkspaceRegistry;
use serde_json::Value;

use crate::settings::SettingsContext;

/// Handler a relay/focus listener dispatches one method call or
/// stream subscription against. Implemented by [`BridgeRpc`]; kept a
/// trait so transports (the desktop's `instance.sock` focus listener,
/// the outbound relay client in [`crate::relay`]) stay decoupled from
/// the concrete dispatcher. Phase 13/14 (mobile companion).
#[async_trait::async_trait]
pub trait BridgeRpcHandler: Send + Sync {
	/// Dispatch one method call. Returns the result JSON or an error
	/// message the transport wraps into its own error frame.
	async fn dispatch(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String>;

	/// Subscribe to an event stream. Returns a receiver of JSON event
	/// payloads the transport forwards until the client disconnects,
	/// or an error string if the method isn't a known stream.
	async fn subscribe(
		&self,
		method: &str,
		params: serde_json::Value,
	) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>, String>;
}

/// Host-flavoured "launch a workspace" hook. The desktop spawns a
/// sibling `moon-ide --workspace <id>` (focus-or-spawn); the headless
/// binary spawns `moon-remote serve --workspace <id>`. Optional on
/// [`BridgeRpc`] — a host without a launcher answers the phone with
/// an error instead of silently ignoring the tap.
#[async_trait::async_trait]
pub trait WorkspaceLauncher: Send + Sync {
	async fn launch(&self, workspace_id: &str) -> Result<(), String>;
}

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
	settings: SettingsContext,
	launcher: Option<Arc<dyn WorkspaceLauncher>>,
	/// Per-folder `git fetch` throttle for the phone's SCM surface:
	/// a status request (fired on every project switch / manual
	/// refresh) triggers a *background* fetch at most once per
	/// [`FETCH_THROTTLE`], so ahead/behind counts track the remote
	/// without a fetch storm.
	last_fetch: tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

/// Minimum spacing between phone-triggered background fetches per
/// folder. Hardcoded (scope discipline): five minutes keeps counts
/// honest without hammering the remote from a bouncy phone.
const FETCH_THROTTLE: std::time::Duration = std::time::Duration::from_secs(300);

impl BridgeRpc {
	pub fn new(
		coder: CoderHandle,
		workspaces: Arc<WorkspaceRegistry>,
		settings: SettingsContext,
		launcher: Option<Arc<dyn WorkspaceLauncher>>,
	) -> Self {
		Self {
			coder,
			workspaces,
			settings,
			launcher,
			last_fetch: tokio::sync::Mutex::new(std::collections::HashMap::new()),
		}
	}

	/// Spawn a background `git fetch` for `entry` unless one ran in
	/// the last [`FETCH_THROTTLE`]. Fire-and-forget: the *next*
	/// status read shows the updated ahead/behind; failures
	/// (offline, auth) log at debug and cost nothing.
	async fn maybe_fetch(&self, entry: &Arc<moon_core::workspace::WorkspaceFolderEntry>) {
		let path = entry.folder.path.clone();
		{
			let mut last = self.last_fetch.lock().await;
			let due = last.get(&path).is_none_or(|at| at.elapsed() >= FETCH_THROTTLE);
			if !due {
				return;
			}
			last.insert(path.clone(), std::time::Instant::now());
		}
		let host = entry.host.clone();
		tokio::spawn(async move {
			if let Err(err) = host.git_fetch().await {
				tracing::debug!(error = %err, folder = %path, "background git fetch failed");
			}
		});
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
			"coder_running_sessions" => {
				let p: FolderParams = parse_params(params)?;
				// Seed for the phone's session-list "running" pip —
				// the pip is otherwise event-driven and misses
				// sessions already in flight at subscribe time.
				let running = self.coder.running_sessions_in(p.folder.as_deref()).await;
				to_value(&running)
			}
			"coder_active_session" => {
				let active = self.coder.active_session().await;
				to_value(&active)
			}
			"workspace_snapshot" => {
				let snapshot = self.workspaces.snapshot().await;
				to_value(&snapshot)
			}
			// Unbind a project folder from the workspace — the phone's
			// remove-project affordance. Shared state: this removes it
			// from the desktop's folder bar too (announced via
			// `WorkspaceFoldersChanged`). Files on disk are untouched.
			// Worktree folders are refused — their lifecycle belongs
			// to the worker discard flow (ADR 0044).
			"workspace_remove_folder" => {
				let p: FolderPathParams = parse_params(params)?;
				let entry = self
					.workspaces
					.folder_for_path(&p.folder)
					.await
					.ok_or_else(|| format!("no bound folder at `{}`", p.folder))?;
				if matches!(
					entry.folder.origin,
					moon_protocol::workspace::FolderOrigin::Worktree { .. }
				) {
					return Err("this is a worker worktree — discard it through the worker flow instead of unbinding".into());
				}
				self
					.workspaces
					.remove_folder(&p.folder)
					.await
					.map_err(|e| e.to_string())?;
				let snapshot = self.workspaces.snapshot().await;
				// Persist: the registry is in-memory; mirror the removal
				// (and the possibly-moved active pointer) into
				// `session.json` so the next boot doesn't re-bind it.
				if let Some(id) = self.settings.workspace_id.as_deref() {
					match moon_core::session::load(&self.settings.workspaces_dir, id).await {
						Ok(mut session) => {
							session.folders.retain(|f| {
								if f.folder_path == p.folder {
									return false;
								}
								!matches!(
									&f.origin,
									moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. }
										if parent_path == &p.folder
								)
							});
							session.active_folder_path = snapshot.active_folder.clone();
							if let Err(err) = moon_core::session::save(&self.settings.workspaces_dir, id, &session).await {
								tracing::warn!(?err, "failed to persist folder removal to session.json");
							}
						}
						Err(err) => {
							tracing::warn!(?err, "failed to load session.json for folder removal");
						}
					}
				}
				self.coder.announce_workspace_folders_changed(&p.folder);
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
					.observe_session_in(p.folder.as_deref(), p.id, p.max_events)
					.await
					.map_err(|e| e.to_string())?;
				to_value(&observed)
			}
			"coder_session_history_older" => {
				let p: SessionHistoryOlderParams = parse_params(params)?;
				// Upward pagination: the previous window's
				// `before_event_ordinal` is this call's exclusive
				// upper bound. Read-only — mounts nothing, emits
				// nothing on the desktop's event channel.
				let window = self
					.coder
					.session_history_older(p.folder.as_deref(), p.id, p.before_event_ordinal, p.max_events)
					.await
					.map_err(|e| e.to_string())?;
				to_value(&window)
			}
			"coder_send" => {
				let p: SendParams = parse_params(params)?;
				// Images aren't part of the phone composer yet.
				// `session_id` (the session the phone has open) routes
				// via `send_to_as_user` so the message can't land in
				// whatever session the desktop happens to have visible
				// — and, when the target is a coordinator's worker, so
				// the coordinator hears about it (ADR 0043) exactly as
				// it would for a desktop message.
				match p.session_id {
					Some(sid) => {
						self
							.coder
							.send_to_as_user(&sid, p.text, Vec::new())
							.await
							.map_err(|e| e.to_string())?;
					}
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
			// Re-run the round-trip that failed — the phone's retry
			// affordance on a trailing error. Session-targeted (the
			// phone never means "the desktop's visible session").
			"coder_retry_last_turn" => {
				let p: RetryParams = parse_params(params)?;
				self
					.coder
					.retry_last_turn_in(&p.session_id)
					.await
					.map_err(|e| e.to_string())?;
				Ok(Value::Null)
			}
			// Un-queue a still-queued steer (pop it back toward the
			// composer) or drive it now, session-targeted by id — the
			// session the phone has open, which needn't be the
			// desktop's visible one. Both resolve the runtime by id
			// across all folders (mirroring `coder_send`). The desktop
			// uses the visible-session variants; these are the
			// companion's.
			"coder_unqueue_steer" => {
				let p: SteerParams = parse_params(params)?;
				let popped = self.coder.unqueue_steer_in(&p.session_id, &p.id).await;
				Ok(serde_json::json!({ "text": popped.map(|s| s.text) }))
			}
			"coder_drain_steer_now" => {
				let p: SteerParams = parse_params(params)?;
				let drained = self.coder.drain_steer_now_in(&p.session_id, &p.id).await;
				Ok(serde_json::json!({ "drained": drained }))
			}
			"coder_rename_session" => {
				let p: RenameSessionParams = parse_params(params)?;
				// Persists + broadcasts; the desktop panel and any
				// observing phone pick the new title up off the event
				// channel, so the phone doesn't need to patch its own
				// copy.
				let summary = self
					.coder
					.rename_session_in(p.folder.as_deref(), p.id, p.title)
					.await
					.map_err(|e| e.to_string())?;
				to_value(&summary)
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
				let settings = crate::settings::get_model_settings(&self.coder, &self.settings)
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
				let Some(launcher) = &self.launcher else {
					return Err("this host cannot launch workspaces".to_owned());
				};
				launcher.launch(&p.workspace_id).await?;
				Ok(Value::Null)
			}
			// Probe a provider endpoint/key before committing — the
			// phone's add-provider form surfaces the upstream failure
			// verbatim ("401 Unauthorized", DNS, …).
			"coder_probe_provider" => {
				let p: ProbeProviderParams = parse_params(params)?;
				let key = if p.api_key.is_empty() {
					None
				} else {
					Some(p.api_key.as_str())
				};
				let result = self
					.coder
					.probe_provider(&p.base_url, p.kind, key)
					.await
					.map_err(|e| e.to_string())?;
				to_value(&result)
			}
			// Add a user provider (with API key) and make it available
			// to the picker — the phone's "add provider" flow. Returns
			// the new provider's id plus the refreshed settings.
			"coder_add_provider" => {
				let p: AddProviderParams = parse_params(params)?;
				let id = self.coder.new_provider_id();
				let config = moon_protocol::coder_models::CoderProviderConfig {
					id: id.clone(),
					label: p.label,
					kind: p.kind,
					base_url: p.base_url,
					standard_model: p.standard_model,
					cheap_model: p.cheap_model,
					has_api_key: false,
				};
				let settings = crate::settings::add_provider(&self.coder, &self.settings, config, &p.api_key)
					.await
					.map_err(|e| e.to_string())?;
				Ok(serde_json::json!({ "id": id, "settings": settings }))
			}
			"coder_set_model_settings" => {
				let p: SetModelSettingsParams = parse_params(params)?;
				crate::settings::set_model_settings(&self.coder, &self.settings, p.settings)
					.await
					.map_err(|e| e.to_string())?;
				Ok(Value::Null)
			}
			// --- SCM (git) status + commit. Same host methods the
			// desktop's SCM panel uses, exposed folder-targeted so
			// the phone can inspect + commit any bound folder.
			// Review a folder's work against the default branch: file
			// list vs merge-base plus the unified patch (committed +
			// uncommitted; untracked files excluded — same baseline
			// the desktop's "vs main" review tab uses). The phone
			// passes a worktree session's `worktree_root` as `folder`
			// to review an isolated agent's work against main.
			// `base_ref: null` when there's nothing to review against
			// (on the default branch, detached HEAD, no remote).
			// Working-tree diff (vs HEAD, untracked synthesised in,
			// 64 kB cap) — the phone's "view changes" overlay on the
			// SCM card. Read-only.
			"workspace_scm_diff" => {
				let p: FolderParams = parse_params(params)?;
				let folder = self.resolve_folder(&p).await?;
				let diff = folder.host.git_diff_patch().await.map_err(|e| e.to_string())?;
				Ok(serde_json::json!({ "diff": diff }))
			}
			"workspace_scm_review" => {
				let p: FolderParams = parse_params(params)?;
				let folder = self.resolve_folder(&p).await?;
				let Some(status) = folder.host.git_default_branch_diff().await.map_err(|e| e.to_string())? else {
					return Ok(serde_json::json!({ "base_ref": null }));
				};
				let paths: Vec<String> = status.entries.iter().map(|e| e.path.clone()).collect();
				let diff = if paths.is_empty() {
					String::new()
				} else {
					folder
						.host
						.git_diff_against(&status.merge_base, &paths)
						.await
						.map_err(|e| e.to_string())?
				};
				let files: Vec<Value> = status
					.entries
					.iter()
					.map(|e| {
						serde_json::json!({
							"path": e.path,
							"status": format!("{:?}", e.status).to_lowercase(),
						})
					})
					.collect();
				Ok(serde_json::json!({
					"base_ref": status.default_branch_ref,
					"merge_base": status.merge_base,
					"files": files,
					"diff": diff,
				}))
			}
			"workspace_scm_status" => {
				let p: FolderParams = parse_params(params)?;
				let folder = self.resolve_folder(&p).await?;
				// Throttled background fetch so ahead/behind track the
				// remote — a project switch on the phone is the natural
				// "am I current?" moment.
				self.maybe_fetch(&folder).await;
				let branch = folder.host.git_branch().await.unwrap_or_default();
				// Repo web base (e.g. https://github.com/owner/repo) for
				// the phone's `#123` transcript autolinks; absent when
				// the remote isn't a recognised host.
				let remote_url = folder.host.git_remote_web_url().await.ok().flatten();
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
					"remote_url": remote_url,
					"branch": {
						"name": branch.name,
						"head_short_sha": branch.head_short_sha,
						"has_upstream": branch.has_upstream,
						"ahead": branch.ahead,
						"behind": branch.behind,
						"default_branch_remote_ref": branch.default_branch_remote_ref,
						"default_branch_behind": branch.default_branch_behind,
						"previous_branch": branch.previous_branch,
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
				// Fetch first (inline, not throttled): the pull/push
				// decision below is only as good as the freshness of
				// the ahead/behind counts, and a sync click is an
				// explicit "talk to the remote" gesture.
				{
					let mut last = self.last_fetch.lock().await;
					last.insert(folder.folder.path.clone(), std::time::Instant::now());
				}
				if let Err(err) = folder.host.git_fetch().await {
					tracing::debug!(error = %err, "pre-sync git fetch failed; proceeding with cached counts");
				}
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
		tokio::spawn(async move {
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
struct ProbeProviderParams {
	base_url: String,
	#[serde(default)]
	api_key: String,
	#[serde(default)]
	kind: moon_protocol::coder_models::ProviderKind,
}

#[derive(serde::Deserialize)]
struct AddProviderParams {
	label: String,
	#[serde(default)]
	kind: moon_protocol::coder_models::ProviderKind,
	base_url: String,
	#[serde(default)]
	api_key: String,
	#[serde(default)]
	standard_model: String,
	#[serde(default)]
	cheap_model: String,
}

/// Params for methods that *require* a folder path (no active-folder
/// fallback — unbinding "whatever is active" from a phone would be
/// an accident magnet).
#[derive(serde::Deserialize)]
struct FolderPathParams {
	folder: String,
}

#[derive(serde::Deserialize)]
struct OpenSessionParams {
	id: String,
	#[serde(default)]
	folder: Option<String>,
	/// Window the replayed transcript to its newest `max_events`
	/// events. The companion always sets this so a very long session
	/// (or one with pasted images) doesn't ship its whole history
	/// over the phone's WS before the first row renders; absent =
	/// full replay (legacy callers).
	#[serde(default)]
	max_events: Option<usize>,
}

#[derive(serde::Deserialize)]
struct RenameSessionParams {
	id: String,
	title: String,
	#[serde(default)]
	folder: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionHistoryOlderParams {
	id: String,
	#[serde(default)]
	folder: Option<String>,
	/// Exclusive upper bound: replay the window ending just before
	/// this full-sequence ordinal (the `before_event_ordinal` the
	/// previous window's response carried).
	before_event_ordinal: usize,
	max_events: usize,
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
struct RetryParams {
	session_id: String,
}

#[derive(serde::Deserialize)]
struct SteerParams {
	/// Session holding the queued steer (the one the phone has
	/// open). Always required: unlike send/abort there's no
	/// meaningful "visible session" fallback for the phone.
	session_id: String,
	/// The steer's `UserMessage::id` (its placeholder row id).
	id: String,
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
	"coder_running_sessions",
	"coder_active_session",
	"workspace_snapshot",
	"workspace_remove_folder",
	"coder_open_session",
	"coder_session_history_older",
	"coder_rename_session",
	"coder_send",
	"coder_abort",
	"coder_retry_last_turn",
	"coder_unqueue_steer",
	"coder_drain_steer_now",
	"coder_new_session",
	"coder_new_coordinator_session",
	"coder_delete_session",
	"coder_respond_to_prompt",
	"coder_revert_to_message",
	"coder_get_model_settings",
	"coder_set_model_settings",
	"coder_probe_provider",
	"coder_add_provider",
	"workspace_launch",
	"workspace_scm_status",
	"workspace_scm_review",
	"workspace_scm_diff",
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
