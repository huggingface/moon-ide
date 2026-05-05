//! The agent loop.
//!
//! `Coder` owns the in-memory session, the inference client, the
//! tool registry, the cancellation handle for the active turn, and
//! the per-workspace session-storage layer. UI-facing state changes
//! happen via [`CoderEvent`] pushes on the broadcast channel the
//! Tauri layer subscribes to.
//!
//! Loop shape (see `specs/coder.md` § Loop shape):
//!
//! 1. Append the user message to `messages` + the JSONL session.
//! 2. Stream `chat/completions` and emit `assistant_message_*`
//!    events as deltas land.
//! 3. If the response has tool calls, dispatch each via
//!    [`ToolRegistry`], append the assistant message + tool result
//!    messages to `messages` + the JSONL session, loop.
//! 4. If the response is text-only, append the assistant message,
//!    emit `TurnComplete`, exit.
//! 5. After the *first* successful turn, kick off an
//!    auto-rename pass that asks the fast model for a 4-6 word
//!    title and persists it.
//! 6. Cap iterations at [`MAX_TURN_ITERATIONS`] so a misbehaving
//!    model can't run forever.

use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use crate::auth::{Authenticator, DeviceCode, HfIdentity};
use crate::defaults::{DEFAULT_FAST_MODEL, DEFAULT_LARGE_MODEL, MAX_TURN_ITERATIONS, PHASE_6_0_SYSTEM_PROMPT};
use crate::error::CoderError;
use crate::event::{CoderEvent, CoderStatus};
use crate::inference::{AssistantResponse, ChatMessage, FunctionCall, InferenceClient, StreamEvent};
use crate::sessions::{
	self, current_time_ms, new_session_id, session_title_from_prompt, sessions_dir, LoadedSession, SessionHeader,
	SessionRecord, SessionSummary, SESSION_SCHEMA_VERSION,
};
use crate::tools::ToolRegistry;

/// Capacity for the broadcast channel the Tauri layer subscribes to.
/// Each turn produces O(few hundred) events at most; oversizing
/// avoids back-pressure stalls when the UI is slow to consume.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Public, cheap-to-clone handle the Tauri layer holds on to. Wraps
/// the inner shared state in `Arc`s so the same coder can be addressed
/// from every command + the event-pump task.
#[derive(Clone)]
pub struct CoderHandle {
	state: Arc<CoderState>,
}

/// Inner shared state. Each field is independently lockable / cloneable
/// so the spawned turn future can take exactly the handles it needs
/// without aliasing a single big lock.
struct CoderState {
	auth: Authenticator,
	inference: InferenceClient,
	tools: ToolRegistry,
	events: broadcast::Sender<CoderEvent>,
	turn: Arc<Mutex<TurnState>>,
	session: Arc<Mutex<Session>>,
	/// Held here in addition to inside `ToolRegistry` so `status()`
	/// can read the active folder + container state for the panel-
	/// header indicator without going through the tool dispatch path.
	workspaces: Arc<WorkspaceRegistry>,
	/// Parent directory under which each workspace's compose state
	/// lives (`<workspaces_dir>/<workspace_id>/compose.yaml`). Used
	/// by [`crate::tools::resolve_bash_target`] to ask
	/// `moon_container::Workspace` whether the container is running.
	workspaces_dir: Utf8PathBuf,
	/// Per-machine root for persisted coder sessions —
	/// `<XDG_DATA_HOME>/moon-ide/coder-sessions/`. Each workspace
	/// folder gets a deterministic `<basename>-<hash>/` subdirectory
	/// computed by [`sessions::project_slug`]; the JSONL files
	/// live one level deeper still. Sessions deliberately don't
	/// live inside the project tree any more — they're personal
	/// scratch / history, not project artefacts.
	coder_sessions_dir: Utf8PathBuf,
}

/// Per-turn cancellation token + "is anything running right now?"
/// flag. Held under one mutex so `abort` and `send` race on the same
/// lock, avoiding the "abort fires between status check and spawn"
/// hole.
#[derive(Default)]
struct TurnState {
	cancel: Option<CancellationToken>,
}

