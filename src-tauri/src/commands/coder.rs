//! Tauri commands wrapping `moon-coder`.
//!
//! Phase 6.0 surface: device-flow sign-in, status probe, sign-out,
//! one-shot `send`, mid-turn `abort`. Loop events stream out on the
//! `coder:event` Tauri channel. See
//! `specs/test-plans/0039-coder-skeleton.md`.

use camino::Utf8PathBuf;
use moon_coder::{
	CoderHandle, CoderStatus, DeviceCode, HfIdentity, ImageAttachment, PromptResponse, RerunToolOutcome, RevertedMessage,
	SessionSummary, UnqueuedSteer,
};
use moon_core::app_state as app_state_store;
use moon_core::session as core_session;
use moon_protocol::coder_hub::{CoderHubBucket, HubNamespace, HubUploadAllSummary};
use moon_protocol::coder_models::{
	CoderModelSettings, CoderProviderConfig, CoderProviderLock, ProviderModelSummary, ProviderProbeResult, RouterModel,
};
use moon_protocol::MoonError;
use serde::Deserialize;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Channel name the loop's events are emitted on. The frontend
/// listens via `getCurrent().listen('coder:event', ...)`. Mirrored in
/// `src/lib/coder.svelte.ts`.
pub const CODER_EVENT_CHANNEL: &str = "coder:event";

/// Spawn the long-running task that re-broadcasts the coder's
/// in-process broadcast channel onto Tauri's event bus. Called once
/// at app startup; the task lives for the entire process lifetime.
pub fn spawn_event_pump(app: AppHandle, coder: CoderHandle) {
	let mut rx = coder.subscribe();
	tauri::async_runtime::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(event) => {
					if let Err(err) = app.emit(CODER_EVENT_CHANNEL, &event) {
						tracing::warn!(error = %err, "failed to emit coder event");
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					// Rare, since we sized the channel generously.
					// Logged so a flood is visible without crashing the
					// pump — the frontend resyncs from `coder_status`
					// on its next mount.
					tracing::warn!(missed = n, "coder event pump lagged");
				}
				Err(tokio::sync::broadcast::error::RecvError::Closed) => {
					tracing::info!("coder event channel closed; pump exiting");
					break;
				}
			}
		}
	});
}

/// Snapshot the coder's auth + busy state. Polled by the panel on
/// mount so reopens land in the right shape.
#[tauri::command]
pub async fn coder_status(state: State<'_, AppState>) -> Result<CoderStatus, MoonError> {
	state.coder.status().await.map_err(MoonError::from)
}

/// Fetch the cached "Bound folders" description for `folder`
/// (absolute path matching `WorkspaceFolder.path`). Returns
/// `None` when the cache is cold or stale — the runner kicks off
/// regeneration on its next turn, and a `folder_summary_ready`
/// event will fire when it finishes. Used by the project bar
/// tooltip and sub-agent picker preview.
#[tauri::command]
pub async fn coder_folder_summary(state: State<'_, AppState>, folder: String) -> Result<Option<String>, MoonError> {
	Ok(state.coder.folder_summary(&folder).await)
}

/// Kick off the HF device flow. Returns the user/device code pair
/// immediately. The frontend opens `verification_uri_complete` in
/// the system browser then calls [`coder_poll_device_code`] to wait
/// for the consent screen.
#[tauri::command]
pub async fn coder_start_device_flow(state: State<'_, AppState>) -> Result<DeviceCode, MoonError> {
	state.coder.start_device_flow().await.map_err(MoonError::from)
}

/// Poll the token endpoint until the user approves / denies. Returns
/// the freshly-fetched [`HfIdentity`] on success. The future blocks
/// until completion; the frontend awaits with the modal still open.
#[tauri::command]
pub async fn coder_poll_device_code(state: State<'_, AppState>, code: DeviceCode) -> Result<HfIdentity, MoonError> {
	state.coder.poll_device_code(code).await.map_err(MoonError::from)
}

/// Drop the keyring entry + the in-memory session. Idempotent.
#[tauri::command]
pub async fn coder_sign_out(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.coder.sign_out().await.map_err(MoonError::from)
}

/// Send one user message and start a turn. Non-blocking — the future
/// resolves once the turn has been spawned, then events stream over
/// the `coder:event` channel. Errors here mean the turn never
/// started (no auth, already-running turn, etc.).
///
/// Pins `last_session_by_folder[active]` to the active session after
/// a successful send. Two reasons:
///
/// 1. `new_session` doesn't persist a pointer — empty sessions
///    aren't on disk yet, so there's no id worth remembering until
///    the first record lands. Without this nudge, "create new
///    session → send → quit" leaves the pointer on whichever
///    previous session the user had open, and the next launch
///    hydrates the wrong transcript.
/// 2. If the previous pointer was stale (session deleted out-of-
///    band), `persist_last_session` here overwrites it. Combined
///    with the silent fall-through in `coder.svelte.ts`'
///    `#hydrateSession`, that turns the "host error: Aucun
///    fichier ou dossier de ce nom" stuck-error-row failure mode
///    into a self-healing one — the first send after a relaunch
///    refreshes the pointer for next time.
///
/// `persist_last_session` is a cheap no-op when the pointer
/// already matches, so we don't bother gating it on "is the
/// pointer stale".
#[tauri::command]
pub async fn coder_send(
	state: State<'_, AppState>,
	text: String,
	images: Vec<ImageAttachment>,
) -> Result<(), MoonError> {
	state.coder.send(text, images).await.map_err(MoonError::from)?;
	let folder = active_folder_path(&state).await;
	if let Some(folder) = folder {
		if let Some(summary) = state.coder.active_session().await {
			persist_last_session(&state.config_dir, &folder, Some(summary.id)).await;
		}
	}
	Ok(())
}

/// Suggest a kebab-cased branch name based on the user's draft
/// commit message and the active folder's `git diff HEAD --stat`
/// summary. Used by the SCM panel's "Commit to new branch…" form
/// to populate the branch input. Falls back to an error string
/// the panel surfaces verbatim when the model is unreachable —
/// the user can still type a name manually.
#[tauri::command]
pub async fn coder_suggest_branch_name(state: State<'_, AppState>, message: String) -> Result<String, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let diff = entry.host.git_diff_summary().await.unwrap_or_default();
	state
		.coder
		.suggest_branch_name(&message, &diff)
		.await
		.map_err(MoonError::from)
}

