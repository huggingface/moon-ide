//! The agent loop.
//!
//! `Coder` owns the in-memory session, the inference client, the
//! tool registry, and the cancellation handle for the active turn.
//! All UI-facing state changes happen via [`CoderEvent`] pushes on
//! the broadcast channel the Tauri layer subscribes to.
//!
//! Loop shape (see `specs/coder.md` § Loop shape):
//!
//! 1. Append the user message to `messages`.
//! 2. POST `chat/completions` with `messages` + tool defs.
//! 3. If the response has tool calls, dispatch each via
//!    [`ToolRegistry`], append the assistant message + tool result
//!    messages to `messages`, loop.
//! 4. If the response is text-only, append the assistant message,
//!    emit `TurnComplete`, exit.
//! 5. Cap iterations at [`MAX_TURN_ITERATIONS`] so a misbehaving
//!    model can't run forever.

use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use crate::auth::{Authenticator, DeviceCode, HfIdentity};
use crate::defaults::{DEFAULT_LARGE_MODEL, MAX_TURN_ITERATIONS, PHASE_6_0_SYSTEM_PROMPT};
use crate::error::CoderError;
use crate::event::{CoderEvent, CoderStatus};
use crate::inference::{AssistantResponse, ChatMessage, FunctionCall, InferenceClient};
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
	/// Same path AppState computes once at startup and `lsp.rs`
	/// already consumes — see [`AppState::workspace_state_dir`].
	workspaces_dir: Utf8PathBuf,
}

/// Per-turn cancellation token + "is anything running right now?"
/// flag. Held under one mutex so `abort` and `send` race on the same
/// lock, avoiding the "abort fires between status check and spawn"
/// hole.
#[derive(Default)]
struct TurnState {
	cancel: Option<CancellationToken>,
}

/// In-memory session. 6.0 holds one session for the lifetime of the
/// process; persistence + multi-session land in 6.3.
struct Session {
	messages: Vec<ChatMessage>,
}

impl Session {
	fn new() -> Self {
		Self {
			messages: vec![ChatMessage::System {
				content: PHASE_6_0_SYSTEM_PROMPT.to_string(),
			}],
		}
	}
}

/// Public alias kept for symmetry with how the Tauri layer used to
/// reach the inner type. Removing it later is a non-issue.
pub type Coder = CoderHandle;

impl CoderHandle {
	pub fn new(workspaces: Arc<WorkspaceRegistry>, workspaces_dir: Utf8PathBuf) -> Result<Self, CoderError> {
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
				session: Arc::new(Mutex::new(Session::new())),
				workspaces,
				workspaces_dir,
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
		// a fresh conversation. Keeps the panel simple (no leftover
		// "[user] hi" from a previous user when account-switching).
		*self.state.session.lock().await = Session::new();
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

		let cancel = CancellationToken::new();
		{
			let mut turn = self.state.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}

		let user_id = new_message_id();
		{
			let mut session = self.state.session.lock().await;
			session.messages.push(ChatMessage::User { content: text.clone() });
		}
		let _ = self.state.events.send(CoderEvent::UserMessage { id: user_id, text });

		let state = self.state.clone();
		let cancel_outer = cancel.clone();
		tokio::spawn(async move {
			let result = run_turn(&state, cancel_outer).await;
			state.turn.lock().await.cancel = None;
			match result {
				Ok(()) => {
					let _ = state.events.send(CoderEvent::TurnComplete);
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
		let response = state
			.inference
			.chat_completion(DEFAULT_LARGE_MODEL, &messages, &tool_defs, &cancel)
			.await?;

		state.session.lock().await.messages.push(response_to_message(&response));

		if let Some(text) = response.content.as_ref() {
			let _ = state.events.send(CoderEvent::AssistantMessage {
				id: new_message_id(),
				text: text.clone(),
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
						content,
					});
				}
				Err(CoderError::Aborted) => return Err(CoderError::Aborted),
				Err(err) => {
					let payload = serde_json::json!({ "error": err.to_string() });
					let _ = state.events.send(CoderEvent::ToolResult {
						id: call.id.clone(),
						result: payload.clone(),
						is_error: true,
					});
					state.session.lock().await.messages.push(ChatMessage::Tool {
						tool_call_id: call.id.clone(),
						content: payload.to_string(),
					});
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