/// In-memory session. Per AGENTS.md "no premature migrations" we
/// keep one active session at a time; switching to another
/// session is "open it, replace this struct's contents".
struct Session {
	/// Per-session metadata. The header is in memory from the
	/// moment the session is created; it lands on disk only after
	/// the first record append (lazy persist, see `sessions.rs`).
	header: SessionHeader,
	/// Resolved sessions directory the session writes to (typically
	/// `<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/`).
	/// `None` for a fresh session that hasn't been associated with
	/// a folder yet (the binding happens on first `send`, taking
	/// the active folder at that moment). Without it we can't
	/// write to disk and `list_sessions` won't see the file.
	session_dir: Option<Utf8PathBuf>,
	/// The full chat history sent to the model. Always starts
	/// with the system prompt; everything else appends in turn
	/// order. The system prompt is **not** persisted — re-opening
	/// a session re-adds the current default at load time, so
	/// prompt updates between releases apply retroactively.
	messages: Vec<ChatMessage>,
	/// Records appended since session start. Mirrors `messages`
	/// minus the system prompt; kept separately so writing a new
	/// JSONL file when persisting a previously-empty session
	/// doesn't have to filter `messages`.
	persisted_records: u32,
	/// `true` until the auto-rename pass has run (or been skipped
	/// because the model failed). Avoids re-renaming on every
	/// subsequent turn.
	auto_rename_pending: bool,
}

impl Session {
	/// Make a fresh session shell — id allocated, title empty
	/// pending the first prompt, no folder bound.
	fn new_blank() -> Self {
		let now = current_time_ms();
		Self {
			header: SessionHeader {
				schema: SESSION_SCHEMA_VERSION,
				id: new_session_id(),
				title: String::new(),
				created_at_ms: now,
				updated_at_ms: now,
				model: DEFAULT_LARGE_MODEL.to_string(),
			},
			session_dir: None,
			messages: vec![ChatMessage::System {
				content: PHASE_6_0_SYSTEM_PROMPT.to_string(),
			}],
			persisted_records: 0,
			auto_rename_pending: false,
		}
	}

	fn summary(&self) -> SessionSummary {
		SessionSummary {
			id: self.header.id.clone(),
			title: self.header.title.clone(),
			created_at_ms: self.header.created_at_ms,
			updated_at_ms: self.header.updated_at_ms,
		}
	}
}

/// Public alias kept for symmetry with how the Tauri layer used to
/// reach the inner type. Removing it later is a non-issue.
pub type Coder = CoderHandle;

impl CoderHandle {
	pub fn new(
		workspaces: Arc<WorkspaceRegistry>,
		workspaces_dir: Utf8PathBuf,
		coder_sessions_dir: Utf8PathBuf,
	) -> Result<Self, CoderError> {
		let auth = Authenticator::new()?;
		let inference = InferenceClient::new(auth.clone())?;
		let tools = ToolRegistry::new(workspaces.clone(), workspaces_dir.clone());
		let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
		Ok(Self {
			state: Arc::new(CoderState {
				auth,
				inference,
				tools,
				events,
				turn: Arc::new(Mutex::new(TurnState::default())),
				session: Arc::new(Mutex::new(Session::new_blank())),
				workspaces,
				workspaces_dir,
				coder_sessions_dir,
			}),
		})
	}

	pub async fn status(&self) -> Result<CoderStatus, CoderError> {
		let identity = self.state.auth.identity().await?;
		let busy = self.state.turn.lock().await.cancel.is_some();
		// `bash_target` mirrors what `tools::bash` would pick if it
		// ran right now. Computed here so the panel header can show
		// the indicator without waiting for the first `bash` call.
		// `None` when no folder is active — chat still works, only
		// tool calls would fail.
		let bash_target = if self.state.workspaces.active_folder().await.is_some() {
			Some(
				crate::tools::resolve_bash_target(&self.state.workspaces, &self.state.workspaces_dir)
					.await
					.to_string(),
			)
		} else {
			None
		};
		Ok(CoderStatus {
			signed_in: identity.is_some(),
			identity,
			busy,
			bash_target,
		})
	}

	pub async fn start_device_flow(&self) -> Result<DeviceCode, CoderError> {
		self.state.auth.start_device_flow().await
	}

	pub async fn poll_device_code(&self, code: DeviceCode) -> Result<HfIdentity, CoderError> {
		self.state.auth.poll_device_code(&code).await
	}