/// Suggest a one-line commit subject from the active folder's
/// `git diff HEAD` patch (capped to ~64 KB upstream) and whatever
/// the user has already typed in the composer. Used by the SCM
/// panel's sparkle button inset on the commit textarea. Errors
/// surface verbatim through the panel as a flash toast — the user
/// can still type the message manually.
#[tauri::command]
pub async fn coder_suggest_commit_message(state: State<'_, AppState>, message: String) -> Result<String, MoonError> {
	let entry = state.workspaces.require_active_folder().await?;
	let diff = entry.host.git_diff_patch().await.unwrap_or_default();
	state
		.coder
		.suggest_commit_message(&message, &diff)
		.await
		.map_err(MoonError::from)
}

/// Translate a natural-language `request` into a single shell
/// command for a terminal's `Ctrl+K` prompt. `target_kind` is the
/// terminal's shell (`"host"` / `"container"`) and `cwd` its
/// working directory — passed from the frontend since the
/// terminal supervisor, not the workspace, owns that state. We
/// fold in the active folder's git branch + local branch names so
/// the model can resolve references like "the feat branch" or
/// "rebase onto main". The result is prefilled into the PTY line
/// (not executed); the user reviews it and presses Enter. Errors
/// surface verbatim through a terminal flash — the user can still
/// type the command manually.
#[tauri::command]
pub async fn coder_suggest_terminal_command(
	state: State<'_, AppState>,
	request: String,
	target_kind: String,
	cwd: String,
) -> Result<String, MoonError> {
	let mut ctx = moon_coder::TerminalCommandContext {
		shell_kind: target_kind,
		cwd,
		..Default::default()
	};
	// Git context is best-effort: a non-git folder (or no active
	// folder at all) just means the model gets less to work with,
	// not a hard error — a Ctrl+K request might be a plain `ls`.
	if let Ok(entry) = state.workspaces.require_active_folder().await {
		if let Ok(branch) = entry.host.git_branch().await {
			ctx.git_branch = branch.name.unwrap_or_default();
		}
		if let Ok(branches) = entry.host.git_local_branches().await {
			ctx.git_branches = branches.join("\n");
		}
	}
	state
		.coder
		.suggest_terminal_command(&request, &ctx)
		.await
		.map_err(MoonError::from)
}

/// Cancel the **active folder's** running turn, if any.
/// Background turns running in other folders are left alone — the
/// user has to switch to them and stop manually if they want
/// (per the multi-session "agents keep running per project"
/// contract). Async because resolving the active folder + its
/// `FolderSession` map entry needs `await`.
#[tauri::command]
pub async fn coder_abort(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.coder.abort().await;
	Ok(())
}

/// "Go now" on a previously-queued steer — the button on the
/// muted "queued" row in the transcript. Cancels the running turn
/// and lets the spawn loop drain the steer into a fresh turn.
/// Returns `false` (no-op) when `id` no longer matches a queued
/// steer — the runner already drained it at its last iteration
/// top.
#[tauri::command]
pub async fn coder_drain_steer_now(state: State<'_, AppState>, id: String) -> Result<bool, MoonError> {
	Ok(state.coder.drain_steer_now(&id).await)
}

/// Pop a previously-queued steer (a `send` issued while a turn
/// was already running, sitting in `pending_steers`) by id and
/// return its `text` + `images` so the panel can repopulate the
/// composer. Returns `None` when the id no longer matches a
/// queued steer — by the time the user pressed `Up`, the runner
/// drained the queue at the top of the next iteration. Bound to
/// `ArrowUp` on an empty composer in the panel.
#[tauri::command]
pub async fn coder_unqueue_steer(state: State<'_, AppState>, id: String) -> Result<Option<UnqueuedSteer>, MoonError> {
	Ok(state.coder.unqueue_steer(&id).await)
}

/// Resolve an in-flight `ask_user` prompt on the active folder's
/// visible session with the user's structured per-question answers.
/// `call_id` is the tool-call id the panel saw on the prompt's
/// `tool_call` event. Returns `false` (no-op) when there's no
/// matching parked prompt — the user already skipped it by sending a
/// composer message, the turn aborted, or the id is stale. Either
/// way the row settles off the `tool_result` event. The user can
/// always skip the prompt entirely by just sending a normal message
/// instead of answering — that path runs through `coder_send`.
#[tauri::command]
pub async fn coder_respond_to_prompt(
	state: State<'_, AppState>,
	call_id: String,
	response: PromptResponse,
) -> Result<bool, MoonError> {
	Ok(state.coder.respond_to_prompt(&call_id, response).await)
}

/// Revert the active folder's visible session to just before its
/// `user_ordinal`-th user message (0-based, in transcript order),
/// dropping that message and everything after it from both disk
/// and the in-memory chat history. Returns the dropped prompt so
/// the panel's "edit & resend" can prefill the composer; a plain
/// "revert to here" ignores the payload. The backend re-emits the
/// trimmed transcript on the `coder:event` channel via the
/// `open_session` reload path, so the panel rebuilds its rows from
/// those events rather than from the return value. Refuses while a
/// turn is in flight.
#[tauri::command]
pub async fn coder_revert_to_message(
	state: State<'_, AppState>,
	user_ordinal: usize,
) -> Result<RevertedMessage, MoonError> {
	state
		.coder
		.revert_to_message(user_ordinal)
		.await
		.map_err(MoonError::from)
}

/// Replay the active folder's visible session from its
/// `user_ordinal`-th user message (0-based, transcript order):
/// truncate to just before that message (the same rewrite as
/// `coder_revert_to_message`) and immediately re-send the dropped
/// prompt verbatim. The "re-run this turn" gesture — edit-and-resend
/// without the composer round-trip. Auth-gates before the truncation
/// so a signed-out replay fails clean without rewriting the JSONL.
/// Refused while a turn is in flight. The trimmed transcript replays
/// on `coder:event` (via the `coder_revert_to_message` reload), then
/// the new turn's events stream as a normal send.
#[tauri::command]
pub async fn coder_replay_from_message(state: State<'_, AppState>, user_ordinal: usize) -> Result<(), MoonError> {
	state
		.coder
		.replay_from_message(user_ordinal)
		.await
		.map_err(MoonError::from)
}

/// Resume the active folder's visible session from its
/// `assistant_ordinal`-th assistant message with tool calls (0-based,
/// among assistant records that have non-empty `tool_calls`):
/// truncate the session to keep everything up to and including that
/// assistant message, drop its tool results and everything after,
/// then re-dispatch the kept assistant's tool calls against the
/// current workspace and continue the turn loop. The "resume the
/// tool-loop from this checkpoint" gesture — the model isn't
/// re-prompted for that round-trip; its existing tool calls execute
/// fresh and the loop continues with the new results in context.
/// Auth-gates before the truncation. Refused while a turn is in
/// flight. The trimmed transcript replays on `coder:event` (via the
/// reload), then the re-dispatched tool calls stream as normal
/// `tool_call` / `tool_result` events.
#[tauri::command]
pub async fn coder_resume_from_assistant(
	state: State<'_, AppState>,
	assistant_ordinal: usize,
) -> Result<(), MoonError> {
	state
		.coder
		.resume_from_assistant(assistant_ordinal)
		.await
		.map_err(MoonError::from)
}