	pub async fn sign_out(&self) -> Result<(), CoderError> {
		self.abort_inner().await;
		self.state.auth.sign_out().await?;
		// Reset the in-memory session — a re-sign-in is conceptually
		// a fresh conversation. On-disk sessions are untouched (they
		// belong to the workspace, not the user identity).
		*self.state.session.lock().await = Session::new_blank();
		Ok(())
	}

	/// Snapshot of the active session. `None` when the session is
	/// blank (no user message yet) — the panel uses this to render
	/// the empty / "send your first message" state.
	pub async fn active_session(&self) -> Option<SessionSummary> {
		let session = self.state.session.lock().await;
		if session.header.title.is_empty() && session.persisted_records == 0 {
			return None;
		}
		Some(session.summary())
	}

	/// List sessions on disk for the active workspace folder.
	/// Empty when the folder has none — including when no folder
	/// is active at all (chat-only sessions aren't supported).
	pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CoderError> {
		let Some(folder) = self.state.workspaces.active_folder().await else {
			return Ok(Vec::new());
		};
		let folder_root = Utf8PathBuf::from(folder.folder.path.clone());
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		sessions::list_sessions(&dir).await
	}

	/// Discard the current in-memory session and start a blank
	/// one. Doesn't touch disk — empty sessions never get a file
	/// in the first place. Returns the new session's metadata so
	/// the panel can reference it before the first send.
	pub async fn new_session(&self) -> Result<SessionSummary, CoderError> {
		self.abort_inner().await;
		let mut session = self.state.session.lock().await;
		*session = Session::new_blank();
		let summary = session.summary();
		drop(session);
		// Empty sessions don't fire `SessionLoaded` (frontend
		// reconciles to "blank state" on its own), but the list
		// hasn't actually changed either — no disk impact yet.
		Ok(summary)
	}

	/// Replace the active session with the persisted one
	/// identified by `id` under the active workspace folder.
	/// Replays the JSONL records as live events so the panel's
	/// existing event handlers populate the transcript without a
	/// special "loaded" code path beyond the initial reset.
	pub async fn open_session(&self, id: String) -> Result<SessionSummary, CoderError> {
		sessions::validate_session_id(&id)?;
		let folder = self
			.state
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let folder_root = Utf8PathBuf::from(folder.folder.path.clone());
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		let LoadedSession { header, records } = sessions::load(&dir, &id).await?;

		self.abort_inner().await;
		let mut messages: Vec<ChatMessage> = vec![ChatMessage::System {
			content: PHASE_6_0_SYSTEM_PROMPT.to_string(),
		}];
		// Reconstruct the chat history from the persisted records.
		// Tool messages need to know their `tool_call_id`, which
		// the persisted Assistant record carries verbatim — we
		// echo it onto the rebuilt `ChatMessage::Tool`.
		for record in &records {
			match record {
				SessionRecord::User { text } => {
					messages.push(ChatMessage::User { content: text.clone() });
				}
				SessionRecord::Assistant {
					content,
					tool_calls,
					thinking: _,
				} => {
					messages.push(ChatMessage::Assistant {
						content: content.clone(),
						tool_calls: tool_calls.clone(),
					});
				}
				SessionRecord::Tool { tool_call_id, content } => {
					messages.push(ChatMessage::Tool {
						tool_call_id: tool_call_id.clone(),
						content: content.clone(),
					});
				}
				SessionRecord::TitleUpdate { .. } => {}
			}
		}
		let summary = SessionSummary {
			id: header.id.clone(),
			title: header.title.clone(),
			created_at_ms: header.created_at_ms,
			updated_at_ms: header.updated_at_ms,
		};
		let session = Session {
			header,
			session_dir: Some(dir),
			messages,
			persisted_records: records.len() as u32,
			auto_rename_pending: false,
		};
		*self.state.session.lock().await = session;

		// Tell the panel to clear + reload, then fan out the
		// records as the same events a live turn would emit.
		// `SessionLoaded` carries the metadata so the sticky
		// header doesn't need a follow-up IPC round trip.
		let _ = self.state.events.send(CoderEvent::SessionLoaded {
			id: summary.id.clone(),
			title: summary.title.clone(),
			created_at_ms: summary.created_at_ms,
			updated_at_ms: summary.updated_at_ms,
		});
		for record in records {
			emit_replay_events(&self.state.events, record);
		}
		Ok(summary)
	}

	/// Delete a persisted session under the active workspace
	/// folder. Idempotent. If the deleted session is the one
	/// currently mounted in memory, replace the in-memory session
	/// with a blank one and emit `SessionLoaded` for it so the
	/// panel resets.
	pub async fn delete_session(&self, id: String) -> Result<(), CoderError> {
		sessions::validate_session_id(&id)?;
		let folder = self
			.state
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let folder_root = Utf8PathBuf::from(folder.folder.path.clone());
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		sessions::delete(&dir, &id).await?;
		{
			let mut session = self.state.session.lock().await;
			if session.header.id == id {
				*session = Session::new_blank();
			}
		}
		let _ = self.state.events.send(CoderEvent::SessionListChanged);
		Ok(())
	}

	pub async fn send(&self, text: String) -> Result<(), CoderError> {
		// Reject double-sends. The frontend disables the composer
		// while a turn runs; this is the backend belt-and-brace.
		{
			let turn = self.state.turn.lock().await;
			if turn.cancel.is_some() {
				return Err(CoderError::Internal("a turn is already running".into()));
			}
		}
		// Bail early if there's no signed-in session — surface a
		// clean error instead of letting the inference layer fail
		// on the first request.
		if !self.state.auth.has_valid_session().await {
			return Err(CoderError::NotSignedIn);
		}
		let folder = self
			.state
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let folder_root = Utf8PathBuf::from(folder.folder.path.clone());
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);

		let cancel = CancellationToken::new();
		{
			let mut turn = self.state.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}

		// Bind / prep the session: first `send` allocates the
		// title and locks the sessions dir; subsequent sends just
		// append.
		let (auto_rename_after, summary_to_announce) = {
			let mut session = self.state.session.lock().await;
			let needs_loaded_event = session.header.title.is_empty() && session.persisted_records == 0;
			if session.session_dir.is_none() {
				session.session_dir = Some(dir.clone());
			}
			if session.header.title.is_empty() {
				session.header.title = session_title_from_prompt(&text);
				session.auto_rename_pending = true;
			}
			session.header.updated_at_ms = current_time_ms();
			let auto_rename = session.auto_rename_pending;
			let summary = if needs_loaded_event {
				Some(session.summary())
			} else {
				None
			};
			(auto_rename, summary)
		};
		if let Some(summary) = summary_to_announce {
			// Fresh session graduating to "first message landed".
			// Tell the UI so the sticky header switches from
			// "untitled" → the truncated prompt and the sessions
			// list picks it up.
			let _ = self.state.events.send(CoderEvent::SessionLoaded {
				id: summary.id.clone(),
				title: summary.title.clone(),
				created_at_ms: summary.created_at_ms,
				updated_at_ms: summary.updated_at_ms,
			});
			let _ = self.state.events.send(CoderEvent::SessionListChanged);
		}

		// Append the user message to in-memory chat history + the
		// session JSONL. The disk write is best-effort: a failure
		// only loses the user's prompt from the saved transcript,
		// the in-memory turn proceeds.
		{
			let mut session = self.state.session.lock().await;
			session.messages.push(ChatMessage::User { content: text.clone() });
			let header = session.header.clone();
			let dir = session
				.session_dir
				.clone()
				.expect("session_dir set above before this point");
			drop(session);
			if let Err(err) = sessions::append_record(&dir, &header, &SessionRecord::User { text: text.clone() }).await {
				tracing::warn!(error = %err, "failed to persist user message");
			} else {
				let mut session = self.state.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}

		let user_id = new_message_id();
		let _ = self.state.events.send(CoderEvent::UserMessage {
			id: user_id,
			text: text.clone(),
		});

		let state = self.state.clone();
		let cancel_outer = cancel.clone();
		tokio::spawn(async move {
			let result = run_turn(&state, cancel_outer).await;
			state.turn.lock().await.cancel = None;
			match result {
				Ok(()) => {
					let _ = state.events.send(CoderEvent::TurnComplete);
					if auto_rename_after {
						spawn_auto_rename(state.clone());
					}
				}
				Err(CoderError::Aborted) => {
					let _ = state.events.send(CoderEvent::Aborted);
				}
				Err(err) => {
					tracing::warn!(error = %err, "coder turn failed");
					let _ = state.events.send(CoderEvent::Error {
						message: err.to_string(),
					});
				}
			}
		});

		Ok(())
	}