/// Manually reapply a recorded `write_file` / `edit_file` tool
/// call from the active folder's visible session. The recovery
/// affordance behind the per-row "re-apply" control: a user who
/// reset or clobbered a file can put the agent's edit back without
/// re-running the turn. Pure side-effect — the transcript and
/// JSONL are untouched. Errors when `tool_call_id` isn't a
/// reapplyable call (unknown id, a non-write tool) or when the
/// dispatch itself fails (e.g. an `edit_file` whose `find` no
/// longer matches the current file).
#[tauri::command]
pub async fn coder_rerun_tool_call(
	state: State<'_, AppState>,
	tool_call_id: String,
) -> Result<RerunToolOutcome, MoonError> {
	state.coder.rerun_tool_call(tool_call_id).await.map_err(MoonError::from)
}

/// List persisted sessions for the active workspace folder. Empty
/// when the folder has none — including when no folder is active.
#[tauri::command]
pub async fn coder_list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionSummary>, MoonError> {
	state.coder.list_sessions().await.map_err(MoonError::from)
}

/// Snapshot of the active in-memory session, if any. `None` for a
/// blank session — the panel uses this to decide between "show
/// the sessions list" and "show this session's transcript" on
/// mount.
#[tauri::command]
pub async fn coder_active_session(state: State<'_, AppState>) -> Result<Option<SessionSummary>, MoonError> {
	Ok(state.coder.active_session().await)
}

/// Drop the in-memory session and start a blank one. Doesn't
/// touch disk — empty sessions never write a file.
#[tauri::command]
pub async fn coder_new_session(state: State<'_, AppState>) -> Result<SessionSummary, MoonError> {
	state.coder.new_session().await.map_err(MoonError::from)
}

/// Start a fresh **coordinator** (orchestrator) session (ADR 0030).
/// Same as `coder_new_session` but stamps `mode: "coordinator"` on the
/// header so `run_turn` advertises the worker-management tools
/// (`spawn_worker` / `observe_worker` / etc.) instead of `task` /
/// `ask_user`, and seeds the coordinator system prompt.
#[tauri::command]
pub async fn coder_new_coordinator_session(state: State<'_, AppState>) -> Result<SessionSummary, MoonError> {
	state.coder.new_coordinator_session().await.map_err(MoonError::from)
}

/// Result of [`coder_new_worktree_session`]: the new bound-folder
/// snapshot (so the frontend renders the nested worktree row) plus
/// the freshly-minted session to open.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewWorktreeSession {
	pub workspace: moon_protocol::workspace::Workspace,
	pub session: SessionSummary,
}

/// Create an isolated coder session in its own git worktree
/// (ADR 0028). Branches off the **active** folder's `HEAD`, checks
/// the branch out into a worktree under the workspace state dir,
/// binds that worktree as a nested folder, and mints a session
/// (filed under the parent) whose tools route to the worktree.
///
/// The active folder is unchanged — the user keeps working in the
/// parent and the new session opens in the parent's panel.
///
/// `base_branch` picks the worktree's branch:
/// - `None` → a fresh `moon/agent-<id>` off the parent's current
///   `HEAD` (the default "start clean" case).
/// - `Some(b)` → check out the **existing** branch `b` (local, or
///   DWIM-created from a remote of the same name, like `git switch`)
///   so an agent can work on it in isolation — the parent's checkout
///   is never touched, so other agents aren't disturbed.
#[tauri::command]
pub async fn coder_new_worktree_session(
	state: State<'_, AppState>,
	base_branch: Option<String>,
) -> Result<NewWorktreeSession, MoonError> {
	// Delegates to `CoderHandle::create_worktree_session` (ADR 0030
	// Prerequisite #2 — promoted from a UI-only Tauri command to a
	// client-callable method so an in-process orchestrator agent can
	// mint workers via `spawn_worker` without going through the Tauri
	// command layer). The UI path always mints an ordinary `Agent`
	// worker; a coordinator is created the same way with `Coordinator`.
	let (session, workspace) = state
		.coder
		.create_worktree_session(base_branch, moon_coder::tools::CoderMode::Agent)
		.await?;
	Ok(NewWorktreeSession { workspace, session })
}

/// Discard an isolated session's git worktree (ADR 0028): prune the
/// linked worktree via its **parent** repo and unbind the folder.
/// The branch is never deleted — it's the deliverable, left for a
/// PR. `force` passes `git worktree remove --force`, needed when the
/// worktree has uncommitted / untracked changes; the UI calls this
/// without force first and re-confirms before forcing.
///
/// Returns the updated workspace snapshot so the frontend drops the
/// nested folder row. Sessions still routed to the worktree fall
/// back to their parent folder on their next turn.
#[tauri::command]
pub async fn coder_discard_worktree(
	state: State<'_, AppState>,
	path: String,
	force: bool,
) -> Result<moon_protocol::workspace::Workspace, MoonError> {
	use moon_protocol::workspace::FolderOrigin;

	let entry = state
		.workspaces
		.folder_for_path(&path)
		.await
		.ok_or_else(|| MoonError::NotFound(format!("folder {path}")))?;
	let FolderOrigin::Worktree { parent_path, .. } = entry.folder.origin.clone() else {
		return Err(MoonError::invalid(format!("{path} is not a worktree folder")));
	};
	let parent = state
		.workspaces
		.folder_for_path(&parent_path)
		.await
		.ok_or_else(|| MoonError::invalid("the worktree's parent folder is not bound; can't prune it"))?;

	// `git worktree remove` runs against the parent repo on its active
	// shell target. The worktree's relative links resolve under either
	// mount (ADR 0029), so we just pass the path for that target:
	// the in-container `/workspace/<parent>/.worktrees/…` path when the
	// workspace runs in a container, the host path otherwise.
	let remove_path = if crate::commands::container::workspace_container_running(&state).await {
		moon_core::worktree::worktree_container_path(camino::Utf8Path::new(&parent_path), camino::Utf8Path::new(&path))
			.unwrap_or_else(|| camino::Utf8PathBuf::from(&path))
	} else {
		camino::Utf8PathBuf::from(&path)
	};
	parent.host.git_worktree_remove(&remove_path, force).await?;
	state.workspaces.remove_folder(&path).await?;
	Ok(state.workspaces.snapshot().await)
}