	pub fn abort(&self) {
		// Cheap synchronous variant for the Tauri command path —
		// just trip the token; the spawned turn observes it on the
		// next `select!` and exits.
		if let Ok(turn) = self.state.turn.try_lock() {
			if let Some(token) = turn.cancel.as_ref() {
				token.cancel();
			}
		}
	}

	async fn abort_inner(&self) {
		let turn = self.state.turn.lock().await;
		if let Some(token) = turn.cancel.as_ref() {
			token.cancel();
		}
	}

	pub fn subscribe(&self) -> broadcast::Receiver<CoderEvent> {
		self.state.events.subscribe()
	}
}

async fn run_turn(state: &Arc<CoderState>, cancel: CancellationToken) -> Result<(), CoderError> {
	let tool_defs = state.tools.definitions();
	for _iter in 0..MAX_TURN_ITERATIONS {
		if cancel.is_cancelled() {
			return Err(CoderError::Aborted);
		}
		let messages = state.session.lock().await.messages.clone();

		// One stable id per assistant message, shared between the
		// `start`, every content / thinking `delta`, and the final
		// `end` event so the frontend can reconcile by id (see the
		// `tool_call` / `tool_result` pattern). A fresh id every
		// loop iteration — multi-iteration turns with tool calls
		// produce multiple assistant messages.
		let assistant_id = new_message_id();
		let content_started = std::sync::atomic::AtomicBool::new(false);
		let thinking_emitted = std::sync::atomic::AtomicBool::new(false);
		let events = state.events.clone();
		let id_for_cb = assistant_id.clone();
		let response = state
			.inference
			.chat_completion_stream(
				DEFAULT_LARGE_MODEL,
				&messages,
				&tool_defs,
				&cancel,
				|event| match event {
					StreamEvent::ContentDelta { delta } => {
						if !content_started.swap(true, std::sync::atomic::Ordering::Relaxed) {
							let _ = events.send(CoderEvent::AssistantMessageStart { id: id_for_cb.clone() });
						}
						let _ = events.send(CoderEvent::AssistantMessageDelta {
							id: id_for_cb.clone(),
							delta: delta.to_string(),
						});
					}
					StreamEvent::ThinkingDelta { delta } => {
						// Thinking arrives before content on every
						// reasoning-model provider we know of. Fire
						// `AssistantMessageStart` on the first thinking
						// delta too — that way the panel inserts the
						// row early, the user sees the thinking block
						// land, and content streams into the same row
						// when it eventually arrives.
						if !content_started.swap(true, std::sync::atomic::Ordering::Relaxed) {
							let _ = events.send(CoderEvent::AssistantMessageStart { id: id_for_cb.clone() });
						}
						thinking_emitted.store(true, std::sync::atomic::Ordering::Relaxed);
						let _ = events.send(CoderEvent::AssistantThinkingDelta {
							id: id_for_cb.clone(),
							delta: delta.to_string(),
						});
					}
					// Tool-call deltas are intentionally not surfaced.
					// The runner buffers them inside the inference
					// client and dispatches once the whole call is
					// assembled — partial JSON arguments aren't
					// useful to render.
					StreamEvent::ToolCallDelta { .. } => {}
				},
			)
			.await?;

		state.session.lock().await.messages.push(response_to_message(&response));
		persist_assistant_record(state, &response).await;

		// Always emit `End` *if* we ever started a bubble; otherwise
		// the frontend would be stuck with an empty placeholder.
		// The sequencing is `Start (once) → N × Delta (content
		// and/or thinking) → End` — the UI uses `End.text` /
		// `End.thinking` as the canonical replacements so any drift
		// between concatenated deltas and the final assembly heals
		// on close.
		if content_started.into_inner() {
			// Drop empty-string thinking on the canonical message —
			// `Some("")` would force the UI to render an empty
			// "Thoughts" disclosure for messages that didn't actually
			// reason. Only carry the field when we genuinely saw
			// reasoning bytes.
			let canonical_thinking = if thinking_emitted.into_inner() {
				response.thinking.clone()
			} else {
				None
			};
			let _ = state.events.send(CoderEvent::AssistantMessageEnd {
				id: assistant_id,
				text: response.content.clone().unwrap_or_default(),
				thinking: canonical_thinking,
			});
		}

		if response.tool_calls.is_empty() {
			return Ok(());
		}

		for call in &response.tool_calls {
			if cancel.is_cancelled() {
				return Err(CoderError::Aborted);
			}
			let args = parse_tool_args(&call.function);
			let _ = state.events.send(CoderEvent::ToolCall {
				id: call.id.clone(),
				name: call.function.name.clone(),
				args: args.clone(),
			});

			let outcome = state.tools.dispatch(&call.function.name, &args, &cancel).await;
			match outcome {
				Ok(value) => {
					let content = value.to_string();
					let _ = state.events.send(CoderEvent::ToolResult {
						id: call.id.clone(),
						result: value,
						is_error: false,
					});
					state.session.lock().await.messages.push(ChatMessage::Tool {
						tool_call_id: call.id.clone(),
						content: content.clone(),
					});
					persist_tool_record(state, &call.id, &content).await;
				}
				Err(CoderError::Aborted) => return Err(CoderError::Aborted),
				Err(err) => {
					let payload = serde_json::json!({ "error": err.to_string() });
					let content = payload.to_string();
					let _ = state.events.send(CoderEvent::ToolResult {
						id: call.id.clone(),
						result: payload,
						is_error: true,
					});
					state.session.lock().await.messages.push(ChatMessage::Tool {
						tool_call_id: call.id.clone(),
						content: content.clone(),
					});
					persist_tool_record(state, &call.id, &content).await;
				}
			}
		}
	}
	let _ = state.events.send(CoderEvent::Error {
		message: format!(
			"agent loop exceeded {} iterations without finishing",
			MAX_TURN_ITERATIONS
		),
	});
	Err(CoderError::Internal(format!(
		"loop iteration cap reached ({})",
		MAX_TURN_ITERATIONS
	)))
}

/// Append an `Assistant` record to the active session's JSONL.
/// Best-effort: a write failure logs but doesn't fail the turn.
async fn persist_assistant_record(state: &Arc<CoderState>, response: &AssistantResponse) {
	let (dir, header) = {
		let session = state.session.lock().await;
		let Some(dir) = session.session_dir.clone() else {
			return;
		};
		(dir, session.header.clone())
	};
	let record = SessionRecord::Assistant {
		content: response.content.clone(),
		thinking: response.thinking.clone(),
		tool_calls: response.tool_calls.clone(),
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(error = %err, "failed to persist assistant message");
		return;
	}
	let mut session = state.session.lock().await;
	session.persisted_records = session.persisted_records.saturating_add(1);
}

async fn persist_tool_record(state: &Arc<CoderState>, tool_call_id: &str, content: &str) {
	let (dir, header) = {
		let session = state.session.lock().await;
		let Some(dir) = session.session_dir.clone() else {
			return;
		};
		(dir, session.header.clone())
	};
	let record = SessionRecord::Tool {
		tool_call_id: tool_call_id.to_string(),
		content: content.to_string(),
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(error = %err, "failed to persist tool result");
		return;
	}
	let mut session = state.session.lock().await;
	session.persisted_records = session.persisted_records.saturating_add(1);
}