/// Move the active folder's visible coder session into its own git
/// worktree (ADR 0028) — the in-session counterpart to
/// `coder_new_worktree_session`. The conversation keeps its history
/// and stays in the same per-project session list; its tools just
/// start operating on an isolated branch from the next turn.
///
/// Branch choice prefers the **session's associated branch**
/// (`committed_branch` from the last commit made with this session
/// open) over the main tree's current branch:
/// - **session has an associated branch `B`**: the worktree checks out
///   `B`. The main tree is reset to the default branch only if it was
///   currently on `B` (freeing the main tree from the session's
///   branch); if the main tree is on something else, it's left alone
///   — the worktree becomes the isolated home for `B`.
/// - **no associated branch** + on a **non-default** branch `B`: the
///   worktree checks out `B` and the main tree is reset to the default
///   branch. Requires a clean tree.
/// - **no associated branch** + on the **default** branch (or detached
///   / no known default): a fresh `moon/agent-<id>` branch off `HEAD`;
///   the main tree is left as-is.
#[tauri::command]
pub async fn coder_move_session_to_worktree(state: State<'_, AppState>) -> Result<NewWorktreeSession, MoonError> {
	use moon_core::host::WorktreeBranch;

	let parent = state.workspaces.require_active_folder().await?;
	if matches!(
		parent.folder.origin,
		moon_protocol::workspace::FolderOrigin::Worktree { .. }
	) {
		return Err(MoonError::invalid("the active folder is already an isolated worktree"));
	}
	// Bail before touching git if there's nothing movable, so we never
	// strand an orphaned worktree.
	if !state.coder.can_move_visible_session().await.map_err(MoonError::from)? {
		return Err(MoonError::invalid(
			"no movable session (none is open, or it already runs in a worktree)",
		));
	}
	let parent_path = parent.folder.path.clone();

	// Decide the branch + whether the main tree resets. Prefer the
	// session's associated branch (committed_branch) over the main
	// tree's current branch — so the worktree opens on the session's
	// own work, not whatever the main tree happens to be on (which
	// could be a different session's branch).
	let session_branch = state.coder.visible_session_branch().await.map_err(MoonError::from)?;
	let info = parent.host.git_branch().await?;
	let default = info
		.default_branch_remote_ref
		.as_deref()
		.and_then(|r| r.rsplit_once('/').map(|(_, b)| b.to_string()));
	let (spec, reset_to) = if let Some(b) = &session_branch {
		// The session has an associated branch. Check it out in the
		// worktree. Only reset the main tree if it's currently on that
		// same branch (the main tree is being "freed" from it); if the
		// main tree is on something else, leave it — the worktree is the
		// isolated home for this branch now.
		let reset_to = if info.name.as_deref() == Some(b.as_str()) {
			default.clone()
		} else {
			None
		};
		(WorktreeBranch::Existing(b.clone()), reset_to)
	} else {
		match (&info.name, &default) {
			// No session branch, on a non-default branch: move it, reset main.
			(Some(b), Some(d)) if b != d => (WorktreeBranch::Existing(b.clone()), Some(d.clone())),
			// No session branch, on the default branch / detached / no
			// known default: fork fresh off HEAD.
			_ => {
				let now_ms = std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_millis())
					.unwrap_or(0) as u64;
				(
					WorktreeBranch::New(format!("moon/agent-{:08x}", now_ms & 0xffff_ffff)),
					None,
				)
			}
		}
	};
	let branch = spec.name().to_string();
	let branch_slug = branch.replace('/', "-");

	// `<parent>/.worktrees/<branch-slug>` — inside the parent repo so it
	// rides the parent's bind mount (ADR 0029).
	let worktree_path = camino::Utf8Path::new(&parent_path)
		.join(moon_core::WORKTREES_DIR_NAME)
		.join(&branch_slug);
	if let Some(dir) = worktree_path.parent() {
		std::fs::create_dir_all(dir.as_std_path()).map_err(MoonError::from)?;
	}

	// Git work first (atomic switch + add under the git lock). If it
	// fails, the session and folder registry are untouched.
	parent
		.host
		.git_worktree_add_moving(&worktree_path, spec, reset_to)
		.await?;

	let wt_entry = state
		.workspaces
		.add_worktree_folder(worktree_path, parent_path, branch.clone())
		.await?;

	let session = state
		.coder
		.move_visible_session_to_worktree(wt_entry.folder.path.clone(), branch)
		.await
		.map_err(MoonError::from)?
		.ok_or_else(|| MoonError::invalid("no visible session to move"))?;

	let workspace = state.workspaces.snapshot().await;
	Ok(NewWorktreeSession { workspace, session })
}

/// Tie the active folder's visible coder session to the branch its
/// work was just committed onto (ADR 0028). Resolves the current
/// `HEAD` branch — so it covers both "commit on new branch" and a
/// plain commit on the current branch — and stamps it on the session.
/// Returns the updated summary so the panel can show a "switch back
/// to this branch" chip; `None` when `HEAD` is detached or there's no
/// persisted session to tie (e.g. a manual commit with nothing open).
#[tauri::command]
pub async fn coder_associate_branch(state: State<'_, AppState>) -> Result<Option<SessionSummary>, MoonError> {
	let Some(folder) = state.workspaces.active_folder().await else {
		return Ok(None);
	};
	let Some(branch) = folder.host.git_branch().await?.name else {
		return Ok(None);
	};
	state
		.coder
		.set_visible_session_branch(branch)
		.await
		.map_err(MoonError::from)
}

/// Set the per-session bash-target override for the active
/// folder's visible session. `force_host = true` pins this
/// session's `bash` / shell tool to the host machine even while
/// the workspace runs in a container; `false` restores auto.
/// Returns the resolved force-host state.
#[tauri::command]
pub async fn coder_set_bash_target_override(state: State<'_, AppState>, force_host: bool) -> Result<bool, MoonError> {
	state
		.coder
		.set_bash_target_override(force_host)
		.await
		.map_err(MoonError::from)
}

/// Replace the in-memory session with the persisted one
/// identified by `id`. Backend emits `session_loaded` + per-record
/// replay events on the `coder:event` channel; the frontend reacts
/// to those rather than getting the records back inline.
#[tauri::command]
pub async fn coder_open_session(state: State<'_, AppState>, id: String) -> Result<SessionSummary, MoonError> {
	let summary = state.coder.open_session(id).await.map_err(MoonError::from)?;
	let folder = active_folder_path(&state).await;
	if let Some(folder) = folder {
		persist_last_session(&state.config_dir, &folder, Some(summary.id.clone())).await;
	}
	Ok(summary)
}

/// Resolve the on-disk JSONL path of a session under the active
/// workspace folder. Frontend uses this to open the raw trace in
/// the editor via the host-direct file path (same mechanism as
/// `Ctrl+O` for files outside the workspace). Works for sub-agent
/// ids too — they share the parent folder's slug.
#[tauri::command]
pub async fn coder_session_jsonl_path(state: State<'_, AppState>, id: String) -> Result<String, MoonError> {
	let path = state.coder.session_jsonl_path(id).await.map_err(MoonError::from)?;
	Ok(path.into_string())
}

/// Delete a persisted session for the active workspace folder.
/// Idempotent. Emits `session_list_changed` afterwards.
#[tauri::command]
pub async fn coder_delete_session(state: State<'_, AppState>, id: String) -> Result<(), MoonError> {
	state.coder.delete_session(id.clone()).await.map_err(MoonError::from)?;
	let folder = active_folder_path(&state).await;
	if let Some(folder) = folder {
		let id_owned = id.clone();
		app_state_store::mutate(&state.config_dir, move |s| {
			if s.coder.last_session_by_folder.get(&folder).map(|v| v.as_str()) == Some(id_owned.as_str()) {
				s.coder.last_session_by_folder.remove(&folder);
			}
		})
		.await?;
	}
	Ok(())
}

/// Persist the last-opened session id for the given workspace
/// folder so a relaunch lands the user back in the right
/// transcript per project. Best-effort: a write failure logs but
/// doesn't fail the open call. `None` clears the entry (e.g. the
/// user just deleted the session).
async fn persist_last_session(config_dir: &camino::Utf8Path, folder: &str, id: Option<String>) {
	let folder_owned = folder.to_string();
	let result = app_state_store::mutate(config_dir, move |s| {
		let existing = s.coder.last_session_by_folder.get(&folder_owned).cloned();
		match (existing, id) {
			(Some(prev), Some(new)) if prev == new => {}
			(None, None) => {}
			(_, Some(new)) => {
				s.coder.last_session_by_folder.insert(folder_owned, new);
			}
			(_, None) => {
				s.coder.last_session_by_folder.remove(&folder_owned);
			}
		}
	})
	.await;
	if let Err(err) = result {
		tracing::warn!(error = %err, "could not persist last session id");
	}
}

/// Active workspace folder's absolute path, or `None` when the
/// workspace is empty / no folder is bound. Used by the
/// per-folder persistence helpers.
async fn active_folder_path(state: &AppState) -> Option<String> {
	state
		.workspaces
		.active_folder()
		.await
		.map(|entry| entry.folder.path.clone())
}

/// Snapshot of the user's current model picks. The popover reads
/// this on open so it doesn't fall out of sync if a different
/// surface (or a future hotkey) wrote to AppState. Returns the
/// full picker state in one shot: HF picks, the active provider
/// id, the user-added providers list (each carrying its own
/// `has_api_key` flag, which is sourced from the keyring rather
/// than echoed from disk), and the per-workspace provider lock.
///
/// The `active_provider` field on the response is the
/// **effective** active provider (lock if pinned, else global
/// default) — i.e. what the runner is actually using. The
/// `provider_lock` field tells the picker whether the value
/// came from a workspace lock or the global. They never disagree
/// at the value level when the lock is set; the lock is just the
/// "where did this come from?" annotation the picker needs to
/// route writes correctly.
#[tauri::command]
pub async fn coder_get_model_settings(state: State<'_, AppState>) -> Result<CoderModelSettings, MoonError> {
	let models = state.coder.current_models().await;
	let provider_lock = workspace_provider_lock(&state).await;
	Ok(CoderModelSettings {
		standard_model: models.standard,
		cheap_model: models.cheap,
		bill_to: models.bill_to.unwrap_or_default(),
		active_provider: models.active_provider,
		providers: models.providers,
		// Clone out of the `Arc<HashMap>`: the picker mutates
		// the map locally and round-trips it back in
		// `coder_set_model_settings`; sharing the `Arc` would
		// risk a write through a stale clone.
		context_window_overrides: (*models.context_window_overrides).clone(),
		provider_lock,
	})
}

/// Read the per-workspace provider lock from `session.json` for
/// the workspace this process owns. `None` for processes that
/// haven't bound a workspace (preboot mode), and `None` on any
/// I/O / parse failure (logged inside `core_session::load`). A
/// missing file is normal — first launch never wrote one.
async fn workspace_provider_lock(state: &AppState) -> Option<CoderProviderLock> {
	let id = state.workspace_id()?;
	match core_session::load(&state.workspaces_dir, id).await {
		Ok(session) => session.coder_provider_lock,
		Err(err) => {
			tracing::warn!(error = %err, "could not load session for provider-lock read");
			None
		}
	}
}

/// Apply `lock` to this workspace's `session.json`. `Some(_)`
/// replaces the existing lock; `None` clears it. No-ops in
/// preboot mode (no workspace bound to the process). Updates the
/// lock field in place; every other field on the session is
/// preserved by load-then-save round-trip so we don't clobber
/// folders / tabs / SCM filters that the frontend's
/// `session_save` flow keeps current.
async fn write_workspace_provider_lock(state: &AppState, lock: Option<CoderProviderLock>) -> Result<(), MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(());
	};
	let id = id.to_owned();
	let mut session = core_session::load(&state.workspaces_dir, &id).await?;
	if session.coder_provider_lock == lock {
		return Ok(());
	}
	session.coder_provider_lock = lock;
	core_session::save(&state.workspaces_dir, &id, &session).await
}