/// Spawn the post-first-turn auto-rename pass. Calls the fast
/// model with a tight prompt asking for a 4-6 word title, then
/// persists the result via a `TitleUpdate` record + a
/// `SessionTitleUpdated` event. Failures are logged at info level
/// — the truncated-prompt title is a perfectly serviceable
/// fallback.
fn spawn_auto_rename(state: Arc<CoderState>) {
	tokio::spawn(async move {
		// Snapshot the chat history without holding the session
		// lock across the LLM call — turns / aborts must be able
		// to grab it freely while we wait on the network.
		let (dir, header_snapshot, transcript) = {
			let mut session = state.session.lock().await;
			session.auto_rename_pending = false;
			let Some(dir) = session.session_dir.clone() else {
				return;
			};
			(dir, session.header.clone(), summarise_transcript(&session.messages))
		};
		if transcript.is_empty() {
			return;
		}
		let messages = vec![
			ChatMessage::System {
				content: AUTO_RENAME_SYSTEM_PROMPT.to_string(),
			},
			ChatMessage::User { content: transcript },
		];
		let cancel = CancellationToken::new();
		let response = match state
			.inference
			.chat_completion(DEFAULT_FAST_MODEL, &messages, &[], &cancel)
			.await
		{
			Ok(resp) => resp,
			Err(err) => {
				tracing::info!(error = %err, "auto-rename: fast-model call failed; keeping fallback title");
				return;
			}
		};
		let Some(raw_title) = response.content else {
			return;
		};
		let new_title = sanitise_auto_title(&raw_title);
		if new_title.is_empty() {
			return;
		}
		// Re-check: the user might have opened a different
		// session while we were waiting on the model. Only apply
		// when the active session is still the one we started.
		let mut session = state.session.lock().await;
		if session.header.id != header_snapshot.id {
			return;
		}
		if session.header.title == new_title {
			return;
		}
		session.header.title = new_title.clone();
		session.header.updated_at_ms = current_time_ms();
		let header_for_disk = session.header.clone();
		drop(session);
		if let Err(err) = sessions::append_record(
			&dir,
			&header_for_disk,
			&SessionRecord::TitleUpdate {
				title: new_title.clone(),
			},
		)
		.await
		{
			tracing::warn!(error = %err, "auto-rename: failed to persist new title");
			return;
		}
		let _ = state.events.send(CoderEvent::SessionTitleUpdated {
			id: header_for_disk.id,
			title: new_title,
		});
		let _ = state.events.send(CoderEvent::SessionListChanged);
	});
}

/// One-shot system prompt for the auto-rename pass. Kept tight on
/// purpose — we want a flat string, not a paragraph of preamble.
const AUTO_RENAME_SYSTEM_PROMPT: &str = "You are a title generator. Given a short transcript of one turn between a user and a coding assistant, return a 4 to 6 word title for the conversation. Output the title only, with no quotes, no period, no markdown, and no preamble.";

/// Cheap projection of `messages` for the rename pass: collapse
/// everything to plain "user: …" / "assistant: …" lines, capped
/// to a few thousand chars so we don't pass an entire turn's
/// worth of tool I/O to the fast model.
fn summarise_transcript(messages: &[ChatMessage]) -> String {
	const TRANSCRIPT_MAX_CHARS: usize = 4_000;
	let mut out = String::new();
	for msg in messages {
		match msg {
			ChatMessage::System { .. } => continue,
			ChatMessage::User { content } => {
				out.push_str("user: ");
				out.push_str(content);
				out.push('\n');
			}
			ChatMessage::Assistant { content, .. } => {
				if let Some(text) = content {
					out.push_str("assistant: ");
					out.push_str(text);
					out.push('\n');
				}
			}
			ChatMessage::Tool { .. } => continue,
		}
		if out.len() >= TRANSCRIPT_MAX_CHARS {
			break;
		}
	}
	if out.len() > TRANSCRIPT_MAX_CHARS {
		let mut idx = TRANSCRIPT_MAX_CHARS;
		while idx > 0 && !out.is_char_boundary(idx) {
			idx -= 1;
		}
		out.truncate(idx);
	}
	out
}

/// Strip the rough edges off an LLM-generated title — surrounding
/// quotes, trailing punctuation, leading list bullets — and cap
/// length. We don't try to translate ALL CAPS to title case; the
/// model picks its own style and that's fine.
fn sanitise_auto_title(raw: &str) -> String {
	const MAX_CHARS: usize = 80;
	let trimmed = raw.trim();
	let trimmed = trimmed.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*');
	let trimmed = trimmed.trim_end_matches(['.', ',', ':', ';']);
	let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
	if collapsed.chars().count() <= MAX_CHARS {
		return collapsed;
	}
	let mut out: String = collapsed.chars().take(MAX_CHARS).collect();
	out.push('…');
	out
}