/// Persist + apply the new picker settings. Writes through
/// AppState (so a relaunch sees the same picks) and pokes the
/// coder so the very next round-trip uses the new model +
/// bill_to + provider list. Slugs are already in their final
/// `model:provider` form because the picker concatenates on
/// click; the runner doesn't do any post-processing.
///
/// API keys do **not** travel through this command. The picker
/// uses `coder_set_provider_api_key` / `coder_clear_provider_api_key`
/// (per-id, keyring-backed) so secrets never round-trip through
/// AppState or the IPC layer's logging.
///
/// **Provider lock semantics**: when `settings.provider_lock` is
/// `Some(_)`, the workspace is pinned to the picked active
/// provider. The lock is persisted into `session.json` and the
/// runner is updated, but the global
/// [`crate::commands::app_state`] active provider is left
/// untouched — sibling workspaces keep their own behaviour. When
/// the lock is `None`, the previous behaviour applies: the
/// picked active provider becomes the new global default and any
/// prior lock on this workspace is cleared.
#[tauri::command]
pub async fn coder_set_model_settings(
	state: State<'_, AppState>,
	settings: CoderModelSettings,
) -> Result<(), MoonError> {
	let bill_to = if settings.bill_to.is_empty() {
		None
	} else {
		Some(settings.bill_to.clone())
	};
	let providers_for_runner = settings.providers.clone();
	let active_for_runner = settings.active_provider.clone();
	let overrides_for_runner = settings.context_window_overrides.clone();
	let provider_lock = settings.provider_lock.clone();
	state
		.coder
		.set_user_picks(settings.standard_model.clone(), settings.cheap_model.clone(), bill_to)
		.await;
	// Runner always gets the effective active provider (lock if
	// pinned, else the global). The picker pre-resolved this onto
	// `settings.active_provider`, so we forward verbatim.
	state.coder.set_providers(providers_for_runner, active_for_runner).await;
	state.coder.set_context_window_overrides(overrides_for_runner).await;

	// Persist the per-workspace lock first. If this fails we
	// haven't yet touched the global `state.json`, so the user
	// retries against an unchanged baseline. (Reverse order
	// would mean a transient session-write failure silently
	// promoted a workspace pin to the global default.)
	write_workspace_provider_lock(&state, provider_lock.clone()).await?;

	let lock_active_provider = match &provider_lock {
		Some(CoderProviderLock::Hf) => Some(None),
		Some(CoderProviderLock::User { id }) => Some(Some(id.clone())),
		None => None,
	};
	app_state_store::mutate(&state.config_dir, move |s| {
		s.coder.standard_model = settings.standard_model;
		s.coder.cheap_model = settings.cheap_model;
		s.coder.bill_to = settings.bill_to;
		// Only the unlocked path writes back to the global
		// active provider — locked saves keep the global frozen
		// so other workspaces aren't dragged along. The runner
		// already got the locked value through `set_providers`
		// above; persistence here is purely about the next
		// boot's global default for unlocked workspaces.
		if lock_active_provider.is_none() {
			s.coder.active_provider = settings.active_provider;
		}
		// Strip `has_api_key` before persisting — it's keyring-derived,
		// not state, and surviving it on disk would let a hand-edited
		// `state.json` claim a key is configured when the keyring is
		// empty.
		s.coder.providers = settings
			.providers
			.into_iter()
			.map(|mut p| {
				p.has_api_key = false;
				p
			})
			.collect();
		// Drop `0`-valued entries on persist: they're already
		// treated as "no cap" at the runtime boundary, but
		// keeping them on disk would litter `state.json` with
		// inert rows after every `Clear` gesture.
		s.coder.context_window_overrides = settings
			.context_window_overrides
			.into_iter()
			.filter(|(_, v)| *v > 0)
			.collect();
	})
	.await?;
	Ok(())
}

/// Fetch the HF router's `/v1/models` catalog for the picker's
/// HF tab. Not gated on the persisted active route — the picker
/// lets the user flip between the HF tab and user-provider tabs
/// while editing, and a 500 on every HF tab visit while
/// OpenRouter is currently active would be wrong. User-provider
/// catalogs use `coder_list_provider_models`; the two
/// entrypoints exist because the wire shapes differ. One round
/// trip per call; the frontend caches the result for the
/// lifetime of the popover so flipping tabs doesn't re-hit the
/// network.
#[tauri::command]
pub async fn coder_list_models(state: State<'_, AppState>) -> Result<Vec<RouterModel>, MoonError> {
	state.coder.list_models().await.map_err(MoonError::from)
}

/// Allocate a fresh opaque provider id. The picker's `Add provider`
/// modal calls this before any keyring / config write so the
/// keyring slot is addressable from the moment the user types a
/// key, even if they cancel out of the modal before saving.
/// Idempotent in the sense that a leaked id without any matching
/// state is harmless — nothing reads the keyring slot until the
/// provider config lands in `AppState`.
#[tauri::command]
pub async fn coder_new_provider_id(state: State<'_, AppState>) -> Result<String, MoonError> {
	Ok(state.coder.new_provider_id())
}

/// Probe a `(base_url, api_key)` pair before the picker commits.
/// Surfaces the upstream HTTP failure verbatim on error so the
/// user can see "401 Unauthorized" / "couldn't reach host" / etc.
/// `api_key` empty = probe without an `Authorization` header
/// (local llama.cpp / Ollama).
#[derive(Debug, Deserialize)]
pub struct ProbeProviderArgs {
	pub base_url: String,
	#[serde(default)]
	pub api_key: String,
	/// Wire-shape hint for the probe. Anthropic uses
	/// `x-api-key` + `anthropic-version` headers and a different
	/// catalog endpoint; everything else (OpenRouter, custom
	/// OpenAI-compat) probes via `Authorization: Bearer …` against
	/// `/v1/models`. Defaults to `Custom` when missing so older
	/// frontends keep working.
	#[serde(default)]
	pub kind: moon_protocol::coder_models::ProviderKind,
}

#[tauri::command]
pub async fn coder_probe_provider(
	state: State<'_, AppState>,
	args: ProbeProviderArgs,
) -> Result<ProviderProbeResult, MoonError> {
	let key = if args.api_key.is_empty() {
		None
	} else {
		Some(args.api_key.as_str())
	};
	state
		.coder
		.probe_provider(&args.base_url, args.kind, key)
		.await
		.map_err(MoonError::from)
}

/// Persist a per-provider API key in the OS keyring. Empty values
/// are rejected — same trap the Tavily key avoids: a silently-empty
/// entry would set `has_api_key: true` while every downstream
/// call 401s. After this returns Ok, the next request resolving
/// to this provider picks up the new key without rewiring.
#[derive(Debug, Deserialize)]
pub struct SetProviderApiKeyArgs {
	pub id: String,
	pub key: String,
}

#[tauri::command]
pub async fn coder_set_provider_api_key(
	state: State<'_, AppState>,
	args: SetProviderApiKeyArgs,
) -> Result<(), MoonError> {
	state
		.coder
		.set_provider_api_key(&args.id, &args.key)
		.map_err(MoonError::from)
}

/// Drop the keyring entry for a provider. Idempotent — fine to
/// call on a provider that never had a key (the local-vLLM case
/// where the user is just removing a stale entry).
#[tauri::command]
pub async fn coder_clear_provider_api_key(state: State<'_, AppState>, id: String) -> Result<(), MoonError> {
	state.coder.clear_provider_api_key(&id).map_err(MoonError::from)
}

/// Flat `/v1/models` catalog for a user-added provider. The
/// picker uses this instead of `coder_list_models` when a user
/// provider is active. Returns the OpenAI-compat `{id, owned_by}`
/// rows; the picker renders them as a flat searchable list
/// (no pricing / throughput — those aren't uniform across
/// OpenAI-compat servers).
///
/// Errors on network / 4xx / 5xx propagate verbatim. A 404 means
/// the server doesn't expose the catalog endpoint; the picker
/// shows "Catalog unavailable" and the user can still type a
/// model slug directly into the field.
#[tauri::command]
pub async fn coder_list_provider_models(
	state: State<'_, AppState>,
	id: String,
) -> Result<Vec<ProviderModelSummary>, MoonError> {
	state.coder.list_provider_models(&id).await.map_err(MoonError::from)
}

/// Side-channel persist of a brand-new provider entry. Used by
/// the `Add provider` modal so the new provider lands in
/// `AppState` and the runtime view *before* the user has had a
/// chance to flip it to active. The picker can call
/// `coder_set_model_settings` later with the full state, but
/// this commit-per-action shape keeps the "I clicked Save in
/// the Add modal, but cancelled the outer modal" path from
/// losing the provider.
///
/// The keyring entry, if any, was already written via
/// `coder_set_provider_api_key` against `config.id` before this
/// call — we don't take the key as a parameter here.
#[tauri::command]
pub async fn coder_save_provider(state: State<'_, AppState>, config: CoderProviderConfig) -> Result<(), MoonError> {
	let (providers, global_active) = app_state_store::mutate(&state.config_dir, move |s| {
		let mut cfg = config.clone();
		// `has_api_key` is keyring-derived; never trust the caller.
		cfg.has_api_key = false;
		if let Some(existing) = s.coder.providers.iter_mut().find(|p| p.id == cfg.id) {
			*existing = cfg;
		} else {
			s.coder.providers.push(cfg);
		}
		(s.coder.providers.clone(), s.coder.active_provider.clone())
	})
	.await?;
	// A pinned workspace's effective provider isn't the global
	// `active`; if we forwarded the global verbatim, adding a new
	// provider in a locked workspace would silently flip the
	// runner off the lock. Resolve the lock first.
	let effective_active = match workspace_provider_lock(&state).await {
		Some(CoderProviderLock::Hf) => None,
		Some(CoderProviderLock::User { id }) => Some(id),
		None => global_active,
	};
	state.coder.set_providers(providers, effective_active).await;
	Ok(())
}

/// Drop a provider entry from `AppState` and its keyring slot.
/// If the deleted provider was active, the runner falls back to
/// HF (the only always-available route). Idempotent: deleting an
/// unknown id is a no-op.
///
/// If this workspace's `coder_provider_lock` pinned the deleted
/// provider, clear it so the next read resolves the global default
/// without leaving a `tracing::warn!` per turn for the orphaned
/// lock. Sibling workspaces that pinned the same id (other OS
/// processes) get the warn-and-fallback path on their next boot —
/// we don't reach across processes to clean their `session.json`.
#[tauri::command]
pub async fn coder_delete_provider(state: State<'_, AppState>, id: String) -> Result<(), MoonError> {
	// Drop the keyring entry first; even if the AppState write
	// fails, we don't want the credential to outlive the config.
	let _ = state.coder.clear_provider_api_key(&id);
	let id_for_state = id.clone();
	let (providers, global_active) = app_state_store::mutate(&state.config_dir, move |s| {
		s.coder.providers.retain(|p| p.id != id_for_state);
		if s.coder.active_provider.as_deref() == Some(id_for_state.as_str()) {
			s.coder.active_provider = None;
		}
		(s.coder.providers.clone(), s.coder.active_provider.clone())
	})
	.await?;
	let workspace_lock = workspace_provider_lock(&state).await;
	let lock_pointed_at_deleted =
		matches!(&workspace_lock, Some(CoderProviderLock::User { id: locked }) if locked == &id);
	if lock_pointed_at_deleted {
		write_workspace_provider_lock(&state, None).await?;
	}
	// Recompute the effective active provider: the lock — if it
	// survived the deletion — still wins over the global default.
	// Without this, deleting an unrelated provider (Y) while
	// pinned to X would silently flip the runner from X to the
	// global active, which could be HF or anything else.
	let effective_active = if lock_pointed_at_deleted {
		global_active
	} else {
		match workspace_lock {
			Some(CoderProviderLock::Hf) => None,
			Some(CoderProviderLock::User { id }) => Some(id),
			None => global_active,
		}
	};
	state.coder.set_providers(providers, effective_active).await;
	Ok(())
}

/// `true` iff a Tavily API key is stored in the OS keyring. The
/// model-settings popover reads this on mount so it can render the
/// "set a key" / "key configured" state correctly. Cheap sync read
/// of the in-memory cache — no keyring round-trip.
#[tauri::command]
pub async fn coder_web_search_configured(state: State<'_, AppState>) -> Result<bool, MoonError> {
	Ok(state.coder.web_search_configured())
}

// ---------- HF Hub bucket sync commands ----------

/// Namespaces the user can create a bucket under: their own
/// login plus every org they belong to. Populates the connect
/// modal's dropdown. Driven off the cached OAuth identity, so
/// the modal opens instantly.
#[tauri::command]
pub async fn coder_hub_list_namespaces(state: State<'_, AppState>) -> Result<Vec<HubNamespace>, MoonError> {
	state.coder.hub_sync().list_namespaces().await.map_err(MoonError::from)
}

/// Read the current workspace's Hub binding (if any). Cheap
/// `session.json` read; the picker uses this on mount to render
/// the "Connected to …" / "Connect" affordance.
#[tauri::command]
pub async fn coder_hub_get_binding(state: State<'_, AppState>) -> Result<Option<CoderHubBucket>, MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(None);
	};
	let session = core_session::load(&state.workspaces_dir, id).await?;
	Ok(session.coder_hub_bucket)
}

/// Provision a new bucket on the Hub, write the README, and bind
/// it to the active workspace. `autosync` defaults to `false`;
/// the modal nudges the user to flip it on but never auto-flips.
#[tauri::command]
pub async fn coder_hub_create_bucket(
	state: State<'_, AppState>,
	namespace: String,
	name: String,
	private: bool,
) -> Result<CoderHubBucket, MoonError> {
	let Some(id) = state.workspace_id() else {
		return Err(MoonError::invalid("no workspace bound to this process"));
	};
	let id = id.to_owned();
	let workspace_label = workspace_display_label(&state, &id)
		.await
		.unwrap_or_else(|| name.clone());
	let bucket = state
		.coder
		.hub_sync()
		.create_bucket(&namespace, &name, private, &workspace_label)
		.await?;
	let mut session = core_session::load(&state.workspaces_dir, &id).await?;
	session.coder_hub_bucket = Some(bucket.clone());
	core_session::save(&state.workspaces_dir, &id, &session).await?;
	Ok(bucket)
}