/// Re-emit the events the panel would have seen for one persisted
/// session record. Fires assistant content as one final
/// (Start, End) pair — no per-token replay, since the user has
/// already seen it stream and we don't have the original timing.
fn emit_replay_events(events: &broadcast::Sender<CoderEvent>, record: SessionRecord) {
	match record {
		SessionRecord::User { text } => {
			let _ = events.send(CoderEvent::UserMessage {
				id: new_message_id(),
				text,
			});
		}
		SessionRecord::Assistant {
			content,
			thinking,
			tool_calls,
		} => {
			let id = new_message_id();
			let has_text = content.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
			let has_thinking = thinking.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
			if has_text || has_thinking {
				let _ = events.send(CoderEvent::AssistantMessageStart { id: id.clone() });
				let _ = events.send(CoderEvent::AssistantMessageEnd {
					id,
					text: content.unwrap_or_default(),
					thinking: thinking.filter(|t| !t.is_empty()),
				});
			}
			for call in tool_calls {
				let args = parse_tool_args(&call.function);
				let _ = events.send(CoderEvent::ToolCall {
					id: call.id.clone(),
					name: call.function.name,
					args,
				});
			}
		}
		SessionRecord::Tool { tool_call_id, content } => {
			// `content` may not be valid JSON (the model wrote
			// raw bytes for a tool output we serialised as a
			// fallback). In that case, surface the raw string —
			// the panel renders it inside a `<pre>` either way.
			let result = match serde_json::from_str::<Value>(&content) {
				Ok(value) => value,
				Err(_) => Value::String(content),
			};
			// We don't persist `is_error` — derive it: the result
			// looks like `{"error":"…"}` for failures and
			// arbitrary JSON otherwise. Close enough for replay
			// purposes (the panel's sole use is the red-tinted
			// styling on the `tool` row).
			let is_error = matches!(&result, Value::Object(map) if map.contains_key("error") && map.len() == 1);
			let _ = events.send(CoderEvent::ToolResult {
				id: tool_call_id,
				result,
				is_error,
			});
		}
		SessionRecord::TitleUpdate { .. } => {
			// Title is already reflected in the header we sent
			// with `SessionLoaded`; no follow-up needed at the
			// per-record level.
		}
	}
}

fn response_to_message(response: &AssistantResponse) -> ChatMessage {
	ChatMessage::Assistant {
		content: response.content.clone(),
		tool_calls: response.tool_calls.clone(),
	}
}

/// `function.arguments` is a JSON-encoded string per OpenAI's wire
/// convention. Decode it lazily; if it fails to parse fall back to
/// an empty object so the tool dispatcher reports a clean
/// `InvalidToolArgs` error instead of a low-level decode panic.
fn parse_tool_args(call: &FunctionCall) -> Value {
	if call.arguments.trim().is_empty() {
		return Value::Object(Default::default());
	}
	serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|err| {
		tracing::warn!(
			tool = %call.name,
			error = %err,
			raw = %call.arguments,
			"could not parse tool-call arguments as JSON; passing empty object"
		);
		Value::Object(Default::default())
	})
}

fn new_message_id() -> String {
	// 64-bit nanosecond timestamp suffices for a single-process
	// session — collisions would require two events in the same
	// nanosecond, which can't happen on the loop's single-threaded
	// emitter path.
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	format!("m-{now:x}")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sanitise_strips_decorations() {
		assert_eq!(
			sanitise_auto_title("\"Implement bucket sync\""),
			"Implement bucket sync"
		);
		assert_eq!(sanitise_auto_title("**Rename moon-agent.**"), "Rename moon-agent");
		assert_eq!(sanitise_auto_title("  spaced  out  "), "spaced out");
	}

	#[test]
	fn sanitise_truncates_long_titles() {
		let long = "word ".repeat(50);
		let out = sanitise_auto_title(&long);
		assert!(out.ends_with('…'));
	}

	#[test]
	fn summarise_skips_system_and_tool_messages() {
		let msgs = vec![
			ChatMessage::System {
				content: "system prompt body".into(),
			},
			ChatMessage::User {
				content: "do thing".into(),
			},
			ChatMessage::Tool {
				tool_call_id: "x".into(),
				content: "tool body".into(),
			},
			ChatMessage::Assistant {
				content: Some("done".into()),
				tool_calls: Vec::new(),
			},
		];
		let summary = summarise_transcript(&msgs);
		assert!(!summary.contains("system prompt body"));
		assert!(!summary.contains("tool body"));
		assert!(summary.contains("user: do thing"));
		assert!(summary.contains("assistant: done"));
	}
}