/// Flip autosync on or off for the active workspace's binding.
/// No-op (returns `Ok`) when there's no binding — the modal
/// shouldn't surface the toggle in that case, but guarding here
/// keeps the IPC contract uniform across UI states.
#[tauri::command]
pub async fn coder_hub_set_autosync(state: State<'_, AppState>, enabled: bool) -> Result<(), MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(());
	};
	let id = id.to_owned();
	let mut session = core_session::load(&state.workspaces_dir, &id).await?;
	let Some(bucket) = session.coder_hub_bucket.as_mut() else {
		return Ok(());
	};
	if bucket.autosync == enabled {
		return Ok(());
	}
	bucket.autosync = enabled;
	core_session::save(&state.workspaces_dir, &id, &session).await
}

/// Drop the workspace's Hub binding (does not delete the bucket
/// on the Hub — that's a separate web-UI action). Clears the
/// in-flight upload cache too.
#[tauri::command]
pub async fn coder_hub_disconnect(state: State<'_, AppState>) -> Result<(), MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(());
	};
	let id = id.to_owned();
	let mut session = core_session::load(&state.workspaces_dir, &id).await?;
	if session.coder_hub_bucket.is_none() {
		return Ok(());
	}
	session.coder_hub_bucket = None;
	core_session::save(&state.workspaces_dir, &id, &session).await
}

/// Push one session JSONL to the bound bucket. Used by the row
/// "Upload" affordance and the header "Sync all" button. Always
/// available, regardless of `autosync`. Emits
/// `coder:event` envelopes (`HubSyncStarted` / `HubSyncFinished`)
/// for live UI state.
#[tauri::command]
pub async fn coder_hub_upload_session(state: State<'_, AppState>, session_id: String) -> Result<(), MoonError> {
	let Some(id) = state.workspace_id() else {
		return Err(MoonError::invalid("no workspace bound to this process"));
	};
	let id = id.to_owned();
	let Some(folder) = state.coder.active_folder().await else {
		return Err(MoonError::invalid("no active workspace folder"));
	};
	let folder_path = Utf8PathBuf::from(folder);
	state
		.coder
		.hub_sync()
		.upload_session(&id, &folder_path, &session_id)
		.await
		.map_err(MoonError::from)
}

/// Push every local top-level session JSONL across every folder
/// bound to this workspace, batching one `xet-write-token` fetch
/// and one `/batch` POST across the whole set. Skips sessions
/// whose local JSONL is already at the length the `uploaded`
/// marker recorded — Xet would dedup the bytes anyway, but we
/// also save the round-trip. Emits per-session
/// `HubSyncStarted` / `HubSyncFinished` so the panel's row
/// decorations animate in lockstep.
#[tauri::command]
pub async fn coder_hub_upload_all_sessions(state: State<'_, AppState>) -> Result<HubUploadAllSummary, MoonError> {
	if state.workspace_id().is_none() {
		return Err(MoonError::invalid("no workspace bound to this process"));
	}
	state.coder.hub_upload_all_sessions().await.map_err(MoonError::from)
}

/// Build the Hub web-viewer URL for `session_id` under the
/// active folder. Resolves to
/// `https://huggingface.co/buckets/<ns>/<name>/tree/<folder-slug>/<id>.jsonl`
/// — same `<folder-slug>/<id>.jsonl` layout the runner uses for
/// the upload path, so this matches whatever's actually on the
/// Hub.
///
/// Errors with a typed `Invalid` when the workspace has no bucket
/// connected (the panel should not surface the affordance in that
/// case; this is a defence-in-depth check) or when there's no
/// active folder. Doesn't check whether the file actually exists
/// on the Hub — the per-row affordance is gated on the local
/// `uploaded` marker, which is enough.
#[tauri::command]
pub async fn coder_hub_session_url(state: State<'_, AppState>, session_id: String) -> Result<String, MoonError> {
	let Some(id) = state.workspace_id() else {
		return Err(MoonError::invalid("no workspace bound to this process"));
	};
	let id = id.to_owned();
	let session = core_session::load(&state.workspaces_dir, &id).await?;
	let Some(bucket) = session.coder_hub_bucket else {
		return Err(MoonError::invalid(
			"no Hugging Face bucket connected for this workspace",
		));
	};
	let Some(folder) = state.coder.active_folder().await else {
		return Err(MoonError::invalid("no active workspace folder"));
	};
	let folder_path = Utf8PathBuf::from(folder);
	let path_in_bucket = moon_coder::hub_sync::bucket_path_for(&folder_path, &session_id);
	Ok(format!(
		"https://huggingface.co/buckets/{namespace}/{name}/tree/{path}",
		namespace = bucket.namespace,
		name = bucket.name,
		path = path_in_bucket,
	))
}

/// Helper: best-effort human label for the workspace, used to seed
/// the HF Hub bucket README.
///
/// Preference order:
///
/// 1. The workspace's display `name` from the global catalog
///    (`AppState.workspaces[id].name`) — what the user typed at
///    workspace-create time and what's labelled on the picker.
///    The right answer for a multi-folder workspace.
/// 2. The active folder's basename — a fallback for pre-catalog
///    bootstraps where the catalog hasn't been populated yet
///    (early launch, dev path). Worse than (1) for multi-folder
///    workspaces but better than an empty README header.
/// 3. `None` — caller falls back to the bucket name itself.
async fn workspace_display_label(state: &AppState, workspace_id: &str) -> Option<String> {
	if let Ok(app_state) = moon_core::app_state::load(&state.config_dir).await {
		if let Some(meta) = app_state.workspaces.iter().find(|m| m.id == workspace_id) {
			let trimmed = meta.name.trim();
			if !trimmed.is_empty() {
				return Some(trimmed.to_string());
			}
		}
	}
	let folder = state.coder.active_folder().await?;
	let path = Utf8PathBuf::from(folder);
	path.file_name().map(|s| s.to_string())
}

/// Persist a new Tavily API key. Trimmed at the runner; empty
/// values are rejected. After this returns Ok, the next agent
/// turn will see `web_search` in its tool list.
#[tauri::command]
pub async fn coder_set_web_search_key(state: State<'_, AppState>, key: String) -> Result<(), MoonError> {
	state.coder.set_web_search_key(&key).map_err(MoonError::from)
}

/// Drop the Tavily key from the keyring. Idempotent.
#[tauri::command]
pub async fn coder_clear_web_search_key(state: State<'_, AppState>) -> Result<(), MoonError> {
	state.coder.clear_web_search_key().map_err(MoonError::from)
}
