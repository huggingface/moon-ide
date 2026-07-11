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

use std::collections::HashMap;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use moon_core::WorkspaceRegistry;
use serde_json::Value;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::auth::{Authenticator, DeviceCode, HfIdentity};
use crate::defaults::{MAX_TURN_ITERATIONS, PHASE_6_0_SYSTEM_PROMPT};
use crate::error::CoderError;
use crate::event::{CoderEvent, CoderEventEnvelope, CoderStatus, TokenUsageSource};
use crate::folder_summary::FolderSummaryService;
use crate::inference::{
	AssistantResponse, ChatMessage, FunctionCall, ImageAttachment, InferenceClient, StreamEvent, TokenUsage,
};
use crate::models::{self, CoderModels, ResolvedProvider, SharedCoderModels};
use crate::prompts::{ask_user_tool_definition, PromptOutcome, PromptResponse, QuestionAnswer};
use crate::providers::{self, ProviderKeyring};
use crate::sessions::{
	self, current_time_ms, new_session_id, session_title_from_prompt, sessions_dir, subagent_session_dir,
	BashTargetOverride, LoadedSession, SessionHeader, SessionRecord, SessionSummary, SESSION_SCHEMA_VERSION,
};
use crate::subagent::{build_subagent_spec, run_subagent, task_tool_definition};
use crate::tools::{CoderMode, ToolContext, ToolRegistry};
use moon_core::WorkspaceFolderEntry;
use serde_json::json;
use tokio::sync::Semaphore;

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
///
/// **Multi-session model**: every bound workspace folder gets its
/// own [`FolderSession`] (one in-memory `Session` + one
/// `TurnState`), kept in `sessions_by_folder`. Switching the active
/// workspace folder doesn't touch other folders' sessions, so an
/// agent running in folder X keeps streaming events while the user
/// is browsing folder Y. Events on the broadcast channel carry the
/// folder string they belong to (see [`CoderEventEnvelope`]) so
/// the frontend can route them into per-folder UI buckets.
struct CoderState {
	auth: Authenticator,
	inference: InferenceClient,
	tools: ToolRegistry,
	events: broadcast::Sender<CoderEventEnvelope>,
	/// Per-folder session + turn state. Lazy-created on the first
	/// command that targets a given folder; survives across
	/// folder switches so background turns aren't interrupted.
	/// Keyed by absolute path (the same string used in
	/// `WorkspaceFolder.path`).
	sessions_by_folder: Arc<RwLock<HashMap<Utf8PathBuf, Arc<FolderSession>>>>,
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
	/// Per-machine cache for bound-folder descriptions used in the
	/// "Bound folders" section of the parent's system prompt.
	/// Owned via `Arc` so the background generation tasks (one per
	/// in-flight folder) can share it cheaply.
	folder_summaries: Arc<FolderSummaryService>,
	/// User's current model picks + `bill_to` org + user-added
	/// providers. Shared with [`InferenceClient`] so a settings
	/// flip reaches both the model selection (runner reads at
	/// turn-start) and the per-request route resolution (client
	/// reads on every send) without re-wiring anything.
	models: SharedCoderModels,
	/// Per-provider API keys, mirrored from the OS keyring.
	/// Shared with [`InferenceClient`] so a `coder_set_provider_api_key`
	/// flip applies to the very next request. Held here too so
	/// the auth commands can read / mutate it without going
	/// through the inference client.
	provider_keys: ProviderKeyring,
	/// HF Hub bucket sync. Holds the debounce queue + the HTTP
	/// client used for `/api/buckets/*` round-trips. Drives both
	/// the per-turn autosync (runner hook in [`Coder::send`]
	/// continuations) and the panel's manual / "Sync all"
	/// buttons (Tauri commands in `src-tauri/src/commands/coder.rs`).
	pub(crate) hub_sync: crate::hub_sync::HubSync,
	/// Orchestrator → worker registry (ADR 0030). Maps each
	/// orchestrator session id to the set of worker session ids it
	/// spawned, so the events-as-messages feeder can filter the
	/// broadcast for just its workers and forward dispatch packets
	/// back to the orchestrator. Lives on `CoderState` (not the
	/// orchestrator's `SessionRuntime`) so the feeder task can read
	/// it without holding a session lock.
	coordinator_workers: Arc<RwLock<HashMap<String, CoordinatorWorkers>>>,
}

/// Tracks one orchestrator's spawned workers + whether the dispatch
/// feeder task is already running for it.
#[derive(Default)]
struct CoordinatorWorkers {
	worker_ids: std::collections::HashSet<String>,
	feeder_running: bool,
}

/// One concurrently-runnable session in a folder. Holds the
/// in-memory `Session` + its `TurnState` under separate mutexes
/// so `abort` and `send` race on the same `TurnState` lock
/// without holding the session while waiting for it (and
/// inversely, the session can be updated mid-turn without
/// contending with abort).
///
/// Multiple `SessionRuntime`s can live under one [`FolderSession`]
/// — each one is independently runnable, with its own cancel
/// token. See [ADR 0016](../../specs/decisions/0016-coder-concurrent-sessions.md).
struct SessionRuntime {
	session: Mutex<Session>,
	turn: Mutex<TurnState>,
	/// In-flight `ask_user` prompts parked on a `oneshot`, awaiting
	/// the human. At most one at a time (the loop is single-turn);
	/// resolved either by `coder_respond_to_prompt` (the user
	/// answered the card) or by `send`'s skip path (the user sent a
	/// normal composer message instead). See [`crate::prompts`].
	prompts: crate::prompts::PromptRegistry,
}

impl SessionRuntime {
	fn new(session: Session) -> Self {
		Self {
			session: Mutex::new(session),
			turn: Mutex::new(TurnState::default()),
			prompts: crate::prompts::PromptRegistry::default(),
		}
	}
}

/// Per-folder runtime state: many concurrently-runnable
/// [`SessionRuntime`]s, plus a pointer to the one the panel is
/// currently mounted on (the *visible* session).
///
/// The previous shape (one `Mutex<Session>` + one `Mutex<TurnState>`
/// per folder) forced "one running turn per folder" because the
/// session was a shared mutable slot: starting / opening another
/// one had to first cancel the running turn or it would write
/// into the new session. Splitting into per-id runtimes lets a
/// background turn keep writing to *its own* `Session` while the
/// user makes a new session visible. See [ADR 0016].
///
/// `visible` is `None` only on a brand-new folder we've never
/// routed a command for. The first `active_visible_runtime`
/// resolves it by allocating a blank runtime and pointing
/// `visible` at it.
struct FolderSession {
	runtimes: RwLock<HashMap<String, Arc<SessionRuntime>>>,
	visible: RwLock<Option<String>>,
}

impl FolderSession {
	fn new() -> Self {
		Self {
			runtimes: RwLock::new(HashMap::new()),
			visible: RwLock::new(None),
		}
	}

	/// Look up a runtime by session id without creating one.
	async fn runtime(&self, session_id: &str) -> Option<Arc<SessionRuntime>> {
		self.runtimes.read().await.get(session_id).cloned()
	}

	/// Insert a runtime under `session_id` (replacing any existing
	/// entry — the caller is responsible for ensuring the old one
	/// is gone or about to be). Returns the inserted `Arc` for
	/// convenience.
	async fn insert_runtime(&self, session_id: String, runtime: Arc<SessionRuntime>) -> Arc<SessionRuntime> {
		self.runtimes.write().await.insert(session_id, runtime.clone());
		runtime
	}

	/// Make `session_id` the visible session. Does not touch any
	/// runtime's turn — background turns keep streaming into their
	/// own buckets on the frontend.
	async fn set_visible(&self, session_id: String) {
		*self.visible.write().await = Some(session_id);
	}

	/// Snapshot of the currently-visible session id, if any.
	async fn visible_session_id(&self) -> Option<String> {
		self.visible.read().await.clone()
	}

	/// Resolve to the visible runtime + its id, allocating a blank
	/// one when no session has been mounted yet (first time we
	/// route a command for this folder).
	async fn visible_runtime(&self) -> (Arc<SessionRuntime>, String) {
		if let Some(id) = self.visible_session_id().await {
			if let Some(rt) = self.runtime(&id).await {
				return (rt, id);
			}
			// Visible pointer drifted (entry removed by
			// `delete_session` without picking a successor); fall
			// through to allocating a fresh blank one.
		}
		let blank = Session::new_blank();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		self.insert_runtime(id.clone(), rt.clone()).await;
		self.set_visible(id.clone()).await;
		(rt, id)
	}

	/// Cancel every running turn under this folder. Used by global
	/// teardown paths (sign-out is the obvious one).
	async fn cancel_all(&self) {
		let runtimes: Vec<Arc<SessionRuntime>> = self.runtimes.read().await.values().cloned().collect();
		for rt in runtimes {
			if let Some(token) = rt.turn.lock().await.cancel.as_ref() {
				token.cancel();
			}
		}
	}
}

/// Per-turn cancellation token + "is anything running right now?"
/// flag. Held under one mutex so `abort` and `send` race on the same
/// lock, avoiding the "abort fires between status check and spawn"
/// hole.
#[derive(Default)]
struct TurnState {
	cancel: Option<CancellationToken>,
}

/// Pre-tagged event sender. One `FolderEventSink` per running
/// turn / sub-agent / auto-rename pass — captures the
/// `(folder, session_id)` pair once so emit sites don't have to
/// thread it through every send call. Sub-agents share their
/// parent's sink so their events arrive in the parent's
/// `(folder, session_id)` UI bucket on the frontend (sub-agents
/// belong to whichever session originated them).
#[derive(Clone)]
pub(crate) struct FolderEventSink {
	sender: broadcast::Sender<CoderEventEnvelope>,
	folder: String,
	session_id: String,
}

impl FolderEventSink {
	pub(crate) fn new(
		sender: broadcast::Sender<CoderEventEnvelope>,
		folder: impl Into<String>,
		session_id: impl Into<String>,
	) -> Self {
		Self {
			sender,
			folder: folder.into(),
			session_id: session_id.into(),
		}
	}

	pub(crate) fn send(&self, event: CoderEvent) {
		let _ = self.sender.send(CoderEventEnvelope {
			folder: self.folder.clone(),
			session_id: self.session_id.clone(),
			event,
		});
	}

	pub(crate) fn folder(&self) -> &str {
		&self.folder
	}
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
	/// Last provider-supplied (or estimated) token usage from
	/// the previous LLM round-trip. Carries across user turns so
	/// the next turn's first iteration can decide whether to
	/// compact before sending. `None` until the very first
	/// response lands.
	last_usage: Option<TokenUsage>,
	/// In-memory todo list maintained by the agent's `todo_write`
	/// tool. Survives compaction (the messages prefix gets
	/// folded; the plan does not) and is reset only when the user
	/// starts a new session. Persisted via
	/// [`SessionRecord::TodosUpdate`] — replay seeds this from
	/// the **last** record on disk.
	todos: Vec<crate::TodoItem>,
	/// User messages typed into the composer while a turn is
	/// already running. The runner drains them into `messages`
	/// (and persists each as a `SessionRecord::User`) at the top
	/// of every `run_turn` iteration — i.e. after the previous
	/// iteration's tool results have settled, before the next LLM
	/// call. That ordering matters: the OpenAI / Anthropic chat
	/// shape forbids a user message between an `assistant` with
	/// `tool_calls` and its `tool` result rows, so persisting at
	/// queue time would corrupt the on-disk transcript and break
	/// session reload. When the model emits a final response with
	/// no tool calls, `run_turn` re-checks this queue before
	/// returning and loops one more iteration if it's non-empty —
	/// otherwise a steer typed during the final streaming message
	/// would sit here forever, since there'd be no next iteration
	/// top to drain it. The spawn task wrapping `run_turn` also
	/// re-checks under the `turn` lock so a steer that slips in
	/// between the in-loop check and the spawn task clearing
	/// `cancel` still earns another `run_turn` invocation.
	/// Pop with [`Coder::unqueue_steer`] (`ArrowUp`
	/// on an empty composer in the panel) to take a queued steer
	/// back before drain. In-memory only; undrained steers don't
	/// hit disk (they live here, not in the JSONL), so a reload
	/// can't recover them — acceptable since the panel pairs
	/// queue-time emission of a [`CoderEvent::UserMessage`] with a
	/// matching [`CoderEvent::SteerDrained`] only when the steer
	/// actually graduates into the chat.
	pending_steers: Vec<PendingSteer>,
}

/// One queued steer waiting to be drained into `session.messages`
/// at the top of the next `run_turn` iteration. Carries the user
/// text plus any images they pasted into the composer while the
/// turn was already running, so the model sees the same shape it
/// would have seen for a regular send. `id` matches the
/// [`CoderEvent::UserMessage`] id the panel rendered when the
/// steer was queued, so [`Coder::unqueue_steer`] can pop the
/// exact entry the user pointed at and [`drain_pending_steers`]
/// can emit a matching [`CoderEvent::SteerDrained`].
#[derive(Debug, Clone)]
struct PendingSteer {
	id: String,
	text: String,
	images: Vec<ImageAttachment>,
}

impl Session {
	/// Make a fresh session shell in the default top-level `Agent`
	/// mode — id allocated, title empty pending the first prompt,
	/// no folder bound. The historical constructor; most sessions
	/// are ordinary `agent` sessions.
	fn new_blank() -> Self {
		Self::new_blank_with_mode(CoderMode::Agent)
	}

	/// Make a fresh session shell in a given top-level mode. The
	/// mode is stamped onto the header (as its wire string, elided
	/// for the `Agent` default to stay byte-compatible with pre-6
	/// sessions) and drives the tool-list + system-prompt selection
	/// in `run_turn`. A `Coordinator` shell gets the coordinator
	/// system prompt as its seed `messages` entry instead of the
	/// base `Agent` prompt.
	fn new_blank_with_mode(mode: CoderMode) -> Self {
		let now = current_time_ms();
		let system_prompt = match mode {
			CoderMode::Agent | CoderMode::Research => PHASE_6_0_SYSTEM_PROMPT.to_string(),
			CoderMode::Coordinator => crate::coordinator::COORDINATOR_SYSTEM_PROMPT.to_string(),
		};
		Self {
			header: SessionHeader {
				schema: SESSION_SCHEMA_VERSION,
				id: new_session_id(),
				// Bound at first-persistence time by `Coder::send`
				// once we know which workspace folder the session
				// is attached to. Left blank here so the freshly-
				// created shell doesn't accidentally claim a path
				// it never wrote to.
				cwd: String::new(),
				title: String::new(),
				created_at_ms: now,
				updated_at_ms: now,
				// Seed value only; the actual model used for any
				// given round-trip is read fresh from
				// [`CoderState::models`] by the runner. This field
				// in the JSONL header is purely informational and
				// reflects what was *possible* at session-creation
				// time, not what every later turn ran against.
				model: crate::defaults::DEFAULT_STANDARD_MODEL.to_string(),
				parent_session_id: None,
				parent_tool_call_id: None,
				subagent_mode: None,
				// Top-level mode stamp. Elided for `Agent` (the
				// default) so ordinary sessions stay byte-
				// compatible with pre-6 transcripts; `Some` for
				// `Coordinator` so the load path picks it up.
				mode: (mode != CoderMode::Agent).then(|| mode.as_wire().to_string()),
				subagent_target_folder: None,
				bash_target_override: None,
				worktree_root: None,
				worktree_branch: None,
				committed_branch: None,
			},
			session_dir: None,
			messages: vec![ChatMessage::System { content: system_prompt }],
			persisted_records: 0,
			auto_rename_pending: false,
			last_usage: None,
			todos: Vec::new(),
			pending_steers: Vec::new(),
		}
	}

	fn summary(&self) -> SessionSummary {
		SessionSummary {
			id: self.header.id.clone(),
			title: self.header.title.clone(),
			created_at_ms: self.header.created_at_ms,
			updated_at_ms: self.header.updated_at_ms,
			worktree_branch: self.header.worktree_branch.clone(),
			committed_branch: self.header.committed_branch.clone(),
			mode: self.header.mode.clone(),
		}
	}
}

/// Public alias kept for symmetry with how the Tauri layer used to
/// reach the inner type. Removing it later is a non-issue.
pub type Coder = CoderHandle;

impl CoderState {
	/// Get the [`FolderSession`] for `folder_path`, creating it on
	/// first call. Cheap-clone return so callers can hold an `Arc`
	/// across `await` boundaries without contending with the map's
	/// `RwLock`.
	async fn folder_session_for(&self, folder_path: &Utf8Path) -> Arc<FolderSession> {
		{
			let by = self.sessions_by_folder.read().await;
			if let Some(existing) = by.get(folder_path) {
				return existing.clone();
			}
		}
		// Two writers can race here — the second one to grab the
		// write lock sees the first's insert and reuses it. Cheap
		// new() means the wasted allocation on the loser doesn't
		// matter, but the entry itself must be insertion-stable
		// so callers always get the same `Arc` back.
		let mut by = self.sessions_by_folder.write().await;
		by.entry(folder_path.to_path_buf())
			.or_insert_with(|| Arc::new(FolderSession::new()))
			.clone()
	}

	/// The folder that owns the coder session list for the current
	/// active folder (ADR 0028 — per-project session scoping). A
	/// worktree folder defers to its **parent project root**, so a
	/// parent and all its worktrees share one session list; any other
	/// folder is its own root. `None` when nothing is active. The
	/// parent fallback to the active folder covers an orphaned
	/// worktree whose parent isn't bound (shouldn't happen post-W.3).
	async fn coder_root_folder(&self) -> Option<Arc<WorkspaceFolderEntry>> {
		let active = self.workspaces.active_folder().await?;
		if let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = &active.folder.origin {
			if let Some(parent) = self.workspaces.folder_for_path(parent_path).await {
				return Some(parent);
			}
		}
		Some(active)
	}

	/// Resolve to `(coder-root folder's FolderSession, folder path)`
	/// or error with `NoActiveFolder`. Used by commands that
	/// operate at the folder level (`list_sessions`, `new_session`,
	/// the runtime-routing inside `open_session` / `delete_session`).
	/// Routes through [`Self::coder_root_folder`] so worktree folders
	/// share their parent project's session list.
	async fn active_folder_session(&self) -> Result<(Arc<FolderSession>, Utf8PathBuf), CoderError> {
		let folder = self.coder_root_folder().await.ok_or(CoderError::NoActiveFolder)?;
		let folder_path = Utf8PathBuf::from(folder.folder.path.clone());
		let session = self.folder_session_for(&folder_path).await;
		Ok((session, folder_path))
	}

	/// Resolve to `(visible SessionRuntime, session id, folder path)`
	/// for the active folder. Used by every panel-driven command
	/// that targets "the session the user is currently looking at":
	/// `send`, `abort`, `active_session`, `unqueue_steer`. Lazy-
	/// creates a blank runtime if the folder has never been
	/// mounted before.
	///
	/// Background tasks (`run_turn`, `run_subagent`,
	/// `spawn_auto_rename`, `maybe_autosync_to_hub`) close over an
	/// `Arc<SessionRuntime>` from when they were spawned and never
	/// re-resolve through this helper, so a folder switch / new-
	/// session click mid-turn doesn't redirect them.
	async fn active_visible_runtime(&self) -> Result<(Arc<SessionRuntime>, String, Utf8PathBuf), CoderError> {
		let (fs, folder_path) = self.active_folder_session().await?;
		let (rt, id) = fs.visible_runtime().await;
		Ok((rt, id, folder_path))
	}

	/// Find the mounted runtime — in **any** folder — that's holding
	/// an `ask_user` prompt parked under `call_id`. The prompt lives
	/// on whichever session originated it, which may no longer be
	/// the visible session (the user switched away to do something
	/// else, then came back to answer). Scanning every folder's
	/// runtimes keeps "answer later, from anywhere" working without
	/// the caller having to know which session owns the prompt.
	async fn runtime_holding_prompt(&self, call_id: &str) -> Option<Arc<SessionRuntime>> {
		let folders: Vec<Arc<FolderSession>> = self.sessions_by_folder.read().await.values().cloned().collect();
		for fs in folders {
			let runtimes: Vec<Arc<SessionRuntime>> = fs.runtimes.read().await.values().cloned().collect();
			for rt in runtimes {
				if rt.prompts.holds(call_id).await {
					return Some(rt);
				}
			}
		}
		None
	}

	/// Find the mounted runtime + its owning folder path for a given
	/// session id, scanning every folder (ADR 0030 — an orchestrator
	/// driving a worker by id shouldn't have to know or care which
	/// folder the worker was filed under). Returns `None` when no
	/// mounted runtime matches — the session may not have been opened
	/// yet, or may live in a folder this process doesn't track.
	async fn runtime_for_session(&self, session_id: &str) -> Option<(Arc<SessionRuntime>, Utf8PathBuf)> {
		let folders: Vec<(Utf8PathBuf, Arc<FolderSession>)> = self
			.sessions_by_folder
			.read()
			.await
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		for (folder_path, fs) in folders {
			if let Some(rt) = fs.runtime(session_id).await {
				return Some((rt, folder_path));
			}
		}
		None
	}
}

impl CoderHandle {
	pub fn new(
		workspaces: Arc<WorkspaceRegistry>,
		workspaces_dir: Utf8PathBuf,
		coder_sessions_dir: Utf8PathBuf,
		folder_summaries_dir: Utf8PathBuf,
		initial_models: CoderModels,
	) -> Result<Self, CoderError> {
		let auth = Authenticator::new()?;
		// Warm the per-provider keyring from the persisted
		// providers list before the inference client starts
		// resolving routes — otherwise the first request after a
		// relaunch would see "no key" for a provider the user
		// already set up.
		let provider_keys = ProviderKeyring::new();
		let provider_ids: Vec<String> = initial_models.providers.iter().map(|p| p.id.clone()).collect();
		provider_keys.warm(provider_ids);
		// Reflect `has_api_key` on the persisted entries so
		// `current_models()` exposes the right state to the picker
		// on first read — the keyring is the source of truth, not
		// `state.json`.
		let mut initial_models = initial_models;
		for provider in &mut initial_models.providers {
			provider.has_api_key = provider_keys.has_key(&provider.id);
		}
		let models = models::shared(initial_models);
		let inference = InferenceClient::new(auth.clone(), models.clone(), provider_keys.clone())?;
		let web = crate::web::WebClient::new()?;
		let tools = ToolRegistry::new(workspaces.clone(), workspaces_dir.clone(), web);
		let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
		let folder_summaries = Arc::new(FolderSummaryService::new(folder_summaries_dir));
		let hub_sync = crate::hub_sync::HubSync::new(
			auth.clone(),
			events.clone(),
			workspaces_dir.clone(),
			coder_sessions_dir.clone(),
		)?;
		Ok(Self {
			state: Arc::new(CoderState {
				auth,
				inference,
				tools,
				events,
				sessions_by_folder: Arc::new(RwLock::new(HashMap::new())),
				workspaces,
				workspaces_dir,
				coder_sessions_dir,
				folder_summaries,
				models,
				provider_keys,
				hub_sync,
				coordinator_workers: Arc::new(RwLock::new(HashMap::new())),
			}),
		})
	}

	/// Access to the workspace's HF Hub bucket sync state. Used
	/// by the Tauri layer (`coder_hub_*` commands) to drive the
	/// connect / autosync / manual-upload affordances. Cheap
	/// clone — every field on [`crate::hub_sync::HubSync`] is
	/// already `Arc`-wrapped where it needs to be.
	pub fn hub_sync(&self) -> crate::hub_sync::HubSync {
		self.state.hub_sync.clone()
	}

	/// Workspace id this handle was wired against. Used by the
	/// hub sync commands to load + persist `WorkspaceSession`
	/// without re-deriving the id from a folder path.
	pub async fn workspace_id(&self) -> String {
		self.state.workspaces.workspace_id().await
	}

	/// Absolute path of the active workspace folder, if any.
	/// Convenience used by the hub sync Tauri commands so the
	/// `src-tauri` layer doesn't need a direct dep on
	/// [`moon_core::WorkspaceRegistry`].
	pub async fn active_folder(&self) -> Option<String> {
		self
			.state
			.workspaces
			.active_folder()
			.await
			.map(|entry| entry.folder.path.clone())
	}

	/// Bulk-upload every top-level session JSONL across every
	/// folder bound to this workspace into the connected HF Hub
	/// bucket. Delegates to [`crate::hub_sync::HubSync::upload_all_sessions`]
	/// after fetching the folder list off the registry — keeps the
	/// `src-tauri` command boilerplate-free and folds the Hub
	/// round-trips so a workspace with N stale sessions doesn't
	/// pay 3·N round-trips.
	pub async fn hub_upload_all_sessions(&self) -> Result<moon_protocol::coder_hub::HubUploadAllSummary, CoderError> {
		let workspace_id = self.state.workspaces.workspace_id().await;
		let folders: Vec<Utf8PathBuf> = self
			.state
			.workspaces
			.folders()
			.await
			.into_iter()
			.map(|entry| Utf8PathBuf::from(&entry.folder.path))
			.collect();
		self.state.hub_sync.upload_all_sessions(&workspace_id, &folders).await
	}

	/// True iff a Tavily API key is currently stored in the
	/// keyring. The panel reads this on the model-settings popover
	/// to flip the web-search section between "set a key" and
	/// "key configured · clear / replace" states. Cheap sync read
	/// of the in-memory cache — no keyring round-trip.
	pub fn web_search_configured(&self) -> bool {
		self.state.tools.web().has_tavily_key()
	}

	/// Persist a new Tavily API key in the OS keyring. Empty /
	/// whitespace-only values are rejected at the [`crate::web::WebClient`]
	/// boundary. After this returns Ok, [`web_search_configured`]
	/// flips to `true` and the next turn advertises `web_search` in
	/// the tool list.
	pub fn set_web_search_key(&self, key: &str) -> Result<(), CoderError> {
		self.state.tools.web().set_tavily_key(key)
	}

	/// Drop the keyring entry. Idempotent. After this returns Ok,
	/// `web_search` disappears from the tool list on the next
	/// turn.
	pub fn clear_web_search_key(&self) -> Result<(), CoderError> {
		self.state.tools.web().clear_tavily_key()
	}

	/// Hot-swap the user-facing model picks for HF.
	/// `standard` / `cheap` / `bill_to` apply only when the active
	/// route is HF; user providers carry their own picks in
	/// `providers[].standard_model` etc. The router-derived
	/// `context_windows` cache is preserved across the swap so a
	/// fresh save from the picker doesn't blow the catalog away
	/// (the picker fetches the catalog in a separate command).
	///
	/// The runner snapshots [`CoderModels`] at the top of each
	/// turn / sub-agent / cheap-helper call so the *next*
	/// round-trip picks up the change; in-flight requests are
	/// untouched. `bill_to` reaches every subsequent request via
	/// the shared handle held inside [`InferenceClient`].
	pub async fn set_user_picks(&self, standard: String, cheap: String, bill_to: Option<String>) {
		{
			let mut m = self.state.models.write().await;
			m.standard = standard;
			m.cheap = cheap;
			m.bill_to = bill_to;
		}
		// Push the new context-window denominator to any folder
		// whose ring is sitting on the previous model's
		// number — without this the ring wouldn't repaint until
		// the user sent another turn.
		self.refresh_token_usage_windows().await;
	}

	/// Replace the per-slug context-window caps. Called from the
	/// picker `Save` flow alongside [`Self::set_user_picks`] /
	/// [`Self::set_providers`]; the caller (the Tauri command)
	/// has already persisted the same map to `state.json`. Each
	/// `0` value is treated as "no cap" by
	/// [`CoderModels::context_window`] so a frontend that fails
	/// to remove a cleared input doesn't lock the runner out.
	///
	/// Refreshes the per-folder usage rings so a cap edit
	/// repaints them immediately — the next turn isn't required
	/// to see the new denominator.
	pub async fn set_context_window_overrides(&self, overrides: std::collections::HashMap<String, u32>) {
		{
			let mut m = self.state.models.write().await;
			m.context_window_overrides = std::sync::Arc::new(overrides);
		}
		self.refresh_token_usage_windows().await;
	}

	/// Replace the user-added providers list + the active
	/// selection in one go. The caller (Tauri command) has
	/// already persisted the same shape to `state.json`; this
	/// just flips the runtime view.
	///
	/// `providers[].has_api_key` flags are re-computed off the
	/// keyring rather than trusted from the caller — the keyring
	/// is the source of truth, and a frontend trying to spoof the
	/// flag shouldn't be able to make the inference client
	/// believe an empty slot has a key.
	///
	/// Side effect: when the active provider id changes, kicks
	/// off a best-effort background catalog fetch so
	/// [`CoderModels::context_windows`] sees the new route's
	/// slugs before the next turn lands. Without this the user
	/// could flip from HF to OpenRouter, send a message
	/// immediately, and watch the ring fall back to the
	/// static 128k for the entire first turn (until they
	/// happen to open the picker, which would refresh the
	/// cache as a side-effect).
	pub async fn set_providers(
		&self,
		mut providers: Vec<moon_protocol::coder_models::CoderProviderConfig>,
		active: Option<String>,
	) {
		for p in &mut providers {
			p.has_api_key = self.state.provider_keys.has_key(&p.id);
		}
		let active_changed = {
			let mut m = self.state.models.write().await;
			let prev_active = m.active_provider.clone();
			m.providers = providers;
			m.active_provider = active.clone();
			prev_active != active
		};
		// Repaint any folder ring with the new active route's
		// context window — even if the prime below ends up
		// fetching a fresher number, the immediate effect is
		// that the user's previous-model ring stops misleading
		// them. The prime + its own refresh will land later.
		self.refresh_token_usage_windows().await;
		if active_changed {
			self.spawn_prime_context_windows();
		}
	}

	/// Generate a fresh opaque provider id. The Tauri command
	/// uses this to allocate the keyring entry name (under
	/// `service=moon-ide, account=coder-provider:<id>`) before
	/// persisting the config — keeps id generation in one place.
	pub fn new_provider_id(&self) -> String {
		providers::new_provider_id()
	}

	/// Persist a new API key for a provider id. Empty values are
	/// rejected at the keyring boundary. After this returns Ok,
	/// the very next request resolving to this provider picks up
	/// the new key without rewiring.
	pub fn set_provider_api_key(&self, id: &str, key: &str) -> Result<(), CoderError> {
		let result = self.state.provider_keys.set(id, key);
		// Reflect the flag onto the cached models snapshot so the
		// next `current_models()` read by the picker sees the
		// correct state — no need to wait for a `set_providers`
		// round-trip.
		if result.is_ok() {
			let provider_keys = self.state.provider_keys.clone();
			let models = self.state.models.clone();
			let id = id.to_owned();
			tokio::spawn(async move {
				let mut m = models.write().await;
				for p in &mut m.providers {
					if p.id == id {
						p.has_api_key = provider_keys.has_key(&id);
					}
				}
			});
		}
		result
	}

	/// Drop the API key for a provider id. Idempotent — fine to
	/// call on a provider that never had a key (the local-vLLM
	/// case where the user is just removing a stale entry).
	pub fn clear_provider_api_key(&self, id: &str) -> Result<(), CoderError> {
		let result = self.state.provider_keys.clear(id);
		if result.is_ok() {
			let models = self.state.models.clone();
			let id = id.to_owned();
			tokio::spawn(async move {
				let mut m = models.write().await;
				for p in &mut m.providers {
					if p.id == id {
						p.has_api_key = false;
					}
				}
			});
		}
		result
	}

	/// Probe a `(base_url, api_key)` combination before the
	/// picker commits. See [`providers::probe_provider`] for the
	/// fallback order. Builds a fresh `reqwest::Client` for the
	/// probe rather than reusing the inference client's so a
	/// hung probe can't share connection-pool state with live
	/// traffic.
	pub async fn probe_provider(
		&self,
		base_url: &str,
		kind: moon_protocol::coder_models::ProviderKind,
		api_key: Option<&str>,
	) -> Result<moon_protocol::coder_models::ProviderProbeResult, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(CoderError::from)?;
		providers::probe_provider(&http, base_url, kind, api_key).await
	}

	/// Current `CoderModels` snapshot. The Tauri layer reads this
	/// on `coder_status` so the panel can render the active picks
	/// without keeping a parallel cache.
	pub async fn current_models(&self) -> CoderModels {
		self.state.models.read().await.clone()
	}

	/// Best-effort warm of [`CoderModels::context_windows`] for
	/// the currently-active route. Called at startup and on every
	/// active-provider change so the very first turn after a
	/// relaunch / route flip already has authoritative numbers
	/// instead of the static 128k fallback.
	///
	/// Failures (network, 401, 404 on a server that doesn't
	/// expose `/v1/models`) are logged at `debug` and swallowed —
	/// the fallback table still gives the runner a usable
	/// number, and the next turn's response will carry exact
	/// usage from the provider regardless.
	///
	/// Variant for callers that already hold a Tokio runtime
	/// handle (`set_providers` inside an async command). The
	/// Tauri setup hook is **not** one of them — it runs on the
	/// outer thread before `tauri::async_runtime` has been
	/// installed; the desktop layer uses
	/// `tauri::async_runtime::spawn(coder.prime_context_windows())`
	/// to launch the same work on the right reactor.
	pub fn spawn_prime_context_windows(&self) {
		let handle = self.clone();
		tokio::spawn(async move {
			handle.prime_context_windows().await;
		});
	}

	pub async fn prime_context_windows(&self) {
		let route = self.state.models.read().await.resolve_route();
		match route {
			ResolvedProvider::HuggingFace => match self.state.inference.list_hf_models().await {
				Ok(catalog) => {
					let windows = models::context_windows_from_catalog(&catalog);
					{
						let mut m = self.state.models.write().await;
						m.context_windows = models::merge_context_windows(&m.context_windows, windows);
					}
					self.refresh_token_usage_windows().await;
				}
				Err(err) => {
					tracing::debug!(?err, "context-window prime: HF catalog fetch failed; using fallback");
				}
			},
			ResolvedProvider::Custom { id, .. }
			| ResolvedProvider::OpenRouter { id, .. }
			| ResolvedProvider::Anthropic { id, .. } => {
				match self.list_provider_models(&id).await {
					Ok(_) => {
						// `list_provider_models` already merged the fresh
						// windows; just push the updated `context_window`
						// out to any folder session whose ring is sitting
						// on stale numbers from before the prime landed.
						self.refresh_token_usage_windows().await;
					}
					Err(err) => {
						tracing::debug!(
							provider_id = %id,
							?err,
							"context-window prime: provider catalog fetch failed; using fallback"
						);
					}
				}
			}
		}
	}

	/// Re-emit a [`CoderEvent::TokenUsage`] for every folder
	/// session that already has a `last_usage`, using the
	/// **current** active model's context window. The token
	/// counts (prompt / completion / total / cache) are
	/// preserved — only the `context_window` denominator changes.
	///
	/// Called after every catalog refresh and after model-picks
	/// changes so:
	///
	/// - The ring repaints to the right capacity the moment the
	///   user flips models or the picker fetch lands; they don't
	///   have to send another turn just to see the correct
	///   denominator.
	/// - Sessions restored before the cache was warm (cold first
	///   launch, prime still in flight) get their ring corrected
	///   when the prime finishes, instead of stranding them on
	///   the static 128k fallback until the next turn.
	///
	/// No-op for folder sessions without a `last_usage` — those
	/// haven't had a turn yet, so the ring on the panel is empty
	/// and there's nothing to update. Best-effort: a session
	/// dropping its lock between the snapshot read and the emit
	/// is fine, the next turn refreshes anyway.
	async fn refresh_token_usage_windows(&self) {
		let models = self.state.models.read().await.clone();
		let active_model = models.standard().to_owned();
		let context_window = models.context_window(&active_model);
		let folders: Vec<(Utf8PathBuf, Arc<FolderSession>)> = {
			let by = self.state.sessions_by_folder.read().await;
			by.iter().map(|(p, fs)| (p.clone(), fs.clone())).collect()
		};
		for (folder_path, fs) in folders {
			// Per-session ring repaint: every runtime in the folder
			// has its own last_usage and its own context-ring
			// bucket on the frontend, so we emit one TokenUsage
			// event per runtime that has a number to report.
			let runtimes: Vec<(String, Arc<SessionRuntime>)> = fs
				.runtimes
				.read()
				.await
				.iter()
				.map(|(id, rt)| (id.clone(), rt.clone()))
				.collect();
			for (session_id, rt) in runtimes {
				let usage = match rt.session.lock().await.last_usage {
					Some(u) => u,
					None => continue,
				};
				let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id);
				sink.send(CoderEvent::TokenUsage {
					prompt_tokens: usage.prompt_tokens,
					completion_tokens: usage.completion_tokens,
					total_tokens: usage.total_tokens,
					context_window,
					source: TokenUsageSource::Provider,
					cache_read_tokens: usage.cache_read_input_tokens,
					cache_creation_tokens: usage.cache_creation_input_tokens,
				});
			}
		}
	}

	/// HF router `/v1/models` catalog. Returns the rich shape
	/// (per-provider routes + pricing + throughput) the picker's
	/// HF tab renders.
	///
	/// **Not gated on the active route.** The picker shows both
	/// the HF tab and the user-provider tabs side by side and
	/// the user is allowed to flip between them while editing
	/// the modal — gating here would 500 the HF tab any time
	/// OpenRouter / a local vLLM was the persisted active route,
	/// even though the request itself is just "give me the HF
	/// catalog". User-provider catalogs go through
	/// [`Self::list_provider_models`] (id-keyed); the two
	/// entrypoints exist because the wire shapes differ, not
	/// because the active route picks one.
	///
	/// Side effect: refreshes [`CoderModels::context_windows`]
	/// with the HF entries (merge, not replace) so subsequent
	/// turns size the usage ring / compaction threshold against
	/// authoritative numbers instead of the static fallback
	/// table — useful even when HF isn't currently active, since
	/// the user might flip back.
	pub async fn list_models(&self) -> Result<Vec<moon_protocol::coder_models::RouterModel>, CoderError> {
		let catalog = self.state.inference.list_hf_models().await?;
		let windows = models::context_windows_from_catalog(&catalog);
		let mut m = self.state.models.write().await;
		m.context_windows = models::merge_context_windows(&m.context_windows, windows);
		Ok(catalog)
	}

	/// Flat catalog for a user-added provider. `id` matches one
	/// of `CoderModels::providers[].id`; the runner looks up the
	/// `base_url` and the (optional) API key, then calls
	/// `/v1/models` against the endpoint. Errors propagate
	/// verbatim — a 404 means the server doesn't expose the
	/// catalog endpoint and the user can still type a model slug
	/// directly into the picker field.
	///
	/// Side effect: merges the catalog's per-model
	/// `context_length` into [`CoderModels::context_windows`] so
	/// the very next turn's usage ring + auto-compaction trigger
	/// see the authoritative window for whichever slug the user
	/// just picked. Without this every OpenRouter / LiteLLM /
	/// vLLM model would land in the static-fallback `128k`
	/// branch — wrong for 200k Claude, wrong for 1M GPT-4.1, etc.
	pub async fn list_provider_models(
		&self,
		provider_id: &str,
	) -> Result<Vec<moon_protocol::coder_models::ProviderModelSummary>, CoderError> {
		let snapshot = self.state.models.read().await;
		let entry = snapshot
			.providers
			.iter()
			.find(|p| p.id == provider_id)
			.ok_or_else(|| CoderError::Internal(format!("unknown provider id: {provider_id}")))?;
		let base_url = entry.base_url.clone();
		let kind = entry.kind;
		drop(snapshot);
		let api_key = self.state.provider_keys.get(provider_id);
		let catalog = self
			.state
			.inference
			.list_provider_models(&base_url, api_key.as_deref(), kind)
			.await?;
		let windows = models::context_windows_from_provider_catalog(&catalog);
		if !windows.is_empty() {
			let mut m = self.state.models.write().await;
			m.context_windows = models::merge_context_windows(&m.context_windows, windows);
		}
		Ok(catalog)
	}

	pub async fn status(&self) -> Result<CoderStatus, CoderError> {
		let identity = self.state.auth.identity().await?;
		// `signed_in` is route-aware: HF needs OAuth; a user
		// provider just needs a configured key (or a localhost
		// `base_url` where running keyless is conventional). The
		// `identity` field stays HF-only — it's the `HfIdentity`
		// payload the picker renders for the "Bill to" dropdown
		// and the user avatar in the header; off-HF the panel
		// hides that surface.
		let route = self.state.models.read().await.resolve_route();
		let signed_in = match &route {
			ResolvedProvider::HuggingFace => identity.is_some(),
			ResolvedProvider::Custom { id, base_url }
			| ResolvedProvider::OpenRouter { id, base_url }
			| ResolvedProvider::Anthropic { id, base_url } => {
				if self.state.provider_keys.has_key(id) {
					true
				} else {
					is_local_base_url(base_url)
				}
			}
		};
		// `busy` reflects the **active folder's visible session**
		// turn only — the panel mirrors per-folder, per-session UI
		// state, so background sessions in the same folder (or
		// other folders entirely) don't make this session's
		// composer disable. The frontend's sessions-list view
		// surfaces a `running…` pip on every running session row
		// across the folder via the per-bucket event stream.
		//
		// Two-step look-up rather than `visible_runtime()` so we
		// don't lazy-create a blank runtime just to read its
		// `busy` flag — that would litter the folder's runtimes
		// map with empty entries every time the panel polls
		// status on mount.
		let busy = match self.state.workspaces.active_folder().await {
			Some(folder) => {
				let path = Utf8PathBuf::from(folder.folder.path.clone());
				let fs = self.state.folder_session_for(&path).await;
				match fs.visible_session_id().await {
					Some(id) => match fs.runtime(&id).await {
						Some(rt) => rt.turn.lock().await.cancel.is_some(),
						None => false,
					},
					None => false,
				}
			}
			None => false,
		};
		// `bash_target` mirrors what `tools::bash` would pick if it
		// ran right now for the active folder's *visible* session —
		// so the header indicator reflects that session's per-session
		// force-host override, not just the raw container state.
		// `None` when no folder is active — chat still works, only
		// tool calls would fail. `force_host_override` is surfaced
		// separately so the panel can render the "off-default" badge
		// without re-deriving the auto target.
		let (bash_target, force_host_override) = match self.state.workspaces.active_folder().await {
			Some(folder) => {
				let path = Utf8PathBuf::from(folder.folder.path.clone());
				let fs = self.state.folder_session_for(&path).await;
				let force_host = self.visible_session_force_host(&fs).await;
				let target = crate::tools::resolve_bash_target(&self.state.workspaces, &self.state.workspaces_dir, force_host)
					.await
					.to_string();
				(Some(target), force_host)
			}
			None => (None, false),
		};
		Ok(CoderStatus {
			signed_in,
			identity,
			busy,
			bash_target,
			force_host_override,
		})
	}

	/// Returns the cached "Bound folders" description for `folder`
	/// when one exists and is still in sync with the on-disk
	/// manifests. `None` when the cache is cold or stale —
	/// callers (the project bar tooltip, sub-agent target picker
	/// preview) should treat that as "summary still generating"
	/// and let the next turn refresh it.
	pub async fn folder_summary(&self, folder: &str) -> Option<String> {
		let path = camino::Utf8Path::new(folder);
		self
			.state
			.folder_summaries
			.cached(path)
			.await
			.map(|summary| summary.description)
	}

	/// Ask the fast model to propose a kebab-cased branch name from
	/// `commit_message` and `diff_summary`. Either may be empty
	/// (the caller is free to send only one); we just nudge the
	/// model harder when both are blank by saying "no diff
	/// available" so it doesn't hallucinate a plausible-but-wrong
	/// name. Output is post-processed through
	/// [`sanitise_branch_name`] so the model can't slip a slash,
	/// space, or stray quote past us.
	///
	/// Errors when the model call fails or the response sanitises
	/// down to the empty string. `NoActiveFolder` is returned by
	/// the caller if there's no folder bound; this method itself
	/// doesn't touch the workspace.
	pub async fn suggest_branch_name(&self, commit_message: &str, diff_summary: &str) -> Result<String, CoderError> {
		let prompt = build_branch_name_prompt(commit_message, diff_summary);
		let messages = vec![
			ChatMessage::System {
				content: BRANCH_NAME_SYSTEM_PROMPT.to_string(),
			},
			ChatMessage::user(prompt),
		];
		let cheap_model = self.state.models.read().await.cheap().to_owned();
		let cancel = CancellationToken::new();
		let response = self
			.state
			.inference
			.chat_completion(&cheap_model, &messages, &[], &cancel)
			.await?;
		let raw = response.content.unwrap_or_default();
		let cleaned = sanitise_branch_name(&raw);
		if cleaned.is_empty() {
			return Err(CoderError::Internal("branch name suggestion was empty".into()));
		}
		Ok(cleaned)
	}

	/// Suggest a commit message from the working-tree diff. Same
	/// shape as [`Self::suggest_branch_name`] — fast model,
	/// tightly-scoped system prompt, output run through
	/// [`sanitise_commit_message`] so we strip stray markdown / code
	/// fences / quote wrappers the model occasionally tacks on.
	///
	/// `diff_patch` is the actual `git diff HEAD` output (capped
	/// upstream at ~64 KB by [`crate::host::run_git_diff_patch`]) —
	/// the model needs the patch content, not just the stat, to
	/// write a subject line that's specific rather than generic.
	/// `existing_message` is whatever the user has already typed in
	/// the composer, included as soft context: "if the user already
	/// has a direction, refine it; otherwise infer freshly".
	///
	/// Errors when the model call fails or the response sanitises
	/// down to the empty string.
	pub async fn suggest_commit_message(&self, existing_message: &str, diff_patch: &str) -> Result<String, CoderError> {
		let prompt = build_commit_message_prompt(existing_message, diff_patch);
		let messages = vec![
			ChatMessage::System {
				content: COMMIT_MESSAGE_SYSTEM_PROMPT.to_string(),
			},
			ChatMessage::user(prompt),
		];
		let cheap_model = self.state.models.read().await.cheap().to_owned();
		let cancel = CancellationToken::new();
		let response = self
			.state
			.inference
			.chat_completion(&cheap_model, &messages, &[], &cancel)
			.await?;
		let raw = response.content.unwrap_or_default();
		let cleaned = sanitise_commit_message(&raw);
		if cleaned.is_empty() {
			return Err(CoderError::Internal("commit message suggestion was empty".into()));
		}
		Ok(cleaned)
	}

	/// Translate a natural-language request into a single shell
	/// command for a terminal's `Ctrl+K` prompt. The result is
	/// prefilled into the PTY input line (not executed) so the
	/// user reviews it and presses Enter themselves.
	///
	/// `request` is the user's free text ("cherry pick last commit
	/// from feat-x"). `ctx` carries the terminal's situation —
	/// host vs container shell, cwd, and the active folder's git
	/// branch — so the model can disambiguate (e.g. which branch
	/// "the other one" means) without a tool round-trip. Uses the
	/// standard model rather than the cheap one: command synthesis
	/// needs real reasoning about git / shell semantics, and it's
	/// a one-shot call the user explicitly triggered.
	///
	/// Output is run through [`sanitise_terminal_command`] to keep
	/// it to a single line and strip markdown fences the model
	/// occasionally wraps a command in. Errors when the model call
	/// fails or the response sanitises to empty.
	pub async fn suggest_terminal_command(
		&self,
		request: &str,
		ctx: &TerminalCommandContext,
	) -> Result<String, CoderError> {
		let prompt = build_terminal_command_prompt(request, ctx);
		let messages = vec![
			ChatMessage::System {
				content: TERMINAL_COMMAND_SYSTEM_PROMPT.to_string(),
			},
			ChatMessage::user(prompt),
		];
		let model = self.state.models.read().await.standard().to_owned();
		let cancel = CancellationToken::new();
		let response = self
			.state
			.inference
			.chat_completion(&model, &messages, &[], &cancel)
			.await?;
		let raw = response.content.unwrap_or_default();
		let cleaned = sanitise_terminal_command(&raw);
		if cleaned.is_empty() {
			return Err(CoderError::Internal("terminal command suggestion was empty".into()));
		}
		Ok(cleaned)
	}

	pub async fn start_device_flow(&self) -> Result<DeviceCode, CoderError> {
		self.state.auth.start_device_flow().await
	}

	pub async fn poll_device_code(&self, code: DeviceCode) -> Result<HfIdentity, CoderError> {
		self.state.auth.poll_device_code(&code).await
	}

	pub async fn sign_out(&self) -> Result<(), CoderError> {
		// Sign-out aborts every in-flight turn across every
		// folder, since the user is repudiating the auth identity
		// the inference client is using. Then drop every cached
		// per-folder session — a re-sign-in is conceptually a
		// fresh conversation. On-disk sessions are untouched
		// (they belong to the workspace, not the user identity).
		self.abort_all().await;
		self.state.auth.sign_out().await?;
		self.state.sessions_by_folder.write().await.clear();
		Ok(())
	}

	/// Cancel every running turn across every folder + every
	/// concurrently-running session inside each folder. Used by
	/// sign-out (semantic "this auth identity is no longer
	/// driving the agent") and by tests that need a clean slate.
	async fn abort_all(&self) {
		let folders: Vec<Arc<FolderSession>> = self.state.sessions_by_folder.read().await.values().cloned().collect();
		for fs in folders {
			fs.cancel_all().await;
		}
	}

	/// Snapshot of the **active folder's visible session**. `None`
	/// when the session is blank (no user message yet) or no
	/// folder is active — the panel uses this to render the empty
	/// / "send your first message" state. Two-step look-up rather
	/// than `visible_runtime()` so we don't lazy-create a blank
	/// runtime entry on every status poll from the empty state.
	pub async fn active_session(&self) -> Option<SessionSummary> {
		let (fs, _) = self.state.active_folder_session().await.ok()?;
		let id = fs.visible_session_id().await?;
		let rt = fs.runtime(&id).await?;
		let session = rt.session.lock().await;
		if session.header.title.is_empty() && session.persisted_records == 0 {
			return None;
		}
		Some(session.summary())
	}

	/// List sessions on disk for the active workspace folder.
	/// Empty when the folder has none — including when no folder
	/// is active at all (chat-only sessions aren't supported).
	pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CoderError> {
		// Per-project scoping (ADR 0028): a worktree folder's sessions
		// live under its parent project root, so list against that.
		let Some(folder) = self.state.coder_root_folder().await else {
			return Ok(Vec::new());
		};
		let folder_root = Utf8PathBuf::from(folder.folder.path.clone());
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		sessions::list_sessions(&dir).await
	}

	/// Resolve the on-disk JSONL path for a session id under the
	/// active workspace folder. Used by the panel's "open trace"
	/// affordance: the frontend takes the returned path, hands it
	/// to `fs_read_file_host`, and the editor opens the trace as
	/// a host-direct file (so it works the same whether the
	/// project is local or running in a container — the JSONL
	/// always lives on the host's `XDG_DATA_HOME`, never inside
	/// the container).
	///
	/// `id` can be either a top-level session id or a sub-agent
	/// id; both live under the parent folder's slug, so the
	/// active folder is enough to resolve them. Errors with
	/// `NotFound` if the file isn't on disk yet (empty sessions
	/// aren't persisted until the first `send`); the panel
	/// surfaces that as a flash so the user knows there's nothing
	/// to open.
	pub async fn session_jsonl_path(&self, id: String) -> Result<Utf8PathBuf, CoderError> {
		sessions::validate_session_id(&id)?;
		// Per-project scoping (ADR 0028): worktree sessions are filed
		// under the parent project root.
		let Some(folder) = self.state.coder_root_folder().await else {
			return Err(CoderError::NoActiveFolder);
		};
		let folder_root = Utf8PathBuf::from(folder.folder.path.clone());
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		let direct = sessions::session_path(&dir, &id);
		if tokio::fs::try_exists(direct.as_std_path())
			.await
			.map_err(|err| CoderError::Internal(format!("could not stat session jsonl: {err}")))?
		{
			return Ok(direct);
		}
		// Fallback for sub-agent ids: scan per-parent subdirectories
		// (`<dir>/<parent-id>/<sub-id>.jsonl`). The IPC takes a
		// single id and doesn't carry the parent, so we do the
		// lookup here. No-op for top-level ids.
		if let Some(found) = sessions::find_subagent_session(&dir, &id).await {
			return Ok(found);
		}
		Err(CoderError::Internal(format!("session jsonl not found: {direct}")))
	}

	/// Allocate a fresh blank session under the active folder and
	/// make it the visible one. Does **not** touch any other
	/// session's running turn — previously-visible sessions whose
	/// agent is still mid-turn keep streaming into their own UI
	/// bucket on the frontend (see [ADR 0016]). Returns the new
	/// session's metadata so the panel can reference it before
	/// the first send.
	pub async fn new_session(&self) -> Result<SessionSummary, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let blank = Session::new_blank();
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id.clone(), rt).await;
		fs.set_visible(id).await;
		Ok(summary)
	}

	/// Allocate a fresh **coordinator** session under the active
	/// folder and make it the visible one (ADR 0030). Same shape as
	/// `new_session` but builds from `new_blank_with_mode(Coordinator)`
	/// so the header carries `mode: "coordinator"`, the system prompt
	/// seed is the coordinator prompt, and `run_turn` advertises the
	/// worker-management tools instead of `task` / `ask_user`.
	pub async fn new_coordinator_session(&self) -> Result<SessionSummary, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let blank = Session::new_blank_with_mode(CoderMode::Coordinator);
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id.clone(), rt).await;
		fs.set_visible(id).await;
		Ok(summary)
	}

	/// Create a fresh session under the **active (parent) folder**
	/// that routes its tools to an already-created git worktree at
	/// `worktree_root` on `branch` (ADR 0028). The session stays
	/// filed under the parent — same sessions list, same JSONL slug
	/// — but every turn's `cx.folder` resolves to the worktree, so
	/// the agent's edits / builds land in the isolated checkout.
	///
	/// The caller is responsible for having created the worktree and
	/// bound it as a folder first (so `folder_for_path(worktree_root)`
	/// resolves at turn time); this only mints the session and stamps
	/// the binding onto its header.
	pub async fn new_worktree_session(
		&self,
		worktree_root: String,
		branch: String,
	) -> Result<SessionSummary, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let mut blank = Session::new_blank();
		blank.header.worktree_root = Some(worktree_root);
		blank.header.worktree_branch = Some(branch);
		// No bash-target override: a worktree folder routes to the
		// container like any other folder (ADR 0028 W.4.1 — it sits
		// under the shared `/workspace/.worktrees` mount), so the
		// agent's builds get the container toolchain. The user can
		// still force host via the per-session toggle.
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id.clone(), rt).await;
		fs.set_visible(id).await;
		Ok(summary)
	}

	/// Create an isolated coder session in its own git worktree —
	/// the full orchestration the `coder_new_worktree_session`
	/// Tauri command used to own, now promoted to a client-callable
	/// method (ADR 0030 Prerequisite #2) so an in-process orchestrator
	/// agent can mint workers via `spawn_worker` without going through
	/// the Tauri command layer.
	///
	/// Steps: resolve the active (parent) folder, compute the branch
	/// spec (fresh `moon/agent-<id>` off HEAD, or check out an existing
	/// `base_branch`), derive the worktree path under
	/// `<parent>/.worktrees/<branch-slug>`, `git worktree add`, bind
	/// it as a nested folder, and mint a session (filed under the
	/// parent) whose tools route to the worktree.
	///
	/// `mode` selects the new session's top-level mode — `Agent` for
	/// an ordinary worker (the common case), `Coordinator` for a
	/// sub-orchestrator. The active folder is unchanged; the new
	/// session is set visible so the panel opens it, matching the
	/// historical Tauri-command behaviour.
	pub async fn create_worktree_session(
		&self,
		base_branch: Option<String>,
		mode: CoderMode,
	) -> Result<(SessionSummary, moon_protocol::workspace::Workspace), CoderError> {
		use moon_core::host::WorktreeBranch;
		let parent = self.state.workspaces.require_active_folder().await?;
		let parent_path = parent.folder.path.clone();
		let spec = match base_branch.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
			Some(existing) => WorktreeBranch::Existing(existing.to_string()),
			None => {
				let now_ms = std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_millis())
					.unwrap_or(0) as u64;
				WorktreeBranch::New(format!("moon/agent-{:08x}", now_ms & 0xffff_ffff))
			}
		};
		let branch = spec.name().to_string();
		let branch_slug = branch.replace('/', "-");
		let worktree_path = camino::Utf8Path::new(&parent_path)
			.join(moon_core::WORKTREES_DIR_NAME)
			.join(&branch_slug);
		if let Some(parent_dir) = worktree_path.parent() {
			std::fs::create_dir_all(parent_dir.as_std_path())?;
		}
		parent.host.git_worktree_add(&worktree_path, spec).await?;
		let wt_entry = self
			.state
			.workspaces
			.add_worktree_folder(worktree_path, parent_path, branch.clone())
			.await?;
		// Mint the session in the chosen mode and stamp the worktree
		// routing. Builds from `new_blank_with_mode` so a coordinator
		// worker gets its system-prompt seed; an ordinary worker is
		// `Agent`.
		let (fs, _) = self.state.active_folder_session().await?;
		let mut blank = Session::new_blank_with_mode(mode);
		blank.header.worktree_root = Some(wt_entry.folder.path.clone());
		blank.header.worktree_branch = Some(branch);
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id.clone(), rt).await;
		fs.set_visible(id).await;
		let workspace = self.state.workspaces.snapshot().await;
		Ok((summary, workspace))
	}

	/// Set the per-session bash-target override for the **visible
	/// session** under the active folder. `force_host = true` pins
	/// this session's `bash` + format-on-save subprocesses to the
	/// host even while the workspace runs in a container;
	/// `force_host = false` restores the auto default. Mutates the
	/// in-memory header and rewrites the on-disk header (best
	/// effort — a not-yet-persisted session just carries it in
	/// memory until first persist). No-op when no folder is active
	/// or the visible session can't be resolved.
	///
	/// Per-session, not per-workspace: each concurrent session in a
	/// folder keeps its own choice, and a fresh session always
	/// starts auto. Returns the resolved state so the caller can
	/// emit a fresh status without a round-trip.
	pub async fn set_bash_target_override(&self, force_host: bool) -> Result<bool, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let Some(id) = fs.visible_session_id().await else {
			return Err(CoderError::NoActiveFolder);
		};
		let Some(rt) = fs.runtime(&id).await else {
			return Err(CoderError::NoActiveFolder);
		};
		let (session_dir, header) = {
			let mut session = rt.session.lock().await;
			session.header.bash_target_override = force_host.then_some(BashTargetOverride::ForceHost);
			(session.session_dir.clone(), session.header.clone())
		};
		if let Some(dir) = session_dir {
			if let Err(err) = sessions::rewrite_header(&dir, &header).await {
				tracing::warn!(?err, "failed to persist bash_target_override header rewrite");
			}
		}
		Ok(force_host)
	}

	/// Tag the active folder's **visible** session with the branch its
	/// work was committed onto (ADR 0028), rewriting the on-disk
	/// header so a re-open remembers it. Called whenever the user
	/// commits with a session open — to whatever branch `HEAD` landed
	/// on (a fresh "commit on new branch", or a plain commit on the
	/// current one). Most-recent-commit wins.
	///
	/// Skips a not-yet-persisted (blank) session: a branch only means
	/// something once the session has produced work the user
	/// committed. Returns the updated summary so the panel can refresh
	/// the row's chip without a reload; `None` when there's no
	/// persisted visible session to tag (so a manual commit with no
	/// real session open quietly ties nothing).
	pub async fn set_visible_session_branch(&self, branch: String) -> Result<Option<SessionSummary>, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let Some(id) = fs.visible_session_id().await else {
			return Ok(None);
		};
		let Some(rt) = fs.runtime(&id).await else {
			return Ok(None);
		};
		let (session_dir, summary, header) = {
			let mut session = rt.session.lock().await;
			if session.session_dir.is_none() {
				return Ok(None);
			}
			session.header.committed_branch = Some(branch);
			(session.session_dir.clone(), session.summary(), session.header.clone())
		};
		if let Some(dir) = session_dir {
			if let Err(err) = sessions::rewrite_header(&dir, &header).await {
				tracing::warn!(?err, "failed to persist committed_branch header rewrite");
			}
		}
		Ok(Some(summary))
	}

	/// Move the active folder's **visible** session into a git
	/// worktree (ADR 0028): stamp `worktree_root` + `worktree_branch`
	/// on its header so the next turn's tools route to the worktree
	/// checkout, and rewrite the on-disk header. The conversation is
	/// untouched — it keeps its full history and stays in the same
	/// (per-project) session list, it just starts operating on the
	/// isolated branch. Works on a blank, not-yet-persisted session
	/// too (the "+ then worktree" flow): the header rides in memory
	/// until first persist. Returns the updated summary, or `None`
	/// when there's no visible session. Errors if the session is
	/// already in a worktree (the caller should branch on that).
	pub async fn move_visible_session_to_worktree(
		&self,
		worktree_root: String,
		branch: String,
	) -> Result<Option<SessionSummary>, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let Some(id) = fs.visible_session_id().await else {
			return Ok(None);
		};
		let Some(rt) = fs.runtime(&id).await else {
			return Ok(None);
		};
		let (session_dir, summary, header) = {
			let mut session = rt.session.lock().await;
			if session.header.worktree_root.is_some() {
				return Err(CoderError::Internal(
					"this session already runs in a worktree".to_string(),
				));
			}
			session.header.worktree_root = Some(worktree_root);
			session.header.worktree_branch = Some(branch);
			(session.session_dir.clone(), session.summary(), session.header.clone())
		};
		if let Some(dir) = session_dir {
			if let Err(err) = sessions::rewrite_header(&dir, &header).await {
				tracing::warn!(?err, "failed to persist worktree-move header rewrite");
			}
		}
		Ok(Some(summary))
	}

	/// Whether the active folder's visible session can be moved into a
	/// worktree: there is one, and it isn't already in a worktree. The
	/// move command checks this *before* creating any git worktree, so
	/// the no-op cases don't strand an orphaned worktree.
	pub async fn can_move_visible_session(&self) -> Result<bool, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let Some(id) = fs.visible_session_id().await else {
			return Ok(false);
		};
		let Some(rt) = fs.runtime(&id).await else {
			return Ok(false);
		};
		let already = rt.session.lock().await.header.worktree_root.is_some();
		Ok(!already)
	}

	/// The visible session's associated branch — `committed_branch`
	/// (set by the last commit made with this session open), or
	/// `worktree_branch` if the session already runs in a worktree.
	/// `None` when no session is visible or the session has no
	/// associated branch yet. Used by the worktree button to check
	/// out the session's own branch instead of whatever the main tree
	/// happens to be on.
	pub async fn visible_session_branch(&self) -> Result<Option<String>, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		let Some(id) = fs.visible_session_id().await else {
			return Ok(None);
		};
		let Some(rt) = fs.runtime(&id).await else {
			return Ok(None);
		};
		let session = rt.session.lock().await;
		Ok(
			session
				.header
				.worktree_branch
				.clone()
				.or(session.header.committed_branch.clone()),
		)
	}

	/// Read the force-host override of `fs`'s visible session
	/// without lazy-creating a runtime (mirrors the two-step
	/// look-up `status` uses for `busy`). `false` when no session
	/// is visible yet.
	async fn visible_session_force_host(&self, fs: &Arc<FolderSession>) -> bool {
		let Some(id) = fs.visible_session_id().await else {
			return false;
		};
		let Some(rt) = fs.runtime(&id).await else {
			return false;
		};
		let forced = rt.session.lock().await.header.bash_target_override == Some(BashTargetOverride::ForceHost);
		forced
	}

	/// Revert the active folder's visible session back to just
	/// before its `user_ordinal`-th user message, dropping that
	/// message and everything that followed from both the on-disk
	/// JSONL and the in-memory chat history.
	///
	/// `user_ordinal` is 0-based over the session's user messages
	/// in transcript order — the same order the panel renders its
	/// `user` rows. Steers count, matching how they render. The
	/// runner mints fresh per-message ids on every replay, so the
	/// ordinal (not a row id) is the reload-stable anchor.
	///
	/// Powers two panel affordances: "revert to here" (the user
	/// discards the dropped text) and "edit & resend" (the panel
	/// drops the returned text into the composer for the user to
	/// tweak and re-send). The returned [`RevertedMessage`] carries
	/// the dropped prompt for the latter; the former ignores it.
	///
	/// Refuses while the visible session's turn is in flight — the
	/// transcript is being actively appended to and rewriting it
	/// underneath the running loop would corrupt the next
	/// iteration. The panel disables the affordance during a turn;
	/// this is the backend belt-and-braces.
	///
	/// Implementation reuses [`Coder::open_session`] for the
	/// reload: after the disk truncation we drop the mounted
	/// runtime so `open_session` rebuilds the in-memory `Session`
	/// from the now-shorter JSONL and replays the trimmed
	/// transcript to the panel through the existing event path.
	pub async fn revert_to_message(&self, user_ordinal: usize) -> Result<RevertedMessage, CoderError> {
		let (rt, session_id, folder_path) = self.state.active_visible_runtime().await?;
		// Refuse mid-turn — see doc comment. Checked under the
		// turn lock so a concurrent `send` can't slip a turn in
		// between the check and the truncation.
		{
			let turn = rt.turn.lock().await;
			if turn.cancel.is_some() {
				return Err(CoderError::Internal(
					"cannot revert while a turn is running; stop it first".into(),
				));
			}
		}
		let header = rt.session.lock().await.header.clone();
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let reverted = sessions::truncate_before_user_record(&dir, &header, user_ordinal).await?;

		// Drop the mounted runtime so `open_session` takes its
		// rebuild-from-disk path (the fast-path otherwise leaves
		// the stale in-memory `messages` untouched). Any turn is
		// already known-absent from the check above.
		{
			let (fs, _) = self.state.active_folder_session().await?;
			fs.runtimes.write().await.remove(&session_id);
		}
		self.open_session(session_id).await?;

		Ok(RevertedMessage {
			text: reverted.dropped_text,
			images: reverted.dropped_images,
		})
	}

	/// Replay from a specific user message: truncate the
	/// session to just before that message (exactly
	/// [`revert_to_message`]) and immediately re-send the dropped
	/// prompt verbatim. The user-visible gesture is "re-run this
	/// turn" — the same truncation as edit-and-resend, but the
	/// prompt fires again without round-tripping through the
	/// composer. Useful when the previous answer went sideways and
	/// the user wants the same prompt retried against the current
	/// model / workspace state.
	///
	/// Auth-gates **before** the destructive truncation: a
	/// signed-out replay fails clean without rewriting the JSONL,
	/// so the session isn't left truncated and unable to send.
	/// Reuses [`revert_to_message`] for the truncate + remount +
	/// replay, then [`send`] for the new turn — both refuse
	/// mid-turn, so the composition inherits the same guard.
	pub async fn replay_from_message(&self, user_ordinal: usize) -> Result<(), CoderError> {
		self.ensure_can_send().await?;
		let dropped = self.revert_to_message(user_ordinal).await?;
		self.send(dropped.text, dropped.images).await
	}

	/// Resume the turn from a mid-turn agent response: truncate the
	/// session to keep everything up to **and including** the
	/// `assistant_ordinal`-th `Assistant` record (with tool calls),
	/// drop its `Tool` records and everything after, then re-dispatch
	/// the kept `Assistant`'s `tool_calls` against the current
	/// workspace and continue the turn loop. The user-visible gesture
	/// is "re-run the tool calls from this checkpoint" — the model
	/// isn't re-prompted for that round-trip; its existing tool calls
	/// execute fresh against current workspace state and the loop
	/// continues with the new results in context.
	///
	/// Auth-gates **before** the destructive truncation (same posture
	/// as [`replay_from_message`]). Refuses mid-turn. Unlike
	/// [`revert_to_message`], this does **not** drop the mounted
	/// runtime and reopen — it mutates the existing runtime's
	/// `messages` in place and fires a `SessionLoaded` + `Replay` so
	/// the frontend clears and rebuilds to the checkpoint state. The
	/// turn loop then runs on the same `rt`, so `abort` / `send`
	/// target the right runtime.
	pub async fn resume_from_assistant(&self, assistant_ordinal: usize) -> Result<(), CoderError> {
		self.ensure_can_send().await?;
		let (rt, session_id, folder_path) = self.state.active_visible_runtime().await?;
		{
			// Refuse mid-turn — same guard as `revert_to_message`.
			let turn = rt.turn.lock().await;
			if turn.cancel.is_some() {
				return Err(CoderError::Internal(
					"cannot resume while a turn is running; stop it first".into(),
				));
			}
		}
		let header = rt.session.lock().await.header.clone();
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let resumed = sessions::truncate_before_assistant_record(&dir, &header, assistant_ordinal).await?;

		// Load the surviving records from disk (the JSONL was just
		// rewritten by the truncation) and rebuild `messages` from
		// them. We skip orphan recovery — the kept Assistant's
		// tool_calls are intentionally unpaired because we're about
		// to re-dispatch them. Injecting "Interrupted" sentinels
		// would feed the model stale error results alongside the
		// fresh ones.
		let LoadedSession {
			records,
			record_timestamps,
			..
		} = sessions::load(&dir, &header.id).await?;
		let RebuiltMessages {
			messages,
			last_usage,
			last_todos,
		} = Self::rebuild_messages_from_records(&records);

		// Mutate the existing runtime in place. We keep the same
		// `Arc<SessionRuntime>` so `abort` / `send` / the turn loop
		// all target the same `rt` — no stale-runtime split.
		{
			let mut session = rt.session.lock().await;
			session.messages = messages;
			session.last_usage = last_usage;
			session.todos = last_todos;
			session.persisted_records = records.len() as u32;
			session.header.updated_at_ms = current_time_ms();
		}

		// Fire `SessionLoaded` + `Replay` so the frontend clears its
		// bucket and rebuilds from the surviving records — the
		// checkpoint state, ending at the kept Assistant row with no
		// tool rows after it. `in_flight: true` on the replay keeps
		// `busy` asserted so the panel shows the running state.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id.clone());
		sink.send(CoderEvent::SessionLoaded {
			id: header.id.clone(),
			title: header.title.clone(),
			created_at_ms: header.created_at_ms,
			updated_at_ms: header.updated_at_ms,
		});
		// The kept Assistant's tool_calls will be re-dispatched by
		// the turn loop — don't emit `ToolCall` events for them in
		// the replay, or the frontend would create tool rows that
		// the re-dispatch's `ToolCall` events would duplicate (same
		// IDs, new rows pushed).
		let resume_call_ids: std::collections::HashSet<String> =
			resumed.resume_tool_calls.iter().map(|c| c.id.clone()).collect();
		let mut replay_events: Vec<CoderEvent> = Vec::with_capacity(records.len() + 2);
		for (record, record_ts) in records.into_iter().zip(record_timestamps) {
			match record {
				SessionRecord::SubagentSpawned {
					ref tool_call_id,
					ref subagent_id,
					ref target_folder,
					ref mode,
				} => {
					replay_subagent_spawned(
						&mut replay_events,
						&dir,
						&header.id,
						tool_call_id.clone(),
						subagent_id.clone(),
						target_folder.clone(),
						mode.clone(),
					)
					.await;
				}
				SessionRecord::SubagentFinished {
					subagent_id,
					tokens_used_estimate,
					was_error,
					result_preview: _,
				} => {
					replay_events.push(CoderEvent::SubagentFinished {
						subagent_id,
						tokens_used_estimate,
						was_error,
					});
				}
				other => emit_replay_events(&mut replay_events, other, record_ts),
			}
		}
		// Filter out `ToolCall` events for the resume tool calls —
		// the kept Assistant record's `emit_replay_events` emitted
		// them, but the re-dispatch will emit fresh ones. Without
		// this filter the frontend would have duplicate tool rows
		// (replay creates one, re-dispatch creates another with the
		// same id — `tool_call` always pushes a new row, it doesn't
		// update by id the way `tool_result` does).
		replay_events.retain(|event| {
			if let CoderEvent::ToolCall { id, .. } = event {
				!resume_call_ids.contains(id)
			} else {
				true
			}
		});
		// No orphan tool results — the kept Assistant's tool calls
		// are about to be re-dispatched, not marked as interrupted.
		// The trailing `TurnComplete` closes the replay window and
		// sets `busy = false` on the frontend; `in_flight: true`
		// re-asserts it immediately after — the turn is genuinely
		// about to start (the resume dispatch fires within
		// milliseconds).
		replay_events.push(CoderEvent::TurnComplete);
		sink.send(CoderEvent::Replay {
			events: replay_events,
			in_flight: true,
		});

		// Spawn the turn loop with the resume tool calls. The first
		// iteration re-dispatches them; subsequent iterations make
		// normal LLM calls with the fresh results in context.
		let cancel = CancellationToken::new();
		{
			let mut turn = rt.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}
		let state = self.state.clone();
		spawn_turn_loop(
			state,
			rt,
			sink,
			folder_path.to_path_buf(),
			cancel,
			false,
			Some(resumed.resume_tool_calls),
		);
		Ok(())
	}

	/// Pre-flight auth gate shared by every send-shaped entry
	/// point ([`send`], [`replay_from_message`]). HF needs OAuth;
	/// user providers need a configured key (or a localhost
	/// `base_url`, where keyless is conventional for Ollama /
	/// llama.cpp). Surfacing this cleanly up front avoids letting
	/// the inference layer fail on the first request, and for
	/// `replay_from_message` keeps the destructive JSONL
	/// truncation from running before a send that would never
	/// fire. `drain_steer_now` skips this gate — the steer it
	/// drains was already accepted by an authenticated `send`.
	async fn ensure_can_send(&self) -> Result<(), CoderError> {
		let route = self.state.models.read().await.resolve_route();
		match &route {
			ResolvedProvider::HuggingFace => {
				if !self.state.auth.has_valid_session().await {
					return Err(CoderError::NotSignedIn);
				}
			}
			ResolvedProvider::Custom { id, base_url }
			| ResolvedProvider::OpenRouter { id, base_url }
			| ResolvedProvider::Anthropic { id, base_url } => {
				if !self.state.provider_keys.has_key(id) && !is_local_base_url(base_url) {
					return Err(CoderError::NotSignedIn);
				}
			}
		}
		Ok(())
	}

	/// Manually re-dispatch a previously-recorded `write_file` /
	/// `edit_file` tool call from the active folder's visible
	/// session against the current workspace. The recovery
	/// affordance for "I reset / clobbered a file and want the
	/// agent's edit back" without re-running the whole turn.
	///
	/// Scoped to the two file-writing tools: re-running `bash`,
	/// network, or read tools out of band has no recovery value
	/// and could be destructive. An unsupported or unknown
	/// `tool_call_id` errors so the panel can flash a reason.
	///
	/// Pure side-effect — nothing is appended to the transcript or
	/// the JSONL; the row's recorded result stays the historical
	/// record. The reapply runs the same turn-end format-on-save
	/// pass a normal turn would, so the bytes match what the
	/// original turn left on disk. A dispatch failure (e.g. an
	/// `edit_file` whose `find` no longer matches the current file)
	/// propagates as `Err` for the panel to surface.
	pub async fn rerun_tool_call(&self, tool_call_id: String) -> Result<RerunToolOutcome, CoderError> {
		let (rt, _, _) = self.state.active_visible_runtime().await?;
		let (tool_name, args) = {
			let session = rt.session.lock().await;
			find_recorded_tool_call(&session.messages, &tool_call_id)
				.ok_or_else(|| CoderError::Internal(format!("no tool call `{tool_call_id}` in the visible session")))?
		};
		if tool_name != "write_file" && tool_name != "edit_file" {
			return Err(CoderError::Internal(format!(
				"only write_file / edit_file can be reapplied, not `{tool_name}`"
			)));
		}
		let cx = self.state.tools.context_for_active(CoderMode::Agent).await?;
		let cancel = CancellationToken::new();
		let result = self.state.tools.dispatch(&tool_name, &args, &cx, &cancel).await?;
		flush_format_queue(&self.state, &cx.format_queue).await;
		Ok(RerunToolOutcome { tool_name, result })
	}

	/// Make the persisted session identified by `id` the visible
	/// one under the active workspace folder. Replays the JSONL
	/// records as live events so the panel's existing event
	/// handlers populate the transcript without a special "loaded"
	/// code path beyond the initial reset.
	///
	/// Does **not** cancel any other running turn — previously-
	/// visible sessions whose agent is mid-turn keep streaming
	/// into their own UI bucket on the frontend (see [ADR 0016]).
	/// If `id` is already mounted as a runtime (the user clicked
	/// a session that's been running in the background), we reuse
	/// the existing runtime — its in-memory `messages` is the
	/// source of truth, not the on-disk JSONL which may be lagging
	/// the running turn by an iteration.
	/// Rebuild `Vec<ChatMessage>` from persisted records — the
	/// message-history reconstruction `open_session` and
	/// `resume_from_assistant` both need. Walks records linearly,
	/// folding compaction summaries at the same cutoff the live pass
	/// used, and tracking `last_usage` / `last_todos` (last-wins).
	/// **Does not** inject orphan-recovery `Tool` messages — the
	/// caller decides whether that's appropriate (open_session does
	/// it separately; resume skips it because the orphans are about
	/// to be re-dispatched).
	fn rebuild_messages_from_records(records: &[SessionRecord]) -> RebuiltMessages {
		let mut messages: Vec<ChatMessage> = vec![ChatMessage::System {
			content: PHASE_6_0_SYSTEM_PROMPT.to_string(),
		}];
		let mut last_usage: Option<TokenUsage> = None;
		let mut last_todos: Vec<crate::TodoItem> = Vec::new();
		for record in records {
			match record {
				SessionRecord::User { text, images } => {
					messages.push(ChatMessage::User {
						content: text.clone(),
						images: images.clone(),
					});
				}
				SessionRecord::Assistant {
					content,
					tool_calls,
					thinking_blocks,
					thinking: _,
					model: _,
					stop_reason: _,
				} => {
					messages.push(ChatMessage::Assistant {
						content: content.clone(),
						thinking_blocks: thinking_blocks.clone(),
						tool_calls: tool_calls.clone(),
					});
				}
				SessionRecord::Tool {
					tool_call_id,
					tool_name: _,
					content,
				} => {
					messages.push(ChatMessage::Tool {
						tool_call_id: tool_call_id.clone(),
						content: content.clone(),
					});
				}
				SessionRecord::TitleUpdate { .. } => {}
				SessionRecord::Usage {
					prompt_tokens,
					completion_tokens,
					total_tokens,
					cache_read_input_tokens,
					cache_creation_input_tokens,
				} => {
					last_usage = Some(TokenUsage {
						prompt_tokens: *prompt_tokens,
						completion_tokens: *completion_tokens,
						total_tokens: *total_tokens,
						cache_read_input_tokens: *cache_read_input_tokens,
						cache_creation_input_tokens: *cache_creation_input_tokens,
					});
				}
				SessionRecord::TodosUpdate { todos } => {
					last_todos = todos.clone();
				}
				SessionRecord::Compaction {
					summary, messages_kept, ..
				} => {
					let cutoff = messages.len().saturating_sub(*messages_kept as usize).max(1);
					crate::compaction::apply_summary_to_messages(&mut messages, cutoff, summary);
				}
				SessionRecord::Error { .. } => {}
				SessionRecord::SubagentSpawned { .. } | SessionRecord::SubagentFinished { .. } => {}
			}
		}
		RebuiltMessages {
			messages,
			last_usage,
			last_todos,
		}
	}

	pub async fn open_session(&self, id: String) -> Result<SessionSummary, CoderError> {
		sessions::validate_session_id(&id)?;
		let (fs, folder_path) = self.state.active_folder_session().await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);

		// Fast path: this session id is already mounted as a
		// runtime under the folder (most likely a background turn
		// the user is clicking back to). Make it visible, fire a
		// `SessionLoaded` so the panel re-hydrates from disk via
		// its replay loop, and return.
		//
		// We *do* re-load the JSONL from disk for the panel's
		// benefit (the frontend bucket may have been pruned across
		// a webview reload), but the in-memory `Session` stays
		// untouched — the running turn's `messages` is authoritative
		// and clobbering it would corrupt the next iteration.
		let already_mounted = fs.runtime(&id).await.is_some();
		let LoadedSession {
			header,
			records,
			record_timestamps,
		} = sessions::load(&dir, &id).await?;
		let record_count = records.len();

		let RebuiltMessages {
			mut messages,
			last_usage,
			last_todos,
		} = Self::rebuild_messages_from_records(&records);
		// Orphan tool calls = Assistant tool_calls that never got
		// a matching `Tool` record (user stopped mid-tool, IDE
		// crashed before the dispatcher returned, …). Inject a
		// synthetic `Tool` message for each so the rebuilt
		// `messages` slice satisfies the provider's "every
		// tool_call has a tool result" invariant on the next
		// turn. The panel-side recovery (synthesising
		// `ToolResult` events) lives in the replay loop below.
		let orphan_tool_call_ids = sessions::orphan_tool_call_ids(&records);
		for orphan_id in &orphan_tool_call_ids {
			messages.push(ChatMessage::Tool {
				tool_call_id: orphan_id.clone(),
				content: sessions::INTERRUPTED_TOOL_RESULT_JSON.to_string(),
			});
		}
		// When this session has a **live turn** still parked on an
		// `ask_user` prompt (the user switched away and came back),
		// the parked tool call looks like an orphan on disk — its
		// matching `Tool` record isn't written until the prompt
		// resolves. But it isn't interrupted: it's waiting for the
		// human. Collect those live-parked ids so the replay below
		// re-renders their interactive card (via the Assistant
		// record's `tool_call` event) instead of slapping an
		// "Interrupted before tool completed" result on them. Empty
		// for the common reopen / cold-start case.
		// `in_flight` rides alongside `live_parked_ids` off the same
		// runtime lookup: it's `true` when this session has a turn
		// still streaming in the background. The frontend uses it to
		// keep the sessions-list "running" badge lit after the user
		// clicks into a running session and backs out — the replay's
		// trailing `TurnComplete` terminator would otherwise clear it.
		let mut in_flight = false;
		let live_parked_ids: std::collections::HashSet<String> = if already_mounted {
			if let Some(rt) = fs.runtime(&id).await {
				in_flight = rt.turn.lock().await.cancel.is_some();
				let mut set = std::collections::HashSet::new();
				for orphan_id in &orphan_tool_call_ids {
					if rt.prompts.holds(orphan_id).await {
						set.insert(orphan_id.clone());
					}
				}
				set
			} else {
				std::collections::HashSet::new()
			}
		} else {
			std::collections::HashSet::new()
		};
		let summary = SessionSummary {
			id: header.id.clone(),
			title: header.title.clone(),
			created_at_ms: header.created_at_ms,
			updated_at_ms: header.updated_at_ms,
			worktree_branch: header.worktree_branch.clone(),
			committed_branch: header.committed_branch.clone(),
			mode: header.mode.clone(),
		};
		// Snapshot what the panel needs for the restore-time
		// usage hint *before* the move into `Session`. We prefer
		// the last persisted `Usage` record (provider-exact for
		// the round-trip that wrote it) over a bytes/4 estimate
		// of the rebuilt history; the estimate is the fallback
		// for sessions written before the `Usage` variant shipped
		// or for round-trips where the provider didn't emit a
		// usage chunk. Either way the panel's context-usage ring
		// fills in the moment the transcript appears, instead of
		// staying empty until the user sends their first new
		// prompt. The next live call overwrites whatever we send
		// here.
		let restore_models = self.state.models.read().await.clone();
		let restore_standard = restore_models.standard().to_owned();
		let restore_context_window = restore_models.context_window(&restore_standard);
		let (restore_prompt, restore_completion, restore_total, restore_cache_read, restore_cache_creation, restore_source) =
			match last_usage {
				Some(u) => (
					u.prompt_tokens,
					u.completion_tokens,
					u.total_tokens,
					u.cache_read_input_tokens,
					u.cache_creation_input_tokens,
					TokenUsageSource::Provider,
				),
				None => {
					let estimate = estimate_prompt_tokens(&messages);
					(estimate, 0, estimate, 0, 0, TokenUsageSource::Estimate)
				}
			};
		// If a runtime for this id already exists (background turn
		// the user is clicking back to), skip the
		// session-replacement step so we don't stomp the running
		// turn's in-memory `messages` / `last_usage` / `todos`.
		// The replay loop below still fires SessionLoaded + the
		// historic events so the panel re-hydrates its bucket
		// from disk; the runtime's in-memory state is what the
		// next live event from the running turn will continue
		// writing into, and the frontend reconciles deltas on top
		// of the replayed transcript without conflict (the wire
		// shape is idempotent at the row-id level).
		if !already_mounted {
			let session = Session {
				header,
				session_dir: Some(dir.clone()),
				messages,
				persisted_records: records.len() as u32,
				auto_rename_pending: false,
				// Seed the in-memory `last_usage` with whatever
				// we recovered from disk. Without this the auto-
				// compaction trigger wouldn't have a number to
				// compare against until the first post-restore
				// round-trip lands — and a session that was
				// already near the compaction threshold when it
				// got persisted would silently skip the
				// compaction-before-send guard on the very next
				// prompt.
				last_usage,
				todos: last_todos,
				pending_steers: Vec::new(),
			};
			let rt = Arc::new(SessionRuntime::new(session));
			fs.insert_runtime(id.clone(), rt).await;
		}
		fs.set_visible(id.clone()).await;

		// Tell the panel to clear + reload, then fan out the
		// records as the same events a live turn would emit.
		// `SessionLoaded` carries the metadata so the sticky
		// header doesn't need a follow-up IPC round trip.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), id.clone());
		sink.send(CoderEvent::SessionLoaded {
			id: summary.id.clone(),
			title: summary.title.clone(),
			created_at_ms: summary.created_at_ms,
			updated_at_ms: summary.updated_at_ms,
		});
		// Collect the entire replay into one `Vec` and ship it as a
		// single `CoderEvent::Replay`. The frontend delivers each
		// Tauri event as its own task, so fanning a 1000-record
		// transcript out one-event-at-a-time costs ~1 ms/event in
		// pure IPC dispatch — seconds of jank on a long session.
		// One batched payload collapses that to a single IPC
		// crossing + a single frontend reduce pass. `SessionLoaded`
		// above stays a separate immediate event so the panel
		// clears its bucket and enters "replaying" mode before the
		// batch lands.
		//
		// Sub-agent records replay through a dedicated async path
		// that pulls in each sub-agent's own JSONL so the popped-
		// out transcript matches what the user originally saw, not
		// just a synthetic preview. The other variants go through
		// the sync [`emit_replay_events`] path. Both push into the
		// same buffer.
		let mut replay_events: Vec<CoderEvent> = Vec::with_capacity(record_count + 2);
		for (record, record_ts) in records.into_iter().zip(record_timestamps) {
			match record {
				SessionRecord::SubagentSpawned {
					ref tool_call_id,
					ref subagent_id,
					ref target_folder,
					ref mode,
				} => {
					replay_subagent_spawned(
						&mut replay_events,
						&dir,
						&summary.id,
						tool_call_id.clone(),
						subagent_id.clone(),
						target_folder.clone(),
						mode.clone(),
					)
					.await;
				}
				SessionRecord::SubagentFinished {
					subagent_id,
					tokens_used_estimate,
					was_error,
					result_preview: _,
				} => {
					replay_events.push(CoderEvent::SubagentFinished {
						subagent_id,
						tokens_used_estimate,
						was_error,
					});
				}
				other => emit_replay_events(&mut replay_events, other, record_ts),
			}
		}
		// Surface every orphan tool call as an errored
		// `ToolResult` event so the panel flips its row from
		// "running" to "error". The synthetic JSON content
		// matches the `{"error": "…"}`-only-key shape that
		// `emit_replay_events` (and the live runtime) treat as
		// `is_error: true`, so the rendering is identical to a
		// genuinely-failed tool.
		for orphan_id in orphan_tool_call_ids {
			// A live-parked `ask_user` prompt is not interrupted —
			// it's still waiting for the user. Leave its card in the
			// interactive "waiting" state the replayed `tool_call`
			// (from the Assistant record) already put it in.
			if live_parked_ids.contains(&orphan_id) {
				continue;
			}
			replay_events.push(CoderEvent::ToolResult {
				id: orphan_id,
				result: serde_json::json!({ "error": "Interrupted before tool completed." }),
				is_error: true,
			});
		}
		// Restore-time context-usage hint. `Provider` source when
		// we recovered a persisted `Usage` record (the ring renders
		// without the `≈` tooltip prefix), `Estimate` when we
		// fell back to bytes/4. Cache fields are non-zero only on
		// the persisted-Usage path; on the estimate path we don't
		// have any cache info to report, so the tooltip suppresses
		// the `cache:` line. The completion field tracks whatever
		// the persisted record carried (0 on the estimate path)
		// even though no turn is in flight here — the ring keys
		// off `prompt_tokens` regardless, so it's just the
		// tooltip's "completion · total" line that benefits.
		replay_events.push(CoderEvent::TokenUsage {
			prompt_tokens: restore_prompt,
			completion_tokens: restore_completion,
			total_tokens: restore_total,
			context_window: restore_context_window,
			source: restore_source,
			cache_read_tokens: restore_cache_read,
			cache_creation_tokens: restore_cache_creation,
		});
		// Clear the busy state on the frontend. Replayed `UserMessage`
		// events flip `coder.busy = true` (mirroring the live-turn
		// flow), but no `TurnComplete` is recorded in the session
		// log, so without this final nudge the panel would render
		// the "stop" button after every restore — even for a session
		// whose last turn finished cleanly hours ago. Sending an
		// explicit terminator at end-of-replay closes the replay
		// window and resets busy. When the session is genuinely still
		// running (`in_flight`), the frontend re-asserts the pip from
		// the `Replay.in_flight` flag right after applying the batch,
		// so a reopened-and-backed-out running session keeps its
		// sessions-list badge. It rides at the tail of the batch so
		// the frontend closes the replay window in the same reduce
		// pass.
		replay_events.push(CoderEvent::TurnComplete);
		sink.send(CoderEvent::Replay {
			events: replay_events,
			in_flight,
		});
		Ok(summary)
	}

	/// Delete a persisted session under the active workspace
	/// folder. Idempotent. If the deleted session has a mounted
	/// runtime, cancel its turn (if any) and drop the runtime;
	/// when it was the visible session, fall back to "no visible
	/// session" — the panel reconciles by clearing its bucket and
	/// landing on either the sessions list or a fresh blank
	/// session. Other folders' sessions are untouched.
	pub async fn delete_session(&self, id: String) -> Result<(), CoderError> {
		sessions::validate_session_id(&id)?;
		let (fs, folder_path) = self.state.active_folder_session().await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		sessions::delete(&dir, &id).await?;
		// Drop the runtime entry (and cancel its in-flight turn,
		// if any). Other sessions in the same folder keep running.
		let removed = fs.runtimes.write().await.remove(&id);
		if let Some(rt) = removed {
			if let Some(token) = rt.turn.lock().await.cancel.as_ref() {
				token.cancel();
			}
		}
		// Clear the visible pointer if it was this session — the
		// frontend's deletion handler is responsible for picking
		// a successor (open another row from the list or
		// `new_session` for a blank one).
		{
			let mut visible = fs.visible.write().await;
			if visible.as_deref() == Some(id.as_str()) {
				*visible = None;
			}
		}
		// `SessionListChanged` is folder-scoped (it advertises a
		// disk-level change, not anything specific to one runtime),
		// so it goes out with an empty `session_id` and is routed
		// through the frontend's folder-level handler.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), String::new());
		sink.send(CoderEvent::SessionListChanged);
		Ok(())
	}

	pub async fn send(&self, text: String, images: Vec<ImageAttachment>) -> Result<(), CoderError> {
		// Bail early if the active route can't authenticate —
		// surface a clean error instead of letting the inference
		// layer fail on the first request. HF needs OAuth; user
		// providers need a configured key (or a localhost
		// `base_url`, where keyless is conventional for Ollama /
		// llama.cpp).
		self.ensure_can_send().await?;
		let (rt, session_id, folder_path) = self.state.active_visible_runtime().await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);

		// A second `send` while the **visible session's** turn is
		// already in flight is a **steer**: queue the new user
		// message and let the running `run_turn` drain it at its
		// next iteration top. The composer stays open during a
		// turn so the user can nudge the model mid-flight ("also
		// do X", "actually scratch that, just summarise"). Other
		// sessions (in the same folder or other folders) can have
		// their own turns running simultaneously — see ADR 0016.
		{
			let turn = rt.turn.lock().await;
			if turn.cancel.is_some() {
				drop(turn);
				// If the turn is parked on an `ask_user` prompt, a
				// composer send is the user's "skip the questions and
				// keep driving" gesture: resolve the parked prompt
				// with `Skipped` so the tool returns and the loop
				// continues, then let the typed message flow through
				// as a normal steer below. Best-effort — a `false`
				// return means the prompt resolved between the probe
				// and now (raced an answer click), which is fine.
				if rt.prompts.has_pending().await {
					rt.prompts.skip_any().await;
				}
				// Mint the id up here so it's shared between the
				// `PendingSteer` (the backend's queue handle) and
				// the `UserMessage` event (the UI's queue handle).
				// `coder_unqueue_steer` then pops by the same id
				// the panel saw, and the matching `SteerDrained`
				// can target the same row.
				let steer_id = new_message_id();
				let mut session = rt.session.lock().await;
				session.pending_steers.push(PendingSteer {
					id: steer_id.clone(),
					text: text.clone(),
					images: images.clone(),
				});
				session.header.updated_at_ms = current_time_ms();
				drop(session);
				let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id.clone());
				sink.send(CoderEvent::UserMessage {
					id: steer_id,
					text,
					images,
					queued: true,
					created_at_ms: Some(current_time_ms()),
				});
				return Ok(());
			}
		}

		let cancel = CancellationToken::new();
		{
			let mut turn = rt.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}

		// Bind / prep the session: first `send` allocates the
		// title and locks the sessions dir; subsequent sends just
		// append.
		let (auto_rename_after, summary_to_announce) = {
			let mut session = rt.session.lock().await;
			let needs_loaded_event = session.header.title.is_empty() && session.persisted_records == 0;
			if session.session_dir.is_none() {
				session.session_dir = Some(dir.clone());
			}
			// First-persistence binds `cwd` to the workspace folder
			// root so the JSONL header carries a non-empty path —
			// pi-mono's detector ([detect.ts]) drops sessions whose
			// `cwd` isn't a string, and an empty string would still
			// pass that check but rendered as `(no folder)` in the
			// trace viewer. Idempotent: a sub-agent header already
			// carries `cwd` set in `subagent.rs::build_subagent_spec`
			// and we don't clobber it.
			if session.header.cwd.is_empty() {
				session.header.cwd = folder_path.to_string();
			}
			if session.header.title.is_empty() {
				session.header.title = session_title_from_prompt(&text);
				session.auto_rename_pending = true;
			}
			session.header.updated_at_ms = current_time_ms();
			// Capture-and-clear: snapshot whether we owe a rename,
			// then immediately clear the flag so a second `send`
			// running before the spawned rename task gets to flip
			// the flag itself can't double-spawn. The actual call
			// is fired below regardless of how the turn ends —
			// even an Esc'd or errored first turn earns a title
			// from whatever made it into the transcript.
			let auto_rename = session.auto_rename_pending;
			session.auto_rename_pending = false;
			let summary = if needs_loaded_event {
				Some(session.summary())
			} else {
				None
			};
			(auto_rename, summary)
		};
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id.clone());
		if let Some(summary) = summary_to_announce {
			// Fresh session graduating to "first message landed".
			// Tell the UI so the sticky header switches from
			// "untitled" → the truncated prompt and the sessions
			// list picks it up.
			sink.send(CoderEvent::SessionLoaded {
				id: summary.id.clone(),
				title: summary.title.clone(),
				created_at_ms: summary.created_at_ms,
				updated_at_ms: summary.updated_at_ms,
			});
			sink.send(CoderEvent::SessionListChanged);
		}

		// Append the user message to in-memory chat history + the
		// session JSONL. The disk write is best-effort: a failure
		// only loses the user's prompt from the saved transcript,
		// the in-memory turn proceeds.
		{
			let mut session = rt.session.lock().await;
			session.messages.push(ChatMessage::User {
				content: text.clone(),
				images: images.clone(),
			});
			let header = session.header.clone();
			let dir = session
				.session_dir
				.clone()
				.expect("session_dir set above before this point");
			drop(session);
			let record = SessionRecord::User {
				text: text.clone(),
				images: images.clone(),
			};
			if let Err(err) = sessions::append_record(&dir, &header, &record).await {
				tracing::warn!(error = %err, "failed to persist user message");
			} else {
				let mut session = rt.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}

		let user_id = new_message_id();
		sink.send(CoderEvent::UserMessage {
			id: user_id,
			text: text.clone(),
			images: images.clone(),
			queued: false,
			created_at_ms: Some(current_time_ms()),
		});

		let state = self.state.clone();
		let rt_for_turn = rt.clone();
		let sink_for_turn = sink.clone();
		let folder_for_turn = folder_path.clone();
		spawn_turn_loop(
			state,
			rt_for_turn,
			sink_for_turn,
			folder_for_turn,
			cancel,
			auto_rename_after,
			None,
		);
		Ok(())
	}

	/// Cancel the **active folder's visible session** turn (if
	/// any). Background turns — in the same folder's other
	/// sessions, or in any other folder — are left alone;
	/// stopping one requires switching to it first (clicking
	/// its row in the sessions list). Just trips the cancel
	/// token; the spawned turn observes it on its next `select!`
	/// and exits.
	pub async fn abort(&self) {
		let Ok((fs, _)) = self.state.active_folder_session().await else {
			return;
		};
		let Some(id) = fs.visible_session_id().await else {
			return;
		};
		let Some(rt) = fs.runtime(&id).await else {
			return;
		};
		let turn = rt.turn.lock().await;
		if let Some(token) = turn.cancel.as_ref() {
			token.cancel();
		}
	}

	/// "Go now" on a queued steer: the user typed a message mid-
	/// turn (it landed in `pending_steers` and rendered as a
	/// muted "queued" row), then decided they don't want to wait
	/// for the running turn to settle. Cancels the current turn
	/// (like [`abort`]) and flips the matching row out of "queued"
	/// styling by emitting [`CoderEvent::SteerDrained`]. The
	/// spawn loop's `Err(Aborted)` branch detects the still-pending
	/// steer, recovers orphaned tool-call results, and loops back
	/// into `run_turn`, which drains the steer into chat history
	/// and runs a fresh LLM round-trip. The UI sees an
	/// uninterrupted `busy` stretch: no `Aborted` flash, just the
	/// old thinking fading into the new turn.
	///
	/// Returns `false` (no-op) when `id` doesn't match a queued
	/// steer on the active visible session — either it was never
	/// queued, or the runner already drained it at the top of its
	/// next iteration. Auth is **not** re-gated: the steer was
	/// accepted by an already-authenticated `send`, and the model
	/// round-trip will reuse the same route that's mid-flight.
	pub async fn drain_steer_now(&self, id: &str) -> bool {
		let Ok((rt, session_id, folder_path)) = self.state.active_visible_runtime().await else {
			return false;
		};
		// Confirm the id is a live pending steer before doing
		// anything destructive. We just need existence here so a
		// stale "go now" click (the runner already drained the
		// queue at its last iteration top) is a clean no-op
		// rather than an abort with nothing to drain.
		{
			let session = rt.session.lock().await;
			if !session.pending_steers.iter().any(|s| s.id == id) {
				return false;
			}
		}
		// Skip any parked `ask_user` prompt so the tool returns
		// and the turn reaches the cancellation point (the
		// iteration boundary's `select!`).
		if rt.prompts.has_pending().await {
			rt.prompts.skip_any().await;
		}
		// Flip the row out of "queued" styling now — the spawn
		// loop's drain will re-append it as a real `User` record
		// anyway, but the visual transition reads better as
		// "queued → live" the instant the user hits go now.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id);
		sink.send(CoderEvent::SteerDrained { id: id.to_string() });
		// Trip the cancel token: the running `run_turn` returns
		// `Err(Aborted)`, the spawn loop sees the pending steer,
		// recovers orphans, and loops back to drain it.
		let turn = rt.turn.lock().await;
		if let Some(token) = turn.cancel.as_ref() {
			token.cancel();
		}
		true
	}

	/// Pop a queued steer by id from the active folder's session.
	///
	/// Returns the steer's `(text, images)` so the panel can
	/// restore the user's draft + image chips. `None` when no
	/// matching pending steer exists — either it was already
	/// drained into the chat at the top of the latest `run_turn`
	/// iteration (too late, no undo), or no folder is active.
	/// Emits a [`CoderEvent::SteerDrained`] for the popped id so
	/// the row's "queued" styling flips even if the panel didn't
	/// know about the pop ahead of time (e.g. a sibling window
	/// triggered the unqueue).
	pub async fn unqueue_steer(&self, id: &str) -> Option<UnqueuedSteer> {
		let (rt, session_id, folder_path) = self.state.active_visible_runtime().await.ok()?;
		let popped = {
			let mut session = rt.session.lock().await;
			pop_pending_steer(&mut session, id)?
		};
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id);
		sink.send(CoderEvent::SteerDrained { id: id.to_string() });
		Some(UnqueuedSteer {
			text: popped.text,
			images: popped.images,
		})
	}

	/// Resolve an in-flight `ask_user` prompt on the active
	/// folder's visible session with the user's structured answer.
	///
	/// `call_id` is the tool-call id the panel saw on the prompt's
	/// `tool_call` event. Returns `true` when a matching parked
	/// prompt was found and resolved; `false` when there was nothing
	/// to resolve (the user already skipped it via a composer send,
	/// the turn aborted, or the id is stale). A `false` return is a
	/// no-op the panel can ignore — the row will settle off the
	/// `tool_result` event either way.
	pub async fn respond_to_prompt(&self, call_id: &str, response: PromptResponse) -> bool {
		// Resolve against whichever session actually owns the prompt,
		// not the visible one — the user may have switched sessions
		// (e.g. to report progress elsewhere) and come back to answer.
		let Some(rt) = self.state.runtime_holding_prompt(call_id).await else {
			return false;
		};
		rt.prompts.resolve(call_id, PromptOutcome::Answered(response)).await
	}

	pub fn subscribe(&self) -> broadcast::Receiver<CoderEventEnvelope> {
		self.state.events.subscribe()
	}

	// ── Orchestrator-facing client surface (ADR 0030) ───────────
	//
	// By-id variants of the panel-driven methods. An orchestrator
	// session drives its workers by id (not "the visible session"),
	// so it needs `send_to` / `abort_session` / `observe_session`
	// that target a specific runtime regardless of what the user
	// has foregrounded. The visible-session methods above stay
	// unchanged for the UI path.

	/// Send a prompt to a specific session by id (ADR 0030). Unlike
	/// `send` (which targets the active folder's visible session),
	/// this resolves the runtime by id across all folders and seeds
	/// it directly. Used by an orchestrator's `spawn_worker` /
	/// `steer_worker` to drive a worker. If the worker's turn is
	/// already running, the message is queued as a steer (same as a
	/// user steering a visible session).
	pub async fn send_to(&self, session_id: &str, text: String, images: Vec<ImageAttachment>) -> Result<(), CoderError> {
		self.ensure_can_send().await?;
		let Some((rt, folder_path)) = self.state.runtime_for_session(session_id).await else {
			return Err(CoderError::Internal(format!(
				"no mounted runtime for session {session_id}"
			)));
		};
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		// Steer-vs-spawn branch mirrors `send`. A worker whose turn
		// is in flight gets the message queued as a steer.
		{
			let turn = rt.turn.lock().await;
			if turn.cancel.is_some() {
				drop(turn);
				if rt.prompts.has_pending().await {
					rt.prompts.skip_any().await;
				}
				let steer_id = new_message_id();
				let mut session = rt.session.lock().await;
				session.pending_steers.push(PendingSteer {
					id: steer_id.clone(),
					text: text.clone(),
					images: images.clone(),
				});
				session.header.updated_at_ms = current_time_ms();
				drop(session);
				let sink = FolderEventSink::new(
					self.state.events.clone(),
					folder_path.to_string(),
					session_id.to_string(),
				);
				sink.send(CoderEvent::UserMessage {
					id: steer_id,
					text,
					images,
					queued: true,
					created_at_ms: Some(current_time_ms()),
				});
				return Ok(());
			}
		}
		let cancel = CancellationToken::new();
		{
			let mut turn = rt.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}
		// Bind / prep the session: first `send_to` allocates the
		// title and locks the sessions dir, same as `send`.
		let (auto_rename_after, summary_to_announce) = {
			let mut session = rt.session.lock().await;
			let needs_loaded_event = session.header.title.is_empty() && session.persisted_records == 0;
			if session.session_dir.is_none() {
				session.session_dir = Some(dir.clone());
			}
			if session.header.cwd.is_empty() {
				session.header.cwd = folder_path.to_string();
			}
			if session.header.title.is_empty() {
				session.header.title = crate::sessions::session_title_from_prompt(&text);
				session.auto_rename_pending = true;
			}
			session.header.updated_at_ms = current_time_ms();
			let auto_rename = session.auto_rename_pending;
			session.auto_rename_pending = false;
			let summary = if needs_loaded_event {
				Some(session.summary())
			} else {
				None
			};
			(auto_rename, summary)
		};
		let sink = FolderEventSink::new(
			self.state.events.clone(),
			folder_path.to_string(),
			session_id.to_string(),
		);
		if let Some(summary) = &summary_to_announce {
			sink.send(CoderEvent::SessionLoaded {
				id: summary.id.clone(),
				title: summary.title.clone(),
				created_at_ms: summary.created_at_ms,
				updated_at_ms: summary.updated_at_ms,
			});
			sink.send(CoderEvent::SessionListChanged);
		}
		// Append the user message to in-memory chat history + the
		// session JSONL, mirroring `send`'s persist path.
		{
			let mut session = rt.session.lock().await;
			session.messages.push(ChatMessage::User {
				content: text.clone(),
				images: images.clone(),
			});
			let header = session.header.clone();
			let dir = session.session_dir.clone().expect("session_dir set above");
			drop(session);
			let record = SessionRecord::User {
				text: text.clone(),
				images: images.clone(),
			};
			if let Err(err) = sessions::append_record(&dir, &header, &record).await {
				tracing::warn!(error = %err, "failed to persist worker seed message");
			} else {
				let mut session = rt.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}
		sink.send(CoderEvent::UserMessage {
			id: new_message_id(),
			text,
			images,
			queued: false,
			created_at_ms: Some(current_time_ms()),
		});
		// Spawn the turn via the shared loop helper — same detached-task
		// shape as `send`, so the worker gets the same steer-race
		// recovery, format-queue flush, abort backfill, auto-rename,
		// and hub-sync behaviour. The worker runs independently of
		// the orchestrator's turn.
		let state = self.state.clone();
		let rt_for_turn = rt.clone();
		let sink_for_turn = sink.clone();
		let folder_for_turn = folder_path.clone();
		spawn_turn_loop(
			state,
			rt_for_turn,
			sink_for_turn,
			folder_for_turn,
			cancel,
			auto_rename_after,
			None,
		);
		Ok(())
	}

	/// Abort a specific session's in-flight turn by id (ADR 0030).
	/// No-op when the session isn't mounted or has no turn running.
	/// Used by an orchestrator's `abort_worker`.
	pub async fn abort_session(&self, session_id: &str) {
		let Some((rt, _)) = self.state.runtime_for_session(session_id).await else {
			return;
		};
		let turn = rt.turn.lock().await;
		if let Some(token) = turn.cancel.as_ref() {
			token.cancel();
		}
	}

	/// Fetch a compact snapshot of a session's current state by id
	/// (ADR 0030). The shape an orchestrator's `observe_worker` tool
	/// returns — enough to decide what to do next without reading
	/// the worker's full transcript. Returns `None` when the session
	/// isn't mounted.
	pub async fn observe_session(&self, session_id: &str) -> Option<WorkerSnapshot> {
		let (rt, folder_path) = self.state.runtime_for_session(session_id).await?;
		let session = rt.session.lock().await;
		let running = rt.turn.lock().await.cancel.is_some();
		let needs_input = rt.prompts.has_pending().await;
		let turns = session
			.messages
			.iter()
			.filter(|m| matches!(m, ChatMessage::Assistant { .. }))
			.count();
		// Last assistant text — the most recent thing the worker said.
		let last_assistant = session
			.messages
			.iter()
			.rev()
			.find_map(|m| match m {
				ChatMessage::Assistant { content: Some(t), .. } => Some(t.clone()),
				_ => None,
			})
			.unwrap_or_default();
		Some(WorkerSnapshot {
			session_id: session_id.to_string(),
			folder: folder_path.to_string(),
			title: session.header.title.clone(),
			branch: session
				.header
				.worktree_branch
				.clone()
				.or(session.header.committed_branch.clone())
				.unwrap_or_default(),
			turns: turns as u32,
			running,
			needs_input,
			last_assistant,
		})
	}
}

/// Compact snapshot of a worker session's current state (ADR 0030).
/// The shape an orchestrator's `observe_worker` tool returns — enough
/// to decide what to do next (steer, abort, answer, report) without
/// reading the worker's full transcript. Mirrors the dispatch-packet
/// discipline: self-contained per-worker, not a transcript dump.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerSnapshot {
	/// The worker's session id (the handle `spawn_worker` returned).
	pub session_id: String,
	/// Folder the worker is filed under (the parent project root).
	pub folder: String,
	/// Human-readable title (from the seed prompt or auto-rename).
	pub title: String,
	/// Branch the worker's work lands on (worktree branch, or the
	/// committed branch for a main-tree session). Empty when neither
	/// is set yet.
	pub branch: String,
	/// Number of assistant turns the worker has completed so far.
	pub turns: u32,
	/// Whether the worker currently has a turn in flight.
	pub running: bool,
	/// Whether the worker is parked on an `ask_user` prompt waiting
	/// for an answer (from the orchestrator or the user).
	pub needs_input: bool,
	/// The worker's most recent assistant message text. Empty when
	/// the worker hasn't produced one yet.
	pub last_assistant: String,
}

/// Result of a successful [`Coder::unqueue_steer`] — the bytes the
/// panel needs to repopulate the composer. Serialised over the
/// Tauri command boundary in the obvious shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnqueuedSteer {
	pub text: String,
	#[serde(default)]
	pub images: Vec<ImageAttachment>,
}

/// Result of [`Coder::revert_to_message`] — the dropped user
/// prompt, handed back so an "edit & resend" can prefill the
/// composer. A plain "revert to here" ignores it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RevertedMessage {
	pub text: String,
	#[serde(default)]
	pub images: Vec<ImageAttachment>,
}

/// Rebuilt chat history from persisted records, returned by
/// [`Coder::rebuild_messages_from_records`]. Carries the
/// last-wins `usage` and `todos` alongside `messages` so both
/// `open_session` and `resume_from_assistant` can seed the
/// runtime's state without duplicating the record-walk logic.
struct RebuiltMessages {
	messages: Vec<ChatMessage>,
	last_usage: Option<TokenUsage>,
	last_todos: Vec<crate::TodoItem>,
}

/// Result of [`Coder::rerun_tool_call`] — the tool that was
/// reapplied plus its fresh dispatch result, handed back so the
/// panel can confirm the reapply (e.g. "reapplied 1.2 kB"). Only
/// the success payload reaches here; a dispatch failure propagates
/// as `Err` instead.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RerunToolOutcome {
	pub tool_name: String,
	pub result: Value,
}

/// Find a recorded tool call by id in a session's message history
/// and return its `(name, parsed args)`. Tool calls only ever live
/// on assistant messages' `tool_calls`, so that's all we scan.
/// `None` when no recorded call matches — already reverted away, a
/// sub-agent's own call (not on the parent's transcript), or a
/// stale id. Pure over `&[ChatMessage]` so it's unit-testable
/// without a runtime.
fn find_recorded_tool_call(messages: &[ChatMessage], tool_call_id: &str) -> Option<(String, Value)> {
	for msg in messages {
		let ChatMessage::Assistant { tool_calls, .. } = msg else {
			continue;
		};
		for call in tool_calls {
			if call.id == tool_call_id {
				return Some((call.function.name.clone(), parse_tool_args(&call.function)));
			}
		}
	}
	None
}

/// Remove the first matching pending steer from `session` and
/// return it. `None` when the id isn't in the queue — the steer
/// has already been drained, or the panel sent us a stale id. Pure
/// over `&mut Session` so the unit tests don't need a folder /
/// runtime.
fn pop_pending_steer(session: &mut Session, id: &str) -> Option<PendingSteer> {
	let idx = session.pending_steers.iter().position(|s| s.id == id)?;
	Some(session.pending_steers.remove(idx))
}

/// Drain the turn's [`FormatQueue`] and run `host.format_file`
/// against each touched path exactly once. Best-effort: a missing
/// folder or a `format_file` error collapses to a `tracing::warn!`
/// and the next path is still attempted. Fires after every turn
/// (Ok / Aborted / Err) so a partial turn still lands formatted
/// bytes for whatever the model managed to write before bailing —
/// which matches the "treat the model's writes like ordinary
/// `Ctrl+S` saves" mental model the rest of the IDE has.
async fn flush_format_queue(state: &Arc<CoderState>, queue: &Arc<crate::tools::FormatQueue>) {
	let entries = queue.drain();
	if entries.is_empty() {
		return;
	}
	for (folder_path, rel) in entries {
		let Some(folder) = state.workspaces.folder_for_path(folder_path.as_str()).await else {
			tracing::warn!(
				folder = %folder_path,
				path = %rel,
				"format-on-save (turn end): bound folder gone before flush; skipping"
			);
			continue;
		};
		if let Err(err) = folder.host.format_file(&rel).await {
			tracing::warn!(
				folder = %folder_path,
				path = %rel,
				%err,
				"format-on-save (turn end): format_file failed"
			);
		}
	}
}

/// After a successful turn, check the workspace's
/// [`coder_hub_bucket`] binding and, if `autosync` is on, enqueue
/// a debounced upload of the active session's JSONL. Fire-and-
/// forget — the turn task never blocks on the upload. Silently
/// no-ops when there's no binding, when autosync is off, or when
/// the workspace's `session.json` fails to load (we log the
/// failure but don't surface it; the next turn retries).
async fn maybe_autosync_to_hub(state: &Arc<CoderState>, rt: &Arc<SessionRuntime>, folder_path: &Utf8Path) {
	let workspace_id = state.workspaces.workspace_id().await;
	let workspace_session = match moon_core::session::load(&state.workspaces_dir, &workspace_id).await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "hub autosync: could not read session.json");
			return;
		}
	};
	let Some(bucket) = workspace_session.coder_hub_bucket else {
		return;
	};
	if !bucket.autosync {
		return;
	}
	let session_id = {
		let session = rt.session.lock().await;
		// An empty session has nothing to push — guard against
		// the (rare but possible) race where the turn task
		// finished but no records were ever persisted.
		if session.persisted_records == 0 {
			return;
		}
		session.header.id.clone()
	};
	state
		.hub_sync
		.enqueue_session_sync(workspace_id, folder_path.to_path_buf(), session_id);
}

/// Spawn the turn loop as a detached task. Shared by `Coder::send`
/// (visible-session path) and `Coder::send_to` (by-id orchestrator
/// path, ADR 0030) so both get the same steer-race recovery, format-
/// queue flush, abort backfill, auto-rename, and hub-sync behaviour.
/// Closing over `Arc<SessionRuntime>` is what makes the turn run
/// detached — it keeps operating on its session regardless of what
/// the user has foregrounded (ADR 0016).
fn spawn_turn_loop(
	state: Arc<CoderState>,
	rt_for_turn: Arc<SessionRuntime>,
	sink_for_turn: FolderEventSink,
	folder_for_turn: Utf8PathBuf,
	cancel: CancellationToken,
	auto_rename_after: bool,
	resume_tool_calls: Option<Vec<crate::inference::ToolCall>>,
) {
	tokio::spawn(async move {
		// Loop wrapper closes the race between `run_turn` returning
		// `Ok(())` and the spawn task clearing `turn.cancel`: a steer
		// queued in that window lands in `pending_steers` but would
		// otherwise be orphaned. Re-checking here under both the
		// `turn` and `session` locks linearises with `send`'s
		// turn→session take order.
		let mut cancel_outer = cancel;
		let mut resume = resume_tool_calls;
		let result = loop {
			let format_queue = Arc::new(crate::tools::FormatQueue::default());
			let result = run_turn(
				&state,
				&rt_for_turn,
				&folder_for_turn,
				&sink_for_turn,
				cancel_outer.clone(),
				format_queue.clone(),
				// Only the first run_turn call gets the resume
				// tool calls; subsequent loop iterations (steer
				// drains after an abort) start fresh.
				resume.take(),
			)
			.await;
			flush_format_queue(&state, &format_queue).await;
			if matches!(result, Err(CoderError::Aborted)) && !rt_for_turn.session.lock().await.pending_steers.is_empty() {
				recover_in_memory_orphans(&rt_for_turn, &sink_for_turn).await;
				cancel_outer = fresh_cancel(&rt_for_turn).await;
				continue;
			}
			if !matches!(result, Ok(())) {
				rt_for_turn.turn.lock().await.cancel = None;
				break result;
			}
			let mut turn = rt_for_turn.turn.lock().await;
			if rt_for_turn.session.lock().await.pending_steers.is_empty() {
				turn.cancel = None;
				break result;
			}
			let fresh = CancellationToken::new();
			turn.cancel = Some(fresh.clone());
			drop(turn);
			cancel_outer = fresh;
		};
		match &result {
			Ok(()) => {
				sink_for_turn.send(CoderEvent::TurnComplete);
				maybe_autosync_to_hub(&state, &rt_for_turn, &folder_for_turn).await;
			}
			Err(CoderError::Aborted) => {
				recover_in_memory_orphans(&rt_for_turn, &sink_for_turn).await;
				sink_for_turn.send(CoderEvent::Aborted);
			}
			Err(err) => {
				tracing::warn!(error = %err, "coder turn failed");
				persist_error_record(&rt_for_turn, &err.to_string()).await;
				sink_for_turn.send(CoderEvent::Error {
					message: err.to_string(),
				});
			}
		}
		if auto_rename_after {
			spawn_auto_rename(state.clone(), rt_for_turn.clone(), sink_for_turn);
		}
	});
}

async fn run_turn(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	folder_path: &Utf8Path,
	sink: &FolderEventSink,
	cancel: CancellationToken,
	format_queue: Arc<crate::tools::FormatQueue>,
	mut resume_tool_calls: Option<Vec<crate::inference::ToolCall>>,
) -> Result<(), CoderError> {
	// Snapshot the user's current model picks once at turn-start.
	// A settings flip mid-turn doesn't retroactively change which
	// model the in-flight requests are talking to; the *next* turn
	// (or sub-agent, or auto-rename) will see the new pick. `bill_to`
	// is read fresh per request via the shared handle inside
	// `InferenceClient` instead.
	let models = state.models.read().await.clone();
	let standard_model = models.standard().to_owned();
	// Resolved route snapshot so every persisted assistant
	// record carries a `provider/model` stamp matching what
	// actually served the round-trip. Snapshotted alongside
	// `models` so a settings flip mid-turn doesn't make this
	// turn's record claim a route it never spoke to.
	let pi_model = models.resolve_route().pi_provider_model(&standard_model);

	// Pin the tool context to the **session's** bound folder
	// (captured at spawn time), not the live `active_folder()`.
	// This is what makes "agent keeps running in folder X while
	// user browses folder Y" actually work: the spawned `run_turn`
	// closes over its `folder_path`, so its tools always operate
	// against folder X regardless of whatever the user has
	// foregrounded in the IDE.
	// Worktree-backed sessions (ADR 0028) route their tools to the
	// session's git worktree while staying filed under the parent
	// folder (`folder_path`) for persistence + events. Fall back to
	// the parent when the worktree isn't bound — e.g. before
	// startup re-binding lands (W.3) or if the user discarded it.
	let worktree_root = rt.session.lock().await.header.worktree_root.clone();
	let routing_path: Utf8PathBuf = match worktree_root {
		Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root.into(),
		_ => folder_path.to_path_buf(),
	};
	let folder_entry = state
		.workspaces
		.folder_for_path(routing_path.as_str())
		.await
		.ok_or(CoderError::NoActiveFolder)?;
	// Snapshot the per-session bash-target override + the top-level
	// session mode once at turn-start (same posture as `models`
	// above): a toggle / re-stamp mid-turn applies to the *next*
	// turn, not in-flight commands. The mode drives both the
	// `ToolContext` (so the dispatch-level write gate is right)
	// and the tool-list composition below.
	let (force_host_bash, mode) = {
		let session = rt.session.lock().await;
		(
			session.header.bash_target_override == Some(BashTargetOverride::ForceHost),
			CoderMode::from_top_level_wire(session.header.mode.as_deref()),
		)
	};
	let cx = ToolContext::with_format_queue(folder_entry, mode, format_queue).with_force_host_bash(force_host_bash);
	// Tool list composition branches on the top-level session mode
	// (ADR 0030). The historical shape — `definitions()` plus
	// `task` plus `ask_user` — is the ordinary `Agent` mode.
	// `Coordinator` swaps the parent-only tools: `spawn_worker` and
	// the worker-management tools replace `task`/`ask_user`, and the
	// coordinator can't mutate files directly (it must delegate),
	// though the write tools stay advertised so the dispatch-level
	// gate produces a clear `read_only_mode` error if the model
	// tries. Sub-agents pick from the registry alone (no `task`),
	// which is how the depth-1 cap is enforced — a sub-agent
	// literally cannot describe a sub-sub-agent because the model
	// never sees the tool.
	let mut tool_defs = state.tools.definitions();
	if mode.allows_task_tool() {
		tool_defs.push(task_tool_definition());
	}
	if mode.allows_ask_user() {
		tool_defs.push(ask_user_tool_definition());
	}
	if mode == CoderMode::Coordinator {
		tool_defs.push(crate::coordinator::spawn_worker_tool_definition());
		tool_defs.push(crate::coordinator::observe_worker_tool_definition());
		tool_defs.push(crate::coordinator::steer_worker_tool_definition());
		tool_defs.push(crate::coordinator::abort_worker_tool_definition());
		tool_defs.push(crate::coordinator::respond_to_worker_prompt_tool_definition());
	}
	// Compose a fresh system prompt and overwrite the session's
	// `messages[0]`: the base prompt plus a "Bound folders"
	// section keyed off whatever summaries are currently cached.
	// Sub-agent dispatch reads the same cache so the model's
	// awareness of bound folders is consistent across parent +
	// sub-agent prompts.
	// Use the routing path (worktree when bound, else parent) as the
	// prompt's "active folder" so the agent is oriented at the
	// checkout its tools actually operate on.
	refresh_system_prompt(state, rt, &routing_path, force_host_bash, mode).await;
	// Schedule background regeneration for any bound folder whose
	// summary cache is missing or stale. Detached tokio tasks; we
	// don't block the turn waiting for them to land. The next
	// turn will pick up whichever finished in the interim via the
	// fresh `refresh_system_prompt` above.
	kick_off_summary_refresh(state, sink).await;
	for _iter in 0..MAX_TURN_ITERATIONS {
		if cancel.is_cancelled() {
			return Err(CoderError::Aborted);
		}

		// Drain any user steers queued via `send()` while this
		// turn was running. Each one becomes a real
		// `ChatMessage::User` in the prompt and a
		// `SessionRecord::User` on disk. We persist here (not at
		// queue time) because the chat shape forbids a user
		// message between an `assistant` with `tool_calls` and
		// its `tool` rows; queuing during `dispatch_tool_calls`
		// and persisting then would interleave them and break
		// session reload. Compaction below sees the steers in
		// `messages` and folds them like any other history.
		drain_pending_steers(rt, sink).await;

		// Token-aware compaction before each round-trip. Reads the
		// session's last-seen usage; if it crossed the threshold,
		// runs a fast-model summary and rewrites `messages` in
		// place. We also persist a `Compaction` record into the
		// JSONL so reloading the session reaches the same shape —
		// otherwise replay re-inflates the full pre-compaction
		// transcript and the next turn instantly trips the
		// provider's context-length cap.
		let last_usage = rt.session.lock().await.last_usage;
		let mut messages = rt.session.lock().await.messages.clone();
		let compaction = crate::compaction::compact_if_needed(
			&state.inference,
			sink,
			None,
			&models,
			last_usage.as_ref(),
			&mut messages,
			&cancel,
		)
		.await;
		if let Some(applied) = compaction {
			let (header, dir) = {
				let mut session = rt.session.lock().await;
				session.messages = messages.clone();
				// Re-anchor the trigger on an estimate of the
				// freshly-compacted prompt rather than clearing it.
				// `None` would skip the compaction check entirely on
				// the next iteration (it early-returns on missing
				// usage), so a single pass that didn't get under the
				// threshold — a large summary plus heavy kept turns —
				// would sail one over-budget prompt to the provider
				// before the next response's usage re-armed the
				// guard. Seeding the estimate keeps the guard live
				// and re-fires immediately if we're still over.
				let estimate = estimate_prompt_tokens(&messages);
				session.last_usage = Some(TokenUsage {
					prompt_tokens: estimate,
					completion_tokens: 0,
					total_tokens: estimate,
					cache_read_input_tokens: 0,
					cache_creation_input_tokens: 0,
				});
				(session.header.clone(), session.session_dir.clone())
			};
			if let Some(dir) = dir {
				let record = SessionRecord::Compaction {
					summary: applied.summary,
					messages_compacted: applied.messages_compacted,
					messages_kept: applied.messages_kept,
				};
				if let Err(err) = sessions::append_record(&dir, &header, &record).await {
					tracing::warn!(error = %err, "failed to persist compaction record; reload will re-inflate the prefix");
				} else {
					let mut session = rt.session.lock().await;
					session.persisted_records = session.persisted_records.saturating_add(1);
				}
			}
		}

		// Resume-from-checkpoint: on the first iteration only,
		// re-dispatch the kept Assistant's pre-existing tool calls
		// against the current workspace instead of calling the
		// model. The fresh `Tool` results land in `messages` and on
		// disk via the normal `dispatch_tool_calls` →
		// `finish_tool_call` path, then `continue` runs the next
		// iteration which makes a real LLM call with those results
		// in context. Takes ownership once; subsequent iterations
		// see `None` and follow the normal LLM-call path.
		if let Some(calls) = resume_tool_calls.take() {
			if !calls.is_empty() {
				dispatch_tool_calls(state, rt, sink, &cx, &cancel, &calls).await?;
				continue;
			}
		}

		// One stable id per assistant message, shared between the
		// `start`, every content / thinking `delta`, and the final
		// `end` event so the frontend can reconcile by id (see the
		// `tool_call` / `tool_result` pattern). A fresh id every
		// loop iteration — multi-iteration turns with tool calls
		// produce multiple assistant messages.
		let assistant_id = new_message_id();
		let content_started = std::sync::atomic::AtomicBool::new(false);
		let thinking_emitted = std::sync::atomic::AtomicBool::new(false);
		let sink_for_cb = sink.clone();
		let id_for_cb = assistant_id.clone();

		// Real-time token-usage estimates. We send a prompt-only
		// estimate the moment the round-trip starts so the
		// context-usage ring jumps as soon as the user hits send
		// (or a tool result lands), instead of waiting for the
		// provider's final usage chunk. While the assistant
		// streams we update the completion side at most every
		// `STREAM_USAGE_THROTTLE` so the panel reflects "the
		// model is producing a lot of text" without firing an
		// event per delta. The post-call `emit_token_usage` below
		// overrides everything with provider-exact numbers when
		// the chunk arrives.
		//
		// Anchor the pre-call estimate on the prior turn's
		// `last_usage` whenever we have one: the new prompt is
		// the previous prompt + the previous assistant response +
		// whatever was appended afterwards (new user message
		// and/or tool results). Carrying the exact numbers
		// forward and only estimating the tail keeps the ring
		// from shrinking back to a bytes/4 approximation on
		// providers (Anthropic especially) where bytes/4
		// undercounts what the tokenizer actually sees. First
		// turn has no `last_usage`, so we fall back to bytes/4 of
		// the whole array.
		const STREAM_USAGE_THROTTLE: std::time::Duration = std::time::Duration::from_millis(500);
		let prompt_estimate = estimate_prompt_with_anchor(last_usage.as_ref(), &messages);
		let context_window = models.context_window(&standard_model);
		sink.send(CoderEvent::TokenUsage {
			prompt_tokens: prompt_estimate,
			completion_tokens: 0,
			total_tokens: prompt_estimate,
			context_window,
			source: TokenUsageSource::Estimate,
			cache_read_tokens: 0,
			cache_creation_tokens: 0,
		});
		// `Mutex` rather than `Cell` because the future the
		// closure participates in is required to be `Send` —
		// `tokio::spawn` requires a `Send` future, and `Cell` is
		// not `Sync`. The closure runs sequentially from a single
		// task so there's no real contention.
		let stream_usage_state = std::sync::Mutex::new((0u32, std::time::Instant::now()));

		let response = state
			.inference
			.chat_completion_stream(&standard_model, &messages, &tool_defs, &cancel, |event| match event {
				StreamEvent::ContentDelta { delta } => {
					if !content_started.swap(true, std::sync::atomic::Ordering::Relaxed) {
						sink_for_cb.send(CoderEvent::AssistantMessageStart { id: id_for_cb.clone() });
					}
					sink_for_cb.send(CoderEvent::AssistantMessageDelta {
						id: id_for_cb.clone(),
						delta: delta.to_string(),
					});
					maybe_emit_stream_usage(
						&sink_for_cb,
						&stream_usage_state,
						STREAM_USAGE_THROTTLE,
						delta.len(),
						prompt_estimate,
						context_window,
					);
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
						sink_for_cb.send(CoderEvent::AssistantMessageStart { id: id_for_cb.clone() });
					}
					thinking_emitted.store(true, std::sync::atomic::Ordering::Relaxed);
					sink_for_cb.send(CoderEvent::AssistantThinkingDelta {
						id: id_for_cb.clone(),
						delta: delta.to_string(),
					});
					maybe_emit_stream_usage(
						&sink_for_cb,
						&stream_usage_state,
						STREAM_USAGE_THROTTLE,
						delta.len(),
						prompt_estimate,
						context_window,
					);
				}
				// Tool-call deltas are intentionally not surfaced.
				// The runner buffers them inside the inference
				// client and dispatches once the whole call is
				// assembled — partial JSON arguments aren't
				// useful to render.
				StreamEvent::ToolCallDelta { .. } => {}
			})
			.await?;

		// An assistant response with no text, no thinking, and no
		// tool_calls is an empty shell — providers occasionally
		// emit one when they bail mid-stream or return only a
		// usage chunk. Pushing it onto `messages` / persisting it
		// corrupts the next turn's history: Anthropic rejects
		// assistant blocks that are empty or whitespace-only
		// (`messages: text content blocks must contain
		// non-whitespace text`), and on reload an empty record
		// re-inflates into a `ChatMessage::Assistant` that trips
		// the same 400. Drop it on the floor here; the usage
		// figures still flow through `last_usage` /
		// `persist_usage_record` / `emit_token_usage` below so
		// the ring + compaction trigger stay accurate.
		let response_is_empty = assistant_response_is_empty(&response);
		if !response_is_empty {
			let mut session = rt.session.lock().await;
			session.messages.push(response_to_message(&response));
		}
		{
			// Stash whatever usage we have for the next iteration's
			// compaction decision. Provider-supplied is exact; we
			// synthesise a `TokenUsage` from the bytes/4 estimate
			// when missing so the threshold check still has a
			// number to compare against.
			let mut session = rt.session.lock().await;
			session.last_usage = Some(response.usage.unwrap_or_else(|| {
				let prompt = estimate_prompt_tokens(&messages);
				let completion = estimate_completion_tokens(&response);
				TokenUsage {
					prompt_tokens: prompt,
					completion_tokens: completion,
					total_tokens: prompt + completion,
					cache_read_input_tokens: 0,
					cache_creation_input_tokens: 0,
				}
			}));
		}
		if !response_is_empty {
			persist_assistant_record(rt, &response, Some(pi_model.clone())).await;
		}
		// Persist provider usage too, so a session reopened later
		// — by the same IDE process or a fresh launch — restores
		// the panel's context-usage ring with provider-exact
		// figures from the moment the transcript appears, instead
		// of the bytes/4 estimate that's `≈20–30 %` off in
		// practice. No-op when the provider didn't emit usage;
		// the open path falls back to the estimate in that case.
		persist_usage_record(rt, &response).await;

		// Per-iteration token usage report. Drives the in-panel
		// usage ring + the auto-compaction trigger. Provider-supplied
		// numbers are exact; falls back to a bytes/4 estimate when
		// the provider didn't emit a streaming usage chunk so the
		// ring still moves on every turn.
		emit_token_usage(sink, &models, &standard_model, &messages, &response);

		// Always emit `End` *if* we ever started a bubble and the
		// final response actually carries something to render;
		// otherwise the frontend would be stuck with an empty
		// placeholder. The sequencing is `Start (once) → N × Delta
		// (content and/or thinking) → End` — the UI uses
		// `End.text` / `End.thinking` as the canonical replacements
		// so any drift between concatenated deltas and the final
		// assembly heals on close. Skipping `End` on an empty-shell
		// response (no text, no thinking, no tool calls) lets the
		// panel's start-without-end recovery prune the orphan
		// bubble — without this an Anthropic turn that bailed
		// mid-stream would leave a permanent empty row.
		if content_started.into_inner() && !response_is_empty {
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
			sink.send(CoderEvent::AssistantMessageEnd {
				id: assistant_id,
				text: response.content.clone().unwrap_or_default(),
				thinking: canonical_thinking,
				created_at_ms: Some(current_time_ms()),
			});
		}

		if response.tool_calls.is_empty() {
			// Final assistant message of the turn — unless the user
			// queued a steer while it was streaming. We can't just
			// return: `drain_pending_steers` runs at the **top** of
			// each iteration, so a steer landing in `pending_steers`
			// during the last response would be orphaned (the queue
			// would sit there with no future iteration to drain it,
			// because the next `send` starts a fresh turn rather than
			// resuming this one). Peek the queue under the session
			// lock; if non-empty, fall through to the next iteration
			// where the existing drain at the top consumes it. The
			// post-`run_turn` spawn task closes the residual race
			// (steer queued between this check and the spawn task
			// clearing `cancel`).
			if rt.session.lock().await.pending_steers.is_empty() {
				return Ok(());
			}
			continue;
		}

		dispatch_tool_calls(state, rt, sink, &cx, &cancel, &response.tool_calls).await?;
	}

	// Iteration cap reached. Rather than just bailing with an
	// error banner — which leaves the user staring at a wall of
	// tool calls and no actual answer — we ask the model for one
	// final, tools-disabled wrap-up turn. It sees the full history
	// it just produced, the tool budget exhausted note, and is
	// instructed to write its best answer with what it has.
	wrap_up_final_answer(state, rt, sink, &cancel, &tool_defs).await
}

/// Final tools-disabled round-trip after the iteration cap is hit.
/// Appends a sentinel user message asking the model to finish and
/// streams the response with `tools = []` so the model literally
/// cannot call another tool. The wrap-up message is persisted in
/// the JSONL transcript like any other user turn — it's part of
/// the conversation now, not a hidden side-channel; rereading the
/// session later makes it obvious why the assistant suddenly
/// stopped using tools.
///
/// The sentinel is also visible in the panel as a regular user
/// row so the human running the session sees what happened.
/// `tool_defs` is logged but unused on the wire — kept in scope so
/// callers can grep for "the tools that were available at cap time".
async fn wrap_up_final_answer(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	cancel: &CancellationToken,
	tool_defs: &[crate::inference::ToolDefinition],
) -> Result<(), CoderError> {
	tracing::info!(
		iterations = MAX_TURN_ITERATIONS,
		tools_at_cap = tool_defs.len(),
		"iteration cap reached; asking the model for a final tools-disabled wrap-up",
	);
	let models = state.models.read().await.clone();
	let standard_model = models.standard().to_owned();
	let pi_model = models.resolve_route().pi_provider_model(&standard_model);

	let sentinel_id = new_message_id();
	let sentinel_text = format!(
		"[Tool-call budget exhausted: you've used all {MAX_TURN_ITERATIONS} tool-call iterations available for this turn. \
Do not call any more tools. Write a final response now using only what you've already gathered: summarise what was \
done, what's still unfinished, and any uncertainty. If the user needs to take a follow-up action, say so explicitly.]"
	);
	{
		let mut session = rt.session.lock().await;
		session.messages.push(ChatMessage::user(sentinel_text.clone()));
	}
	{
		// Best-effort persist of the sentinel into the JSONL — same
		// shape as a real user turn so re-loading the session shows
		// it inline. Lives entirely inside the lock-then-drop dance
		// the regular user-message path uses, just inlined since
		// we don't need a separate helper for the one-off case.
		let session = rt.session.lock().await;
		let header = session.header.clone();
		let dir = session.session_dir.clone();
		drop(session);
		if let Some(dir) = dir {
			if let Err(err) = sessions::append_record(
				&dir,
				&header,
				&SessionRecord::User {
					text: sentinel_text.clone(),
					images: Vec::new(),
				},
			)
			.await
			{
				tracing::warn!(error = %err, "failed to persist tool-cap sentinel user message");
			} else {
				let mut session = rt.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}
	}
	sink.send(CoderEvent::UserMessage {
		id: sentinel_id,
		text: sentinel_text,
		images: Vec::new(),
		queued: false,
		created_at_ms: Some(current_time_ms()),
	});

	let messages = rt.session.lock().await.messages.clone();
	let assistant_id = new_message_id();
	let id_for_cb = assistant_id.clone();
	let sink_for_cb = sink.clone();
	let started = std::sync::atomic::AtomicBool::new(false);
	let thinking_emitted = std::sync::atomic::AtomicBool::new(false);
	let response = state
		.inference
		.chat_completion_stream(&standard_model, &messages, &[], cancel, |event| match event {
			StreamEvent::ContentDelta { delta } => {
				if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
					sink_for_cb.send(CoderEvent::AssistantMessageStart { id: id_for_cb.clone() });
				}
				sink_for_cb.send(CoderEvent::AssistantMessageDelta {
					id: id_for_cb.clone(),
					delta: delta.to_string(),
				});
			}
			StreamEvent::ThinkingDelta { delta } => {
				if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
					sink_for_cb.send(CoderEvent::AssistantMessageStart { id: id_for_cb.clone() });
				}
				thinking_emitted.store(true, std::sync::atomic::Ordering::Relaxed);
				sink_for_cb.send(CoderEvent::AssistantThinkingDelta {
					id: id_for_cb.clone(),
					delta: delta.to_string(),
				});
			}
			StreamEvent::ToolCallDelta { .. } => {
				// Tools were disabled in the request; if the model
				// still emits a tool-call delta we silently drop it.
				// The dispatcher won't run anything since we won't
				// loop again.
			}
		})
		.await?;

	// Same empty-shell guard as the main loop: skip pushing /
	// persisting / emitting an `End` for an Anthropic turn that
	// bailed mid-stream. See [`assistant_response_is_empty`].
	let response_is_empty = assistant_response_is_empty(&response);
	if started.into_inner() && !response_is_empty {
		let canonical_thinking = if thinking_emitted.into_inner() {
			response.thinking.clone()
		} else {
			None
		};
		sink.send(CoderEvent::AssistantMessageEnd {
			id: assistant_id,
			text: response.content.clone().unwrap_or_default(),
			thinking: canonical_thinking,
			created_at_ms: Some(current_time_ms()),
		});
	}

	if !response_is_empty {
		rt.session.lock().await.messages.push(response_to_message(&response));
		persist_assistant_record(rt, &response, Some(pi_model)).await;
	}
	emit_token_usage(sink, &models, &standard_model, &messages, &response);

	Ok(())
}

/// Limit on concurrent sub-agents per parent batch. A
/// `Semaphore`-bound; only meaningful when the model emits a
/// homogeneous `task` batch larger than this. Excess sub-agents
/// queue against the semaphore. Hardcoded for now per AGENTS.md
/// "hardcode first, configure later" — bumps land when a real
/// workload outgrows it.
const SUBAGENT_PARALLELISM_CAP: usize = 4;

/// Run every `tool_call` in `calls`, emitting the `ToolCall` /
/// `ToolResult` event pair for each and pushing the result onto
/// the session's messages. Branches:
///
/// - **Homogeneous `task` batch (N ≥ 2)**: spawn each sub-agent
///   concurrently, bounded by [`SUBAGENT_PARALLELISM_CAP`].
///   Tool-call events fire upfront so the UI inserts every
///   collapsed card before any sub-agent finishes; results land
///   in completion order but are pushed onto `messages` in the
///   model's original tool-call order so context stays
///   deterministic across replays.
/// - **Anything else** (mixed batch, single call, or zero `task`
///   calls): sequential dispatch. Sub-agent intercept still kicks
///   in for individual `task` calls in mixed batches.
async fn dispatch_tool_calls(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	cx: &ToolContext,
	cancel: &CancellationToken,
	calls: &[crate::inference::ToolCall],
) -> Result<(), CoderError> {
	let homogeneous_subagent = calls.len() >= 2 && calls.iter().all(|c| c.function.name == "task");
	if homogeneous_subagent {
		dispatch_subagent_batch(state, rt, sink, cx, cancel, calls).await
	} else {
		for call in calls {
			if cancel.is_cancelled() {
				return Err(CoderError::Aborted);
			}
			let args = parse_tool_args(&call.function);
			sink.send(CoderEvent::ToolCall {
				id: call.id.clone(),
				name: call.function.name.clone(),
				args: args.clone(),
			});
			let outcome = if call.function.name == "task" {
				handle_task(state, rt, sink, cx, cancel, &call.id, &args).await
			} else if call.function.name == "ask_user" {
				// Bidirectional: parks a oneshot on the session's
				// prompt registry and blocks the turn until the user
				// answers the card, sends a normal composer message
				// (skip), or aborts. The `tool_call` event already
				// fired above, so the panel rendered the prompt.
				handle_ask_user(rt, cancel, &call.id, &args).await
			} else if call.function.name == "todo_write" {
				// `todo_write` mutates per-session state owned by
				// the runner (`Session.todos`), so it doesn't fit
				// the stateless-tool shape `ToolRegistry::dispatch`
				// expects. Short-circuit here, alongside
				// `task`, before falling through to the
				// generic registry dispatch.
				handle_todo_write(rt, &args).await
			} else if call.function.name == "spawn_worker" {
				// Coordinator-only (ADR 0030). Mints a peer
				// top-level session in a worktree + seeds it with
				// the task. Returns a handle (session id), not a
				// blocking result — the worker runs detached.
				handle_spawn_worker(state, sink, &call.id, &args).await
			} else if call.function.name == "observe_worker" {
				handle_observe_worker(state, &args).await
			} else if call.function.name == "steer_worker" {
				handle_steer_worker(state, &args).await
			} else if call.function.name == "abort_worker" {
				handle_abort_worker(state, &args).await
			} else if call.function.name == "respond_to_worker_prompt" {
				handle_respond_to_worker_prompt(state, &args).await
			} else {
				state.tools.dispatch(&call.function.name, &args, cx, cancel).await
			};
			finish_tool_call(rt, sink, &call.id, &call.function.name, outcome).await?;
		}
		Ok(())
	}
}

/// Run N parallel sub-agents under a `Semaphore`, then drain
/// results in the order the model issued them so the conversation
/// history stays deterministic. Cancellation cascades automatically
/// via `cancel.child_token()` (the parent's token is the child's
/// parent).
async fn dispatch_subagent_batch(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	cx: &ToolContext,
	cancel: &CancellationToken,
	calls: &[crate::inference::ToolCall],
) -> Result<(), CoderError> {
	// Emit `ToolCall` events upfront so every collapsed card is
	// present in the parent's transcript before any sub-agent
	// starts streaming events of its own.
	let parsed_args: Vec<Value> = calls.iter().map(|c| parse_tool_args(&c.function)).collect();
	for (call, args) in calls.iter().zip(parsed_args.iter()) {
		sink.send(CoderEvent::ToolCall {
			id: call.id.clone(),
			name: call.function.name.clone(),
			args: args.clone(),
		});
	}

	let sem = Arc::new(Semaphore::new(SUBAGENT_PARALLELISM_CAP));
	let mut tasks = Vec::with_capacity(calls.len());
	for (call, args) in calls.iter().cloned().zip(parsed_args) {
		let state_for_task = state.clone();
		let rt_for_task = rt.clone();
		let sink_for_task = sink.clone();
		let cx_for_task = cx.clone();
		let cancel_for_task = cancel.clone();
		let sem_for_task = sem.clone();
		let call_id = call.id.clone();
		let task = tokio::spawn(async move {
			let _permit = sem_for_task.acquire().await.expect("semaphore not closed");
			handle_task(
				&state_for_task,
				&rt_for_task,
				&sink_for_task,
				&cx_for_task,
				&cancel_for_task,
				&call_id,
				&args,
			)
			.await
		});
		tasks.push((call, task));
	}
	for (call, task) in tasks {
		let outcome = match task.await {
			Ok(o) => o,
			Err(err) => Err(CoderError::Internal(format!(
				"sub-agent task join error for {}: {err}",
				call.id
			))),
		};
		finish_tool_call(rt, sink, &call.id, &call.function.name, outcome).await?;
	}
	Ok(())
}

/// Build + run a `Subagent` from the JSON args. Validation
/// errors surface back to the model as the tool's `is_error: true`
/// result so a confused call ("folder X not bound", "unknown
/// mode") is a recoverable signal, not a hard turn-failure.
async fn handle_task(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	cx: &ToolContext,
	cancel: &CancellationToken,
	tool_call_id: &str,
	args: &Value,
) -> Result<Value, CoderError> {
	let parent_session_id = rt.session.lock().await.header.id.clone();
	// Parent's bound folder is the sink's folder — that's the
	// session this dispatch belongs to. Sub-agent JSONL lands
	// under that slug regardless of which folder the sub-agent's
	// tools operate against (parent's project owns its sub-agents).
	let parent_folder = Utf8PathBuf::from(sink.folder());
	let bound = state.workspaces.folders().await;
	let spec = build_subagent_spec(
		parent_session_id,
		tool_call_id.to_string(),
		parent_folder,
		args,
		&cx.folder,
		&bound,
	)?;
	// Persist the spawn into the **parent**'s JSONL right away
	// (before the sub-agent runs) so a crash / kill mid-sub-agent
	// still leaves a record the parent can replay. The on-disk
	// record mirrors `CoderEvent::SubagentSpawned` byte-for-byte
	// so replay needs no shape conversion. Best-effort: a write
	// failure logs at warn but doesn't fail the spawn.
	persist_parent_record(
		rt,
		SessionRecord::SubagentSpawned {
			tool_call_id: tool_call_id.to_string(),
			subagent_id: spec.id.clone(),
			target_folder: spec.folder.folder.path.clone(),
			mode: spec.mode.as_wire().to_string(),
		},
	)
	.await;
	let subagent_id_for_record = spec.id.clone();
	let sub_cancel = cancel.child_token();
	// Sub-agents share their parent's `FolderEventSink` — events
	// arrive in the parent's folder bucket on the frontend, which
	// is exactly the multi-session contract: sub-agents belong to
	// whichever project originated them.
	let models_snapshot = state.models.read().await.clone();
	let outcome = run_subagent(
		&state.tools,
		&state.inference,
		sink,
		&state.coder_sessions_dir,
		&models_snapshot,
		spec,
		sub_cancel,
	)
	.await;
	// Persist the finish (success or error) into the parent's
	// JSONL. We piggy-back on the live `CoderEvent::SubagentFinished`
	// shape and add a `result_preview` so a reloaded parent can
	// render the collapsed card without lazy-loading the
	// sub-agent's own JSONL. For errors we record `was_error: true`
	// and a `None` preview — the parent's tool_result row already
	// surfaces the error JSON, no need to duplicate it.
	let finished_record = match &outcome {
		Ok(report) => SessionRecord::SubagentFinished {
			subagent_id: subagent_id_for_record.clone(),
			tokens_used_estimate: report.tokens_used_estimate,
			was_error: false,
			result_preview: result_preview_from(&report.result),
		},
		Err(_) => SessionRecord::SubagentFinished {
			subagent_id: subagent_id_for_record,
			tokens_used_estimate: 0,
			was_error: true,
			result_preview: None,
		},
	};
	persist_parent_record(rt, finished_record).await;
	let report = outcome?;
	Ok(json!({
		"result": report.result,
		"sub_session_id": report.sub_session_id,
		"tokens_used_estimate": report.tokens_used_estimate,
		"mode": report.mode.as_wire(),
		"iterations_used": report.iterations_used,
	}))
}

/// First non-empty trimmed line of `result`, capped at 512 chars,
/// for the [`SessionRecord::SubagentFinished::result_preview`] field.
/// We keep the full string instead of the panel's two-line cap so a
/// future "expanded preview" surface doesn't need a re-derivation
/// pass; `None` for empty results.
fn result_preview_from(result: &str) -> Option<String> {
	let trimmed = result.trim();
	if trimmed.is_empty() {
		return None;
	}
	if trimmed.len() <= 512 {
		return Some(trimmed.to_string());
	}
	Some(trimmed.chars().take(512).collect())
}

/// Append a record to the parent's session JSONL. Looks up the
/// session's `session_dir` + header under the lock; logs at warn
/// and proceeds on persistence errors (consistent with how the
/// rest of the runner treats best-effort writes).
async fn persist_parent_record(rt: &Arc<SessionRuntime>, record: SessionRecord) {
	let (session_dir, header) = {
		let session = rt.session.lock().await;
		(session.session_dir.clone(), session.header.clone())
	};
	let Some(dir) = session_dir else {
		// Empty / never-persisted parent session — skip rather
		// than seeding the file from the middle of a sub-agent
		// run; the very next user prompt path persists the
		// header + this record's siblings.
		return;
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(?err, "failed to persist subagent record on parent session");
	}
}

/// Apply a `todo_write` payload to the current session's todo
/// list, persist a snapshot, and return the canonical post-merge
/// list as the tool's result.
///
/// Lives on the runner side rather than in [`crate::tools`]
/// because the list is per-session state — see
/// [`crate::Session::todos`] — and the registry's
/// [`ToolRegistry::dispatch`] surface is intentionally stateless.
/// The short-circuit in [`dispatch_tool_calls`] routes here for
/// `name == "todo_write"`.
///
/// Validation is light: empty `id`s are rejected (they'd collapse
/// distinct items into one merge target), the rest is left to
/// [`crate::merge_todos`]. The model gets a structured
/// `CoderError::invalid_args` response when validation fails, so a
/// confused call surfaces as `is_error: true` in the next round
/// rather than corrupting the list silently.
///
/// Persistence failure is logged at warn but does **not** fail
/// the tool call: the in-memory list is the source of truth for
/// the running turn, and a JSONL write hiccup shouldn't make the
/// model retry a successful state mutation. This mirrors how
/// other persistence sites in the runner treat disk failures.
async fn handle_todo_write(rt: &Arc<SessionRuntime>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct TodoWriteArgs {
		todos: Vec<crate::TodoItem>,
		#[serde(default)]
		merge: bool,
	}
	let parsed: TodoWriteArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("todo_write", err.to_string()))?;
	for item in &parsed.todos {
		if item.id.trim().is_empty() {
			return Err(CoderError::invalid_args(
				"todo_write",
				"todo item `id` must be a non-empty string",
			));
		}
	}

	let mut session = rt.session.lock().await;
	let merged = crate::merge_todos(&session.todos, parsed.todos, parsed.merge);
	session.todos = merged.clone();
	let header = session.header.clone();
	let dir_opt = session.session_dir.clone();
	drop(session);

	if let Some(dir) = dir_opt {
		if let Err(err) =
			sessions::append_record(&dir, &header, &SessionRecord::TodosUpdate { todos: merged.clone() }).await
		{
			tracing::warn!("failed to persist todos update: {err}");
		}
	}
	Ok(json!({ "todos": merged }))
}

/// Handle an `ask_user` tool call: validate the questions, park a
/// oneshot on the session's [`crate::prompts::PromptRegistry`] keyed
/// by `tool_call_id`, then block until the human resolves it.
///
/// Three ways the wait ends:
///
/// - **Answered** — `coder_respond_to_prompt` fires the oneshot with
///   the user's structured per-question choices. The tool returns
///   `{ status: "answered", answers: [...] }` so the model sees
///   exactly what was picked (option ids + any custom free text).
/// - **Skipped** — the user ignored the card and sent a normal
///   composer message; `Coder::send` resolves the oneshot with
///   [`PromptOutcome::Skipped`]. The tool returns
///   `{ status: "skipped" }` and the typed message arrives as the
///   next user turn, so the model continues with the human's new
///   instruction instead of an answer.
/// - **Aborted** — the turn's cancel token trips (Esc / panel close
///   / sign-out). The tool returns [`CoderError::Aborted`], which
///   the loop turns into the usual interrupted-tool recovery.
///
/// Validation is light and mirrors `todo_write`: at least one
/// question, every question needs a non-empty id and at least two
/// options, every option needs a non-empty id. A malformed call
/// comes back as `is_error: true` so the model can fix it next turn
/// rather than parking a prompt the panel can't render.
async fn handle_ask_user(
	rt: &Arc<SessionRuntime>,
	cancel: &CancellationToken,
	tool_call_id: &str,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct AskUserArgs {
		questions: Vec<AskUserQuestion>,
	}
	#[derive(serde::Deserialize)]
	struct AskUserQuestion {
		id: String,
		#[allow(dead_code)]
		question: String,
		options: Vec<AskUserOption>,
	}
	#[derive(serde::Deserialize)]
	struct AskUserOption {
		id: String,
		#[allow(dead_code)]
		label: String,
	}
	let parsed: AskUserArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("ask_user", err.to_string()))?;
	if parsed.questions.is_empty() {
		return Err(CoderError::invalid_args("ask_user", "provide at least one question"));
	}
	for q in &parsed.questions {
		if q.id.trim().is_empty() {
			return Err(CoderError::invalid_args(
				"ask_user",
				"each question needs a non-empty `id`",
			));
		}
		if q.options.len() < 2 {
			return Err(CoderError::invalid_args(
				"ask_user",
				format!("question `{}` needs at least 2 options", q.id),
			));
		}
		for opt in &q.options {
			if opt.id.trim().is_empty() {
				return Err(CoderError::invalid_args(
					"ask_user",
					format!("an option in question `{}` has an empty `id`", q.id),
				));
			}
		}
	}

	let rx = rt.prompts.register(tool_call_id).await;
	let outcome = tokio::select! {
		biased;
		() = cancel.cancelled() => {
			// Clean up the parked sender so a late resolve can't fire
			// into a dropped receiver and confuse `has_pending`.
			rt.prompts.resolve(tool_call_id, PromptOutcome::Skipped).await;
			return Err(CoderError::Aborted);
		}
		res = rx => res,
	};
	match outcome {
		// Sender dropped without a value (shouldn't happen outside
		// teardown): treat as a skip so the model gets a clean
		// "no answer, keep going" rather than an error.
		Err(_) => Ok(json!({ "status": "skipped" })),
		Ok(PromptOutcome::Skipped) => Ok(json!({
			"status": "skipped",
			"note": "The user chose not to answer and is continuing with their own message — read their next instruction and proceed accordingly.",
		})),
		Ok(PromptOutcome::Answered(PromptResponse { answers })) => Ok(json!({
			"status": "answered",
			"answers": answers,
		})),
	}
}

// ── Coordinator tool handlers (ADR 0030) ─────────────────────
//
// These mint / observe / steer / abort / answer **peer top-level
// sessions** (workers), not sub-agents. They call the by-id client
// surface on `CoderHandle` (`create_worktree_session`, `send_to`,
// `abort_session`, `observe_session`, `respond_to_prompt`) — the
// same surface the companion app will use. The orchestrator is an
// in-process client of the coder surface.

/// `spawn_worker` — create a worker in a worktree + seed it with a
/// task. Returns a handle (session id) immediately; the worker runs
/// detached. The orchestrator's turn is not blocked.
async fn handle_spawn_worker(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	tool_call_id: &str,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct SpawnWorkerArgs {
		task: String,
		#[serde(default)]
		base_branch: Option<String>,
	}
	let parsed: SpawnWorkerArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("spawn_worker", err.to_string()))?;
	let task = parsed.task.trim();
	if task.is_empty() {
		return Err(CoderError::invalid_args("spawn_worker", "task must not be empty"));
	}
	// Mint the worker as an ordinary `Agent` session in a worktree.
	// A sub-orchestrator would pass `Coordinator` here, but that's a
	// later-scale concern; v1 workers are plain agents.
	let handle = CoderHandle { state: state.clone() };
	let (summary, _workspace) = handle
		.create_worktree_session(parsed.base_branch, CoderMode::Agent)
		.await?;
	let target_folder = summary.worktree_branch.as_deref().unwrap_or("").to_string();
	// Seed the worker with the task prompt.
	handle.send_to(&summary.id, task.to_string(), Vec::new()).await?;
	// Register the worker under the orchestrator's session id so the
	// events-as-messages feeder can filter the broadcast for it.
	let orchestrator_id = sink.session_id.clone();
	{
		let mut workers = state.coordinator_workers.write().await;
		let entry = workers.entry(orchestrator_id.clone()).or_default();
		entry.worker_ids.insert(summary.id.clone());
		if !entry.feeder_running {
			entry.feeder_running = true;
			spawn_dispatch_feeder(state.clone(), orchestrator_id.clone());
		}
	}
	sink.send(CoderEvent::SubagentSpawned {
		tool_call_id: tool_call_id.to_string(),
		subagent_id: summary.id.clone(),
		target_folder,
		mode: CoderMode::Agent.as_wire().to_string(),
	});
	Ok(json!({
		"worker_id": summary.id,
		"branch": summary.worktree_branch,
		"title": summary.title,
	}))
}

/// Background task that subscribes to the coder event broadcast,
/// filters for events from the orchestrator's workers, builds a
/// dispatch packet, and feeds it into the orchestrator's session as a
/// user message — waking the orchestrator's LLM loop (ADR 0030
/// §events-as-messages). Runs for the orchestrator's lifetime; exits
/// when the broadcast channel closes.
fn spawn_dispatch_feeder(state: Arc<CoderState>, orchestrator_id: String) {
	let handle = CoderHandle { state: state.clone() };
	let mut rx = handle.subscribe();
	tokio::spawn(async move {
		loop {
			let recv = rx.recv().await;
			let Ok(envelope) = recv else { continue };
			// Is this envelope from one of our workers?
			let is_our_worker = {
				let workers = state.coordinator_workers.read().await;
				workers
					.get(&orchestrator_id)
					.is_some_and(|w| w.worker_ids.contains(&envelope.session_id))
			};
			if !is_our_worker {
				continue;
			}
			let worker_id = envelope.session_id.clone();
			// Only forward events that warrant a wake — not every
			// streaming delta. `TurnComplete` (the worker finished
			// its turn) is the one the ADR names as the primary
			// wake signal; the orchestrator then calls
			// `observe_worker` for a snapshot.
			let packet = match &envelope.event {
				CoderEvent::TurnComplete => Some(format!(
					"Worker {worker_id} completed a turn. Use `observe_worker` to see its current state."
				)),
				_ => None,
			};
			if let Some(text) = packet {
				let _ = handle.send_to(&orchestrator_id, text, Vec::new()).await;
			}
		}
	});
}

/// `observe_worker` — fetch a compact snapshot of a worker's state.
async fn handle_observe_worker(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct ObserveArgs {
		worker_id: String,
	}
	let parsed: ObserveArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("observe_worker", err.to_string()))?;
	let handle = CoderHandle { state: state.clone() };
	let Some(snapshot) = handle.observe_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"observe_worker",
			format!("no mounted session for worker_id `{}`", parsed.worker_id),
		));
	};
	Ok(serde_json::to_value(&snapshot).unwrap_or_else(|_| json!({ "error": "serialization failed" })))
}

/// `steer_worker` — send a steering message to a worker by id.
async fn handle_steer_worker(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct SteerArgs {
		worker_id: String,
		text: String,
	}
	let parsed: SteerArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("steer_worker", err.to_string()))?;
	if parsed.text.trim().is_empty() {
		return Err(CoderError::invalid_args("steer_worker", "text must not be empty"));
	}
	let handle = CoderHandle { state: state.clone() };
	handle.send_to(&parsed.worker_id, parsed.text, Vec::new()).await?;
	Ok(json!({ "status": "steered" }))
}

/// `abort_worker` — cancel a worker's in-flight turn by id.
async fn handle_abort_worker(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct AbortArgs {
		worker_id: String,
	}
	let parsed: AbortArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("abort_worker", err.to_string()))?;
	let handle = CoderHandle { state: state.clone() };
	handle.abort_session(&parsed.worker_id).await;
	Ok(json!({ "status": "aborted" }))
}

/// `respond_to_worker_prompt` — answer a worker's parked `ask_user`.
/// Routes through the existing `respond_to_prompt` by-call-id scan
/// (which already targets any session, not just the visible one).
async fn handle_respond_to_worker_prompt(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct RespondArgs {
		worker_id: String,
		answers: Vec<QuestionAnswer>,
	}
	let parsed: RespondArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("respond_to_worker_prompt", err.to_string()))?;
	// Find the worker's parked prompt call id. A worker has at most
	// one pending `ask_user` at a time (the loop blocks on it).
	let handle = CoderHandle { state: state.clone() };
	let Some(snapshot) = handle.observe_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"respond_to_worker_prompt",
			format!("no mounted session for worker_id `{}`", parsed.worker_id),
		));
	};
	if !snapshot.needs_input {
		return Ok(json!({ "status": "no_pending_prompt", "note": "the worker has no parked ask_user" }));
	}
	// Find the call id by scanning the worker's prompt registry.
	let Some((rt, _)) = state.runtime_for_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"respond_to_worker_prompt",
			"worker runtime vanished between observe and respond",
		));
	};
	let call_id = rt
		.prompts
		.pending_call_id()
		.await
		.ok_or_else(|| CoderError::invalid_args("respond_to_worker_prompt", "no pending prompt call id"))?;
	let response = PromptResponse {
		answers: parsed.answers,
	};
	let resolved = handle.respond_to_prompt(&call_id, response).await;
	Ok(json!({ "status": if resolved { "answered" } else { "not_resolved" } }))
}

/// Walk the session's in-memory `messages` for assistant tool
/// calls that never got a matching `Tool` result. Used when a
/// turn ends in `Aborted` (Esc / panel close / sign-out) — the
/// assistant record already landed in `messages` and on disk
/// before the dispatcher was cancelled, so without recovery the
/// next turn would ship a malformed history to the provider
/// (Anthropic returns HTTP 400 "`tool_use` ids were found
/// without `tool_result` blocks"; OpenAI / others reject it
/// similarly). For each orphan we:
///
/// - Push a synthetic `ChatMessage::Tool` carrying the
///   `INTERRUPTED_TOOL_RESULT_JSON` sentinel onto `messages` so
///   the immediately-following user prompt has a valid
///   assistant→tool→user sequence.
/// - Append a matching `SessionRecord::Tool` to the JSONL so a
///   reload sees the same shape we just produced in memory
///   (reload-time orphan recovery in `open_session` then has
///   nothing left to synthesise — idempotent).
/// - Emit a `CoderEvent::ToolResult { is_error: true }` so the
///   panel flips the row from "running" to error and the
///   transcript matches what reload would render.
///
/// Order-preserving: orphans are appended to `messages` in the
/// order their tool_calls appear in the transcript, matching
/// `sessions::orphan_tool_call_ids`'s contract.
/// Mint a fresh `CancellationToken`, store it in `turn.cancel` (so
/// `busy` stays accurate and `abort()`/`drain_steer_now` can cancel
/// the new turn), and return the clone the spawn loop hands to the
/// next `run_turn`. Used on the loop-back paths after an abort-with-
/// steer or a straggler drain: the previous token is permanently
/// cancelled (`CancellationToken` is one-shot), so reusing it would
/// make the next `run_turn` bail at its iteration-top guard before
/// the steer ever drains.
async fn fresh_cancel(rt: &Arc<SessionRuntime>) -> CancellationToken {
	let cancel = CancellationToken::new();
	rt.turn.lock().await.cancel = Some(cancel.clone());
	cancel
}

async fn recover_in_memory_orphans(rt: &Arc<SessionRuntime>, sink: &FolderEventSink) {
	// Snapshot the orphan ids under the session lock, then drop
	// it before we hit the disk / event sink. Persistence
	// re-acquires the lock briefly per record, which is cheap.
	let orphans: Vec<(String, String)> = {
		let session = rt.session.lock().await;
		let mut completed: std::collections::HashSet<&str> = std::collections::HashSet::new();
		for msg in &session.messages {
			if let ChatMessage::Tool { tool_call_id, .. } = msg {
				completed.insert(tool_call_id.as_str());
			}
		}
		let mut orphans: Vec<(String, String)> = Vec::new();
		for msg in &session.messages {
			if let ChatMessage::Assistant { tool_calls, .. } = msg {
				for call in tool_calls {
					if !completed.contains(call.id.as_str()) {
						orphans.push((call.id.clone(), call.function.name.clone()));
					}
				}
			}
		}
		orphans
	};
	if orphans.is_empty() {
		return;
	}
	{
		let mut session = rt.session.lock().await;
		for (id, _) in &orphans {
			session.messages.push(ChatMessage::Tool {
				tool_call_id: id.clone(),
				content: sessions::INTERRUPTED_TOOL_RESULT_JSON.to_string(),
			});
		}
	}
	for (id, name) in &orphans {
		persist_tool_record(rt, id, name, sessions::INTERRUPTED_TOOL_RESULT_JSON).await;
		sink.send(CoderEvent::ToolResult {
			id: id.clone(),
			result: serde_json::json!({ "error": "Interrupted before tool completed." }),
			is_error: true,
		});
	}
}

/// Shared "tool finished, push result + emit events + persist"
/// epilogue used by both the sequential and the parallel paths.
async fn finish_tool_call(
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	tool_call_id: &str,
	tool_name: &str,
	outcome: Result<Value, CoderError>,
) -> Result<(), CoderError> {
	match outcome {
		Ok(value) => {
			let content = value.to_string();
			sink.send(CoderEvent::ToolResult {
				id: tool_call_id.to_string(),
				result: value,
				is_error: false,
			});
			rt.session.lock().await.messages.push(ChatMessage::Tool {
				tool_call_id: tool_call_id.to_string(),
				content: content.clone(),
			});
			persist_tool_record(rt, tool_call_id, tool_name, &content).await;
			Ok(())
		}
		Err(CoderError::Aborted) => Err(CoderError::Aborted),
		Err(err) => {
			let payload = json!({ "error": err.to_string() });
			let content = payload.to_string();
			sink.send(CoderEvent::ToolResult {
				id: tool_call_id.to_string(),
				result: payload,
				is_error: true,
			});
			rt.session.lock().await.messages.push(ChatMessage::Tool {
				tool_call_id: tool_call_id.to_string(),
				content: content.clone(),
			});
			persist_tool_record(rt, tool_call_id, tool_name, &content).await;
			Ok(())
		}
	}
}

/// Recompose the session's system prompt (`messages[0]`) from the
/// base prompt + a freshly-rendered "Bound folders" section.
/// Called at the top of every turn so newly-cached folder
/// summaries pick up without restarting the session.
///
/// The "active" marker in the rendered section tracks the
/// **session's** bound folder (`folder_path`), not the live
/// `WorkspaceRegistry::active_folder()`. With multi-session
/// running, the session running in folder X always marks X as
/// active in its own prompt regardless of which folder the user
/// is currently browsing — that's what keeps the model's
/// "your folder" reference stable across folder switches.
async fn refresh_system_prompt(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	folder_path: &Utf8Path,
	force_host_bash: bool,
	mode: CoderMode,
) {
	let folders = state.workspaces.folders().await;
	let container_mode = workspace_in_container_mode(&state.tools, force_host_bash).await;
	let prompt = compose_system_prompt(
		&folders,
		Some(folder_path.as_str()),
		&state.folder_summaries,
		container_mode,
		mode,
	)
	.await;
	let mut session = rt.session.lock().await;
	if let Some(ChatMessage::System { content }) = session.messages.first_mut() {
		*content = prompt;
	} else {
		session.messages.insert(0, ChatMessage::System { content: prompt });
	}
}

/// Probe whether the workspace's shell container is currently
/// running. Reuses the same `resolve_bash_target` plumbing the
/// `bash` tool dispatches against, so the system prompt's
/// "Bound folders" rendering can't drift from how `bash` actually
/// routes commands.
async fn workspace_in_container_mode(tools: &ToolRegistry, force_host_bash: bool) -> bool {
	tools.bash_target_is_container(force_host_bash).await
}

/// Schedule background regeneration for any bound folder whose
/// summary cache is missing or stale. Detached tasks; the runner
/// never waits on them. A summary that lands during a long turn
/// surfaces in the *next* turn's system prompt — `refresh_system_prompt`
/// runs on every iteration's top.
///
/// `FolderSummaryReady` events are tagged with the **target
/// folder's** path on the envelope (not the session's). The
/// frontend treats this kind of event as a global cache update
/// regardless of which folder bucket it arrives in.
async fn kick_off_summary_refresh(state: &Arc<CoderState>, _sink: &FolderEventSink) {
	let folders = state.workspaces.folders().await;
	let cheap_model = state.models.read().await.cheap().to_owned();
	for entry in folders {
		let folder_root = Utf8PathBuf::from(&entry.folder.path);
		if state.folder_summaries.cached(folder_root.as_path()).await.is_some() {
			continue;
		}
		state.folder_summaries.spawn_regenerate(
			folder_root,
			state.inference.clone(),
			cheap_model.clone(),
			state.events.clone(),
			CancellationToken::new(),
		);
	}
}

/// Build the parent's system prompt. Sections are concatenated in
/// this order:
///
/// 1. Base text — [`PHASE_6_0_SYSTEM_PROMPT`] for the ordinary
///    `Agent` mode, the coordinator system prompt for
///    `Coordinator` mode (ADR 0030). `Research` is a sub-agent-only
///    mode and never reaches this top-level composer; if it ever
///    does, it falls back to the agent prompt.
/// 2. **Project rules** — verbatim contents of `AGENTS.md` (or
///    `CLAUDE.md` as a fallback) from the *active* folder root.
///    Projects that came from the Claude / Anthropic ecosystem
///    name their agent-rules file `CLAUDE.md`; we treat that as
///    equivalent. Both are matched case-insensitively, capped at
///    [`AGENT_RULES_MAX_BYTES`], and truncated with a sentinel so
///    the model knows the file was clipped.
/// 3. **Bound folders** section, listing every bound folder with
///    its 2–3 sentence cached description. Skipped entirely when
///    no folder has a cached description yet — folders without
///    caches render as `(summary still generating)` once the
///    section is emitted.
///
/// All sections are byte-stable across turns when their inputs
/// haven't changed (project rules byte-stable until the user
/// edits the file; folder summaries byte-stable until the user
/// edits a manifest), so the inference router's prefix cache
/// keeps hitting on the system-prompt prefix.
async fn compose_system_prompt(
	folders: &[Arc<WorkspaceFolderEntry>],
	active_path: Option<&str>,
	summaries: &Arc<FolderSummaryService>,
	container_mode: bool,
	mode: CoderMode,
) -> String {
	let base = match mode {
		CoderMode::Coordinator => crate::coordinator::COORDINATOR_SYSTEM_PROMPT,
		// `Research` is sub-agent-only; a top-level research session
		// shouldn't exist, but fall back to the agent prompt rather
		// than panic a turn if one ever does.
		CoderMode::Agent | CoderMode::Research => PHASE_6_0_SYSTEM_PROMPT,
	};
	let mut out = String::with_capacity(base.len() + 1024);
	out.push_str(base);
	if !out.ends_with('\n') {
		out.push('\n');
	}

	if let Some(active) = active_path {
		if let Some(rules) = read_agent_rules(Utf8Path::new(active)).await {
			out.push('\n');
			out.push_str("## Project rules\n\n");
			out.push_str(
				"Verbatim contents of `AGENTS.md` (or `CLAUDE.md` as a fallback) from the active folder. Treat these as authoritative project conventions — they override anything in the base prompt above when the two disagree.\n\n",
			);
			out.push_str(&rules);
			if !out.ends_with('\n') {
				out.push('\n');
			}
		}
		// Personal coder instructions (gitignored, per-dev). Lives at
		// `<active>/.moon/AGENTS.md` — not the repo's committed `AGENTS.md`,
		// but a private addendum for overrides like "ignore TS crashes in
		// dev". Same shape, separate section so the model treats it as a
		// distinct layer (personal > team > base).
		if let Some(personal) = read_personal_instructions(Utf8Path::new(active)).await {
			out.push('\n');
			out.push_str("## Personal coder instructions\n\n");
			out.push_str(
				"Verbatim contents of `.moon/AGENTS.md` from the active folder — your personal, gitignored overrides. These are authoritative for this dev's workflow and override both the project rules above and the base prompt when the three disagree.\n\n",
			);
			out.push_str(&personal);
			if !out.ends_with('\n') {
				out.push('\n');
			}
		}
	}

	if folders.is_empty() {
		return out;
	}
	// Look up cached summaries up-front so the rendered section
	// never half-blocks on disk reads inside a `for` loop.
	let mut entries: Vec<(String, String, Option<String>, bool)> = Vec::with_capacity(folders.len());
	let mut any_cached = false;
	for folder in folders {
		let folder_path = folder.folder.path.clone();
		let folder_name = folder.folder.name.clone();
		let cached = summaries.cached(Utf8Path::new(&folder_path)).await;
		if cached.is_some() {
			any_cached = true;
		}
		let is_active = active_path == Some(folder_path.as_str());
		entries.push((folder_name, folder_path, cached.map(|s| s.description), is_active));
	}
	// Only emit the section when at least one folder has a real
	// description. A 1-folder workspace whose summary hasn't
	// landed yet doesn't benefit from a placeholder-only block —
	// the model already knows it has one folder via the active
	// context elsewhere.
	if !any_cached {
		return out;
	}
	out.push('\n');
	out.push_str("## Bound folders\n\n");
	if container_mode {
		out.push_str(
			"All folders currently bound to this workspace, listed with the `/workspace/<name>` paths the workspace shell container mounts them at. Your file-routing tools (`read_file`, `list_dir`, `write_file`, `edit_file`) accept these absolute paths to address any bound folder; `grep` and `bash` always run against the **active** folder, so for searches or commands in a non-active folder, use `task` with `folder: \"<name>\"`.\n\n",
		);
	} else {
		out.push_str(
			"All folders currently bound to this workspace, listed with their absolute host paths. Your file-routing tools (`read_file`, `list_dir`, `write_file`, `edit_file`) accept these absolute paths to address any bound folder; `grep` and `bash` always run against the **active** folder, so for searches or commands in a non-active folder, use `task` with `folder: \"<name>\"`.\n\n",
		);
	}
	for (name, path, description, is_active) in &entries {
		out.push_str("- `");
		if container_mode {
			out.push_str("/workspace/");
			out.push_str(name);
		} else {
			out.push_str(path);
		}
		out.push('`');
		if *is_active {
			out.push_str(" **(active — your tools operate here)**");
		} else {
			out.push_str(" — sibling, reach via `task`");
		}
		out.push_str(" · ");
		match description {
			Some(text) => out.push_str(text.trim()),
			None => out.push_str("(summary still generating)"),
		}
		out.push('\n');
	}
	out
}

/// Filenames we accept as "the active folder's project rules", in
/// preference order. AGENTS.md is the convention this repo uses
/// (and the one the broader agent ecosystem has been converging
/// on); CLAUDE.md is the Anthropic / Claude Code convention. We
/// take whichever exists, AGENTS.md winning when both are
/// present so a project that ships both has one canonical source.
///
/// Casing matches `folder_summary::CANONICAL_MANIFEST_NAMES` —
/// case-insensitive against the on-disk listing — so `agents.md`
/// / `CLAUDE.MD` / `Claude.md` all resolve.
const AGENT_RULES_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Cap on the agent-rules section. Larger files get truncated
/// with a `... (truncated)` sentinel so the model can still draw
/// signal from the prefix. 20 KB lines up with the most-favoured
/// agent-rules size we've seen in practice (low-thousand-word
/// AGENTS.md files) without bloating the system prompt for repos
/// that ship a sprawling 100 KB file.
const AGENT_RULES_MAX_BYTES: usize = 20_000;

/// Read `AGENTS.md` (or `CLAUDE.md` as a fallback) from
/// `folder_root`. Case-insensitive against the top-level listing.
/// Returns `None` when neither file exists, the read fails, or the
/// file is empty after trimming.
///
/// Walking up parent dirs (a la `.editorconfig` / `git`) is
/// deliberately deferred — most users keep their agent rules at
/// the project root, and the spec note in [`specs/coder.md`] §
/// "What the LLM sees as system prompt" calls for parent walk in
/// 6.6. Today's behaviour is "active folder root only" until
/// somebody actually has a multi-level AGENTS.md hierarchy that
/// matters.
async fn read_agent_rules(folder_root: &Utf8Path) -> Option<String> {
	let mut by_lower: HashMap<String, std::path::PathBuf> = HashMap::new();
	if let Ok(mut iter) = tokio::fs::read_dir(folder_root.as_std_path()).await {
		while let Ok(Some(entry)) = iter.next_entry().await {
			let file_name = entry.file_name();
			let Some(name_str) = file_name.to_str() else {
				continue;
			};
			by_lower.insert(name_str.to_lowercase(), entry.path());
		}
	}
	for canonical in AGENT_RULES_NAMES {
		let Some(path) = by_lower.get(&canonical.to_lowercase()) else {
			continue;
		};
		let bytes = tokio::fs::read(path).await.ok()?;
		if bytes.is_empty() {
			continue;
		}
		let truncated = bytes.len() > AGENT_RULES_MAX_BYTES;
		let slice = if truncated {
			&bytes[..AGENT_RULES_MAX_BYTES]
		} else {
			&bytes[..]
		};
		// Lossy is fine — agent-rules files are human-edited Markdown;
		// any bad bytes are an authoring bug and the model can cope.
		let mut text = String::from_utf8_lossy(slice).into_owned();
		if text.trim().is_empty() {
			continue;
		}
		if truncated {
			if !text.ends_with('\n') {
				text.push('\n');
			}
			text.push_str("\n... (truncated)\n");
		}
		return Some(text);
	}
	None
}

/// Read `.moon/AGENTS.md` from `folder_root` — a gitignored,
/// per-developer addendum to the repo's committed `AGENTS.md`. Lets
/// a dev encode personal coder overrides ("ignore TS crashes in dev",
/// "always use `just` not `cargo`") without polluting the team's
/// committed rules. Same byte-cap / truncation as [`read_agent_rules`].
async fn read_personal_instructions(folder_root: &Utf8Path) -> Option<String> {
	let path = folder_root.join(".moon").join("AGENTS.md");
	let bytes = tokio::fs::read(path.as_std_path()).await.ok()?;
	let truncated = bytes.len() > AGENT_RULES_MAX_BYTES;
	let slice = if truncated {
		&bytes[..AGENT_RULES_MAX_BYTES]
	} else {
		&bytes[..]
	};
	let mut text = String::from_utf8_lossy(slice).into_owned();
	if text.trim().is_empty() {
		return None;
	}
	if truncated {
		if !text.ends_with('\n') {
			text.push('\n');
		}
		text.push_str("\n... (truncated)\n");
	}
	Some(text)
}

/// Drain `pending_steers` into `session.messages` and persist
/// each as a [`SessionRecord::User`]. Called at the top of every
/// `run_turn` iteration so steers reach the model on the next
/// LLM round-trip. The session lock is held while we lift the
/// queue and append, then dropped before the (slow) JSONL write
/// so a steer arriving mid-write doesn't block on us; an aborted
/// turn that never gets to drain leaves the queue intact for
/// garbage collection when the session itself is replaced
/// (`load_session`, `clear_session`).
async fn drain_pending_steers(rt: &Arc<SessionRuntime>, sink: &FolderEventSink) {
	let (steers, dir, header) = {
		let mut session = rt.session.lock().await;
		if session.pending_steers.is_empty() {
			return;
		}
		let drained: Vec<PendingSteer> = std::mem::take(&mut session.pending_steers);
		for steer in &drained {
			session.messages.push(ChatMessage::User {
				content: steer.text.clone(),
				images: steer.images.clone(),
			});
		}
		session.header.updated_at_ms = current_time_ms();
		let dir = session.session_dir.clone();
		let header = session.header.clone();
		(drained, dir, header)
	};
	// Tell the panel the queued rows just graduated — chip-strip
	// "unqueue" disappears, and the row's muted styling clears.
	// We emit before persistence so the UI flip is immediate
	// regardless of disk latency.
	for steer in &steers {
		sink.send(CoderEvent::SteerDrained { id: steer.id.clone() });
	}
	let Some(dir) = dir else {
		return;
	};
	for steer in steers {
		let record = SessionRecord::User {
			text: steer.text,
			images: steer.images,
		};
		if let Err(err) = sessions::append_record(&dir, &header, &record).await {
			tracing::warn!(error = %err, "failed to persist steered user message");
			continue;
		}
		let mut session = rt.session.lock().await;
		session.persisted_records = session.persisted_records.saturating_add(1);
	}
}

/// Append an `Assistant` record to the JSONL of the given
/// folder's session. Best-effort: a write failure logs but
/// doesn't fail the turn. `pi_model` is the `provider/model`
/// slug that actually served the round-trip — stamped on the
/// persisted record so the pi-mono trace viewer renders the
/// real route per turn, not the session header's seed.
async fn persist_assistant_record(rt: &Arc<SessionRuntime>, response: &AssistantResponse, pi_model: Option<String>) {
	let (dir, header) = {
		let session = rt.session.lock().await;
		let Some(dir) = session.session_dir.clone() else {
			return;
		};
		(dir, session.header.clone())
	};
	let record = SessionRecord::Assistant {
		content: response.content.clone(),
		thinking: response.thinking.clone(),
		thinking_blocks: response.thinking_blocks.clone(),
		tool_calls: response.tool_calls.clone(),
		model: pi_model,
		stop_reason: response.stop_reason.clone(),
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(error = %err, "failed to persist assistant message");
		return;
	}
	let mut session = rt.session.lock().await;
	session.persisted_records = session.persisted_records.saturating_add(1);
}

/// Append a [`SessionRecord::Usage`] when the round-trip that
/// just finished carried provider-supplied figures. We skip the
/// bytes/4 estimate path on purpose — those numbers are
/// recomputable from the persisted messages, so persisting them
/// would just bloat the JSONL with redundant approximations.
/// Best-effort: a write failure logs but doesn't fail the turn,
/// same posture as the assistant / tool persisters above.
///
/// `persisted_records` deliberately *isn't* incremented here.
/// That counter feeds the auto-rename "is this session worth
/// renaming yet?" check, which keys off real conversational
/// records (user / assistant / tool); a metadata sidecar like
/// `Usage` shouldn't move it.
async fn persist_usage_record(rt: &Arc<SessionRuntime>, response: &AssistantResponse) {
	let Some(usage) = response.usage else {
		return;
	};
	let (dir, header) = {
		let session = rt.session.lock().await;
		let Some(dir) = session.session_dir.clone() else {
			return;
		};
		(dir, session.header.clone())
	};
	let record = SessionRecord::Usage {
		prompt_tokens: usage.prompt_tokens,
		completion_tokens: usage.completion_tokens,
		total_tokens: usage.total_tokens,
		cache_read_input_tokens: usage.cache_read_input_tokens,
		cache_creation_input_tokens: usage.cache_creation_input_tokens,
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(error = %err, "failed to persist usage record");
	}
}

async fn persist_tool_record(rt: &Arc<SessionRuntime>, tool_call_id: &str, tool_name: &str, content: &str) {
	let (dir, header) = {
		let session = rt.session.lock().await;
		let Some(dir) = session.session_dir.clone() else {
			return;
		};
		(dir, session.header.clone())
	};
	let record = SessionRecord::Tool {
		tool_call_id: tool_call_id.to_string(),
		tool_name: tool_name.to_string(),
		content: content.to_string(),
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(error = %err, "failed to persist tool result");
		return;
	}
	let mut session = rt.session.lock().await;
	session.persisted_records = session.persisted_records.saturating_add(1);
}

/// Append a [`SessionRecord::Error`] when a turn fails with a
/// non-recoverable backend error (auth, decode, provider 400, etc.).
/// Without this the on-disk transcript ends at the last successful
/// record and the failure is invisible to anyone debugging from the
/// JSONL after the fact — the UI toast already vanished by then.
///
/// Best-effort, same posture as the other persisters: a write
/// failure logs but doesn't escalate the already-failing turn.
/// `persisted_records` is **not** incremented — an error isn't a
/// conversational record, so it shouldn't push the auto-rename
/// "worth naming yet?" check (same rationale as `persist_usage_record`).
async fn persist_error_record(rt: &Arc<SessionRuntime>, message: &str) {
	let (dir, header) = {
		let session = rt.session.lock().await;
		let Some(dir) = session.session_dir.clone() else {
			return;
		};
		(dir, session.header.clone())
	};
	let record = SessionRecord::Error {
		message: message.to_string(),
	};
	if let Err(err) = sessions::append_record(&dir, &header, &record).await {
		tracing::warn!(error = %err, "failed to persist error record");
	}
}

/// Spawn the post-first-turn auto-rename pass. Calls the fast
/// model with a tight prompt asking for a 4-6 word title, then
/// persists the result via a `TitleUpdate` record + a
/// `SessionTitleUpdated` event. Failures are logged at info level
/// — the truncated-prompt title is a perfectly serviceable
/// fallback.
///
/// Tied to a specific `FolderSession` so the rename only applies
/// to the session that just finished its first turn — other
/// folders' sessions stay untouched.
fn spawn_auto_rename(state: Arc<CoderState>, rt: Arc<SessionRuntime>, sink: FolderEventSink) {
	tokio::spawn(async move {
		// Snapshot the chat history without holding the session
		// lock across the LLM call — turns / aborts must be able
		// to grab it freely while we wait on the network. The
		// `auto_rename_pending` flag was already cleared at the
		// caller's send-time critical section so a second send
		// can't double-spawn us.
		let (dir, header_snapshot, transcript) = {
			let session = rt.session.lock().await;
			let Some(dir) = session.session_dir.clone() else {
				return;
			};
			(dir, session.header.clone(), summarise_transcript(&session.messages))
		};
		if transcript.is_empty() {
			return;
		}
		tracing::debug!(session = %header_snapshot.id, "auto-rename: requesting title from cheap model");
		let messages = vec![
			ChatMessage::System {
				content: AUTO_RENAME_SYSTEM_PROMPT.to_string(),
			},
			ChatMessage::user(transcript),
		];
		let cheap_model = state.models.read().await.cheap().to_owned();
		let cancel = CancellationToken::new();
		let response = match state
			.inference
			.chat_completion(&cheap_model, &messages, &[], &cancel)
			.await
		{
			Ok(resp) => resp,
			Err(err) => {
				tracing::info!(error = %err, "auto-rename: cheap-model call failed; keeping fallback title");
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
		let mut session = rt.session.lock().await;
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
		sink.send(CoderEvent::SessionTitleUpdated {
			id: header_for_disk.id,
			title: new_title,
		});
		sink.send(CoderEvent::SessionListChanged);
	});
}

/// One-shot system prompt for the auto-rename pass. Kept tight on
/// purpose — we want a flat string, not a paragraph of preamble.
const AUTO_RENAME_SYSTEM_PROMPT: &str = "You are a title generator. Given a short transcript of one turn between a user and a coding assistant, return a 4 to 6 word title for the conversation. Output the title only, with no quotes, no period, no markdown, and no preamble.";

/// One-shot system prompt for branch-name suggestion. Same
/// minimal-preamble shape as the title generator: we want a
/// kebab-cased identifier, not a sentence.
/// Situational context for [`CoderRunner::suggest_terminal_command`].
/// Gathered by the Tauri command from the terminal's target and
/// the active folder's git state so the model can resolve "the
/// other branch" / relative paths without a tool round-trip.
#[derive(Debug, Default, Clone)]
pub struct TerminalCommandContext {
	/// `"host"` or `"container"` — picked up from the terminal's
	/// `TerminalTarget`. Container shells are Debian (moon-base);
	/// the host can be anything.
	pub shell_kind: String,
	/// Working directory the terminal was opened in. Empty when
	/// unknown (e.g. a host shell with no cwd override).
	pub cwd: String,
	/// Current branch of the active folder, if it's a git repo.
	/// Empty otherwise. Lets "rebase onto main" vs "merge the
	/// current branch" land on the right ref.
	pub git_branch: String,
	/// Newline-joined list of the folder's local branches (capped
	/// upstream) so "cherry-pick from feat-x" can match a real
	/// branch name even when the user abbreviates it. Empty when
	/// not a git repo.
	pub git_branches: String,
}

const BRANCH_NAME_SYSTEM_PROMPT: &str = "You suggest git branch names. Given a draft commit message and/or a `git diff --stat` summary, return ONE short branch name in kebab-case (2 to 5 words, lowercase, hyphen-separated, no slashes, no quotes, no leading prefix like `feature/` or `fix/`). Output the name only, no explanation.";

/// One-shot system prompt for commit-message suggestion. Asks
/// for a single subject line (no body, no markdown, no quotes)
/// because that's what fits the textarea and is what the team's
/// commit history actually uses; the user can flesh out a body
/// manually after the prefill if they want one.
const COMMIT_MESSAGE_SYSTEM_PROMPT: &str = "You suggest git commit messages. Given a working-tree diff (and optionally a draft message the user has started typing), return ONE concise subject line (5 to 10 words, imperative mood, no period, no quotes, no markdown, no `feat:` / `fix:` prefix unless the project's existing history obviously uses them). Output the subject only, no body, no explanation.";

/// Build the user-side prompt for the branch-name pass. We always
/// send both fields with explicit headings so a blank one is
/// obviously a non-signal rather than a missing argument the
/// model needs to fill in.
fn build_branch_name_prompt(commit_message: &str, diff_summary: &str) -> String {
	let message = commit_message.trim();
	let diff = diff_summary.trim();
	let mut out = String::new();
	out.push_str("Commit message:\n");
	if message.is_empty() {
		out.push_str("(none)");
	} else {
		out.push_str(message);
	}
	out.push_str("\n\nDiff summary (`git diff HEAD --stat`):\n");
	if diff.is_empty() {
		out.push_str("(none)");
	} else {
		out.push_str(diff);
	}
	out
}

/// User-side prompt for the commit-message pass. We always ship
/// both fields with explicit headings so a blank one is obviously
/// "no signal here, infer from the other" rather than a missing
/// argument the model needs to guess at.
fn build_commit_message_prompt(existing_message: &str, diff_patch: &str) -> String {
	let message = existing_message.trim();
	let diff = diff_patch.trim();
	let mut out = String::new();
	out.push_str("Draft commit message (may be empty):\n");
	if message.is_empty() {
		out.push_str("(none)");
	} else {
		out.push_str(message);
	}
	out.push_str("\n\nWorking-tree diff (`git diff HEAD`):\n");
	if diff.is_empty() {
		out.push_str("(none)");
	} else {
		out.push_str(diff);
	}
	out
}

/// Trim a model-emitted commit subject down to a single clean
/// line. The fast model usually behaves but sometimes wraps in
/// backticks / quotes, prefixes with "Subject:" / "Commit:", or
/// appends a body separated by a blank line — keep the first
/// non-empty line, strip wrapper punctuation, drop common labels,
/// drop a trailing period (commit subjects don't end with one),
/// and cap length so a runaway response can't blow out the
/// composer.
pub(crate) fn sanitise_commit_message(raw: &str) -> String {
	const MAX_CHARS: usize = 100;

	let trimmed = raw.trim();
	let first_line = trimmed.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
	let mut s = first_line.trim().to_string();

	for prefix in ["subject:", "commit message:", "commit:", "message:", "title:"] {
		if let Some(rest) = strip_prefix_ignore_ascii_case(&s, prefix) {
			s = rest.trim().to_string();
		}
	}

	s = s.trim_matches(|c: char| c == '"' || c == '\'' || c == '`').to_string();
	while s.ends_with('.') || s.ends_with(' ') {
		s.pop();
	}

	if s.chars().count() <= MAX_CHARS {
		return s;
	}
	let mut clipped: String = s.chars().take(MAX_CHARS).collect();
	while clipped.ends_with(' ') || clipped.ends_with('.') {
		clipped.pop();
	}
	clipped
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
	if s.len() < prefix.len() {
		return None;
	}
	let head = &s[..prefix.len()];
	if head.eq_ignore_ascii_case(prefix) {
		Some(&s[prefix.len()..])
	} else {
		None
	}
}

/// Coerce a model-emitted branch suggestion into something git
/// will accept. The fast model is usually well-behaved, but it
/// occasionally tacks on quotes, a `feature/` prefix, or a
/// trailing period — strip those, lowercase, replace internal
/// whitespace + underscore with `-`, drop any character outside
/// `[a-z0-9.-]`, collapse runs of `-`, trim leading/trailing
/// `-`, and cap length. The remaining string passes
/// `git check-ref-format --branch` for everything we've seen
/// from the model so far.
pub(crate) fn sanitise_branch_name(raw: &str) -> String {
	const MAX_CHARS: usize = 60;
	let trimmed = raw.trim();
	let trimmed = trimmed.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*' || c == '.');
	// Take the first line — the model occasionally appends a
	// follow-up sentence we don't want.
	let first_line = trimmed.lines().next().unwrap_or("");
	let lower = first_line.to_lowercase();
	let mut out = String::with_capacity(lower.len());
	let mut last_dash = false;
	for ch in lower.chars() {
		let mapped = if ch.is_ascii_alphanumeric() || ch == '.' {
			Some(ch)
		} else if ch == '-' || ch == '_' || ch == ' ' || ch == '/' || ch == '\t' {
			Some('-')
		} else {
			None
		};
		match mapped {
			Some('-') if !last_dash && !out.is_empty() => {
				out.push('-');
				last_dash = true;
			}
			Some('-') => {}
			Some(c) => {
				out.push(c);
				last_dash = false;
			}
			None => {}
		}
	}
	let trimmed = out.trim_matches('-').trim_matches('.').to_owned();
	if trimmed.chars().count() <= MAX_CHARS {
		return trimmed;
	}
	let mut clipped: String = trimmed.chars().take(MAX_CHARS).collect();
	while clipped.ends_with('-') || clipped.ends_with('.') {
		clipped.pop();
	}
	clipped
}

/// One-shot system prompt for the terminal `Ctrl+K` command
/// suggester. The single hard rule is "output exactly one shell
/// command, nothing else" — the result is prefilled straight into
/// the PTY line, so any prose, fences, or explanation would land
/// in the user's command line as garbage. We tell it to favour
/// the current shell/cwd context and to leave the command unrun
/// (the user presses Enter), which keeps it from emitting things
/// like `&& echo done` victory laps.
const TERMINAL_COMMAND_SYSTEM_PROMPT: &str = "You translate a natural-language request into ONE shell command to be prefilled into a terminal prompt. Output ONLY the command, on a single line, with no explanation, no markdown, no code fences, no leading `$`, and no surrounding quotes. Use the provided shell kind, working directory, and git branch context to resolve ambiguous references (branch names, relative paths). If the request genuinely needs multiple steps, chain them with `&&` on one line. Do not invent flags or files you have no basis for. The user reviews the command and presses Enter, so never append confirmation echoes.";

/// User-side prompt for the terminal-command pass. Ships the
/// request plus whatever situational context we could gather,
/// each under an explicit heading so a blank field reads as
/// "no signal" rather than a missing argument.
fn build_terminal_command_prompt(request: &str, ctx: &TerminalCommandContext) -> String {
	let mut out = String::new();
	out.push_str("Request:\n");
	out.push_str(request.trim());
	out.push_str("\n\nShell kind: ");
	out.push_str(if ctx.shell_kind.is_empty() {
		"(unknown)"
	} else {
		ctx.shell_kind.as_str()
	});
	out.push_str("\nWorking directory: ");
	out.push_str(if ctx.cwd.is_empty() {
		"(unknown)"
	} else {
		ctx.cwd.as_str()
	});
	out.push_str("\nCurrent git branch: ");
	out.push_str(if ctx.git_branch.is_empty() {
		"(not a git repo)"
	} else {
		ctx.git_branch.as_str()
	});
	if !ctx.git_branches.trim().is_empty() {
		out.push_str("\nLocal branches:\n");
		out.push_str(ctx.git_branches.trim());
	}
	out
}

/// Coerce a model-emitted command into a single clean line safe
/// to prefill into a PTY. The model usually behaves, but it
/// occasionally wraps the command in a ```` ```bash ```` fence,
/// prefixes a `$ ` prompt, or appends an explanation on a second
/// line — keep the first non-empty, non-fence line, strip a
/// leading prompt marker, and drop a single layer of surrounding
/// backticks. We deliberately do NOT strip shell quotes: they're
/// meaningful inside a command.
pub(crate) fn sanitise_terminal_command(raw: &str) -> String {
	let mut command: Option<String> = None;
	for line in raw.lines() {
		let trimmed = line.trim();
		if trimmed.is_empty() || trimmed.starts_with("```") {
			continue;
		}
		command = Some(trimmed.to_string());
		break;
	}
	let Some(mut s) = command else {
		return String::new();
	};
	for prefix in ["$ ", "$", "# ", "> "] {
		if let Some(rest) = s.strip_prefix(prefix) {
			s = rest.trim_start().to_string();
			break;
		}
	}
	if s.len() >= 2 && s.starts_with('`') && s.ends_with('`') {
		s = s[1..s.len() - 1].trim().to_string();
	}
	s.trim().to_string()
}

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
			ChatMessage::User { content, .. } => {
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
/// Translate one persisted record into the replay events the
/// panel's reducer expects, **pushing into `out`** rather than
/// emitting one-per-event. `open_session` collects the whole
/// transcript into a single `Vec` and ships it as one
/// [`CoderEvent::Replay`], so the frontend pays one IPC crossing
/// instead of one-per-record.
fn emit_replay_events(out: &mut Vec<CoderEvent>, record: SessionRecord, created_at_ms: i64) {
	match record {
		SessionRecord::User { text, images } => {
			out.push(CoderEvent::UserMessage {
				id: new_message_id(),
				text,
				images,
				queued: false,
				created_at_ms: Some(created_at_ms),
			});
		}
		SessionRecord::Assistant {
			content,
			thinking,
			thinking_blocks: _,
			tool_calls,
			model: _,
			stop_reason: _,
		} => {
			let id = new_message_id();
			let has_text = content.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
			let has_thinking = thinking.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
			if has_text || has_thinking {
				out.push(CoderEvent::AssistantMessageStart { id: id.clone() });
				out.push(CoderEvent::AssistantMessageEnd {
					id,
					text: content.unwrap_or_default(),
					thinking: thinking.filter(|t| !t.is_empty()),
					created_at_ms: Some(created_at_ms),
				});
			}
			for call in tool_calls {
				let args = parse_tool_args(&call.function);
				out.push(CoderEvent::ToolCall {
					id: call.id.clone(),
					name: call.function.name,
					args,
				});
			}
		}
		SessionRecord::Tool {
			tool_call_id,
			tool_name: _,
			content,
		} => {
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
			out.push(CoderEvent::ToolResult {
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
		SessionRecord::Usage { .. } => {
			// Per-round-trip usage figures are metadata: the
			// panel cares about the *latest* number for its
			// context-usage ring, not the historical sequence.
			// `open_session` walks the records, picks the last
			// `Usage`, and emits a single `TokenUsage` event for
			// it after the replay loop — replaying every record
			// would just animate the ring through old states.
		}
		SessionRecord::TodosUpdate { .. } => {
			// Same rationale as `Usage`: the panel only needs
			// the last list. Each `todo_write` call replays via
			// the surrounding `Assistant` (tool_call) +
			// subsequent `Tool` (tool_result) pair, and the
			// frontend mirrors `tool_result.todos` into its
			// `coder.todos` bucket — no need for a synthetic
			// `TodosUpdate` event during replay.
		}
		SessionRecord::SubagentSpawned { .. } | SessionRecord::SubagentFinished { .. } => {
			// Sub-agent records are replayed by `open_session` in
			// a dedicated async pass that also pulls in the
			// sub-agent's own JSONL — see [`replay_subagent`]. We
			// can't do that here because [`emit_replay_events`]
			// is sync; this arm exists to keep the match
			// exhaustive.
		}
		SessionRecord::Error { message } => {
			// Re-emit the terminal turn error so the reopened
			// transcript shows the failure inline — the user
			// remembers "it errored", and the JSONL now backs that
			// up instead of trailing off mid-tool-loop.
			out.push(CoderEvent::Error { message });
		}
		SessionRecord::Compaction {
			summary,
			messages_compacted,
			..
		} => {
			// Compaction shapes the in-memory `messages` slice at
			// replay time (see [`load_session`]). We also re-emit
			// the `started` + `complete` event pair so the panel
			// rebuilds the inline compaction disclosure at the
			// point in the transcript where the fold happened —
			// the summary the agent is actually running on stays
			// visible after a reopen, instead of vanishing. The
			// `complete` lands collapsed (the frontend's `<details>`
			// defaults closed), so reopening doesn't pop it open.
			out.push(CoderEvent::CompactionStarted { messages_compacted });
			out.push(CoderEvent::CompactionComplete {
				summary,
				// Replay can't recover the exact post-fold token
				// count, and the ring is re-anchored by the next
				// live round-trip's estimate anyway. 0 keeps the
				// disclosure honest without faking a number.
				prompt_tokens_after: 0,
			});
		}
	}
}

/// Replay one persisted [`SessionRecord::SubagentSpawned`] record:
/// emit the `SubagentSpawned` event so the parent's panel rebuilds
/// the collapsed card, then read the sub-agent's own JSONL (if it
/// exists) and re-emit each of its records as `SubagentEvent`s so
/// the popped-out transcript matches what the user originally saw.
///
/// Sub-agent JSONLs sit at
/// `<parent_sessions_dir>/<parent_session_id>/<subagent_id>.jsonl`
/// — we don't recompute the path; we just probe it and skip
/// gracefully if it's missing (manual deletion, partial write,
/// older session that pre-dated subagent persistence).
async fn replay_subagent_spawned(
	out: &mut Vec<CoderEvent>,
	parent_sessions_dir: &Utf8Path,
	parent_session_id: &str,
	tool_call_id: String,
	subagent_id: String,
	target_folder: String,
	mode: String,
) {
	out.push(CoderEvent::SubagentSpawned {
		tool_call_id,
		subagent_id: subagent_id.clone(),
		target_folder,
		mode,
	});

	let sub_dir = subagent_session_dir(parent_sessions_dir, parent_session_id);
	let loaded = match sessions::load(&sub_dir, &subagent_id).await {
		Ok(loaded) => loaded,
		Err(err) => {
			tracing::warn!(?err, %subagent_id, "skipping sub-agent transcript replay (load failed)");
			return;
		}
	};
	let orphan_tool_call_ids = sessions::orphan_tool_call_ids(&loaded.records);
	for (record, record_ts) in loaded.records.into_iter().zip(loaded.record_timestamps) {
		// Wrap each replayed event into a `SubagentEvent` so the
		// frontend routes by `subagent_id` into the per-sub-agent
		// transcript bucket. Skip records that have no
		// transcript-shape (Usage, TodosUpdate, Compaction,
		// nested Subagent*) — those only matter for live
		// runtime / context reconstruction, not for the popped-
		// out transcript.
		let inners = subagent_replay_inners(record, record_ts);
		for inner in inners {
			out.push(CoderEvent::SubagentEvent {
				subagent_id: subagent_id.clone(),
				inner: Box::new(inner),
			});
		}
	}
	// Same orphan-recovery as the top-level path: a sub-agent
	// killed mid-tool leaves its last `tool_call` without a
	// `tool_result`, which the panel renders as a forever-
	// running row. Synthesise the matching error result so the
	// popped-out transcript settles into a clean done state.
	for orphan_id in orphan_tool_call_ids {
		out.push(CoderEvent::SubagentEvent {
			subagent_id: subagent_id.clone(),
			inner: Box::new(CoderEvent::ToolResult {
				id: orphan_id,
				result: serde_json::json!({ "error": "Interrupted before tool completed." }),
				is_error: true,
			}),
		});
	}
}

/// Translate one sub-agent persisted record into the
/// `CoderEvent`s the parent's panel feeds through
/// `applyInnerEventToRows`. Returns an empty Vec for records that
/// don't shape the transcript (Usage / TodosUpdate / Compaction /
/// nested SubagentSpawned/Finished) — they'd be ignored by the
/// frontend reducer anyway, but skipping them here keeps the IPC
/// chatter down on a long-running sub-agent.
fn subagent_replay_inners(record: SessionRecord, created_at_ms: i64) -> Vec<CoderEvent> {
	match record {
		SessionRecord::User { text, images } => vec![CoderEvent::UserMessage {
			id: new_message_id(),
			text,
			images,
			queued: false,
			created_at_ms: Some(created_at_ms),
		}],
		SessionRecord::Assistant {
			content,
			thinking,
			thinking_blocks: _,
			tool_calls,
			model: _,
			stop_reason: _,
		} => {
			let mut out = Vec::new();
			let id = new_message_id();
			let has_text = content.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
			let has_thinking = thinking.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
			if has_text || has_thinking {
				out.push(CoderEvent::AssistantMessageStart { id: id.clone() });
				out.push(CoderEvent::AssistantMessageEnd {
					id,
					text: content.unwrap_or_default(),
					thinking: thinking.filter(|t| !t.is_empty()),
					created_at_ms: Some(created_at_ms),
				});
			}
			for call in tool_calls {
				let args = parse_tool_args(&call.function);
				out.push(CoderEvent::ToolCall {
					id: call.id.clone(),
					name: call.function.name,
					args,
				});
			}
			out
		}
		SessionRecord::Tool {
			tool_call_id,
			tool_name: _,
			content,
		} => {
			let result = match serde_json::from_str::<Value>(&content) {
				Ok(value) => value,
				Err(_) => Value::String(content),
			};
			let is_error = matches!(&result, Value::Object(map) if map.contains_key("error") && map.len() == 1);
			vec![CoderEvent::ToolResult {
				id: tool_call_id,
				result,
				is_error,
			}]
		}
		SessionRecord::Error { message } => vec![CoderEvent::Error { message }],
		SessionRecord::TitleUpdate { .. }
		| SessionRecord::Usage { .. }
		| SessionRecord::TodosUpdate { .. }
		| SessionRecord::Compaction { .. }
		| SessionRecord::SubagentSpawned { .. }
		| SessionRecord::SubagentFinished { .. } => Vec::new(),
	}
}

fn response_to_message(response: &AssistantResponse) -> ChatMessage {
	ChatMessage::Assistant {
		content: response.content.clone(),
		thinking_blocks: response.thinking_blocks.clone(),
		tool_calls: response.tool_calls.clone(),
	}
}

/// True iff `response` has no text, no thinking, and no tool
/// calls — an empty shell the provider returned when it bailed
/// mid-stream or only emitted a usage chunk. Callers (the main
/// loop in `run_turn`, the wrap-up turn, and `run_subagent`)
/// use this to skip pushing / persisting / emitting the
/// message, because:
///
/// - Anthropic rejects assistant blocks with empty or
///   whitespace-only text (`messages: text content blocks must
///   contain non-whitespace text`). Shipping the empty shell on
///   the next round-trip 400s.
/// - The on-disk shape (`{"role":"assistant","content":[]}`)
///   re-inflates on reload into the same offending shell, so a
///   reopened session would 400 on the very first send.
///
/// Whitespace-only content counts as empty too — same Anthropic
/// rule. Tool calls are kept verbatim; an assistant turn that
/// emits only `tool_use` blocks is valid.
pub(crate) fn assistant_response_is_empty(response: &AssistantResponse) -> bool {
	if !response.tool_calls.is_empty() {
		return false;
	}
	let text_empty = response.content.as_deref().map(str::trim).unwrap_or("").is_empty();
	let thinking_empty = response.thinking.as_deref().map(str::trim).unwrap_or("").is_empty();
	text_empty && thinking_empty
}

/// Emit a [`CoderEvent::TokenUsage`] report for one LLM round-trip.
///
/// Provider-supplied numbers (`response.usage`) are exact and tagged
/// `Provider`; when missing we approximate from message bytes (the
/// ratio of ~4 bytes per BPE token is a good rule of thumb across
/// the Qwen / Llama / DeepSeek families that the HF router serves)
/// and tag `Estimate` so the UI can mark the ring with a `≈`.
///
/// `messages` is the *prompt* the model just saw — i.e. the full
/// history fed in for this round-trip, **not** including the
/// assistant response. Estimating the prompt token count from
/// these bytes mirrors what the provider would have reported.
pub(crate) fn emit_token_usage(
	sink: &FolderEventSink,
	models: &CoderModels,
	model_slug: &str,
	messages: &[ChatMessage],
	response: &AssistantResponse,
) {
	let context_window = models.context_window(model_slug);
	let (prompt_tokens, completion_tokens, total_tokens, cache_read_tokens, cache_creation_tokens, source) =
		match response.usage {
			Some(u) => (
				u.prompt_tokens,
				u.completion_tokens,
				u.total_tokens,
				u.cache_read_input_tokens,
				u.cache_creation_input_tokens,
				TokenUsageSource::Provider,
			),
			None => {
				let prompt = estimate_prompt_tokens(messages);
				let completion = estimate_completion_tokens(response);
				(
					prompt,
					completion,
					prompt + completion,
					0,
					0,
					TokenUsageSource::Estimate,
				)
			}
		};
	sink.send(CoderEvent::TokenUsage {
		prompt_tokens,
		completion_tokens,
		total_tokens,
		context_window,
		source,
		cache_read_tokens,
		cache_creation_tokens,
	});
}

/// Throttled mid-stream token-usage emission. Counts up
/// `delta_len` bytes into `state`'s byte counter, then emits a
/// fresh [`CoderEvent::TokenUsage`] (Estimate-tagged) only when
/// at least `throttle` has elapsed since the previous emission.
/// Cheap enough to call on every content / thinking delta — the
/// throttle keeps the event rate to ~2 Hz no matter how fast
/// the provider streams.
fn maybe_emit_stream_usage(
	sink: &FolderEventSink,
	state: &std::sync::Mutex<(u32, std::time::Instant)>,
	throttle: std::time::Duration,
	delta_len: usize,
	prompt_estimate: u32,
	context_window: u32,
) {
	let len = u32::try_from(delta_len).unwrap_or(u32::MAX);
	let now = std::time::Instant::now();
	let completion_bytes = {
		let Ok(mut guard) = state.lock() else {
			return;
		};
		guard.0 = guard.0.saturating_add(len);
		if now.duration_since(guard.1) < throttle {
			return;
		}
		guard.1 = now;
		guard.0
	};
	// Same bytes/4 ratio used for prompt estimates so the ring
	// stays consistent across the pre-call estimate, mid-stream
	// updates, and the post-call provider-exact numbers.
	let completion_estimate = completion_bytes / 4;
	let total = prompt_estimate.saturating_add(completion_estimate);
	sink.send(CoderEvent::TokenUsage {
		prompt_tokens: prompt_estimate,
		completion_tokens: completion_estimate,
		total_tokens: total,
		context_window,
		source: TokenUsageSource::Estimate,
		cache_read_tokens: 0,
		cache_creation_tokens: 0,
	});
}

/// Rough byte-count of every chat message — covers system / user /
/// assistant / tool. Includes `tool_calls` argument strings since
/// those land in the prompt verbatim and can be substantial when
/// the model emits long structured arguments. Image attachments
/// add their data-URL length to the count: the bytes/4 estimate
/// is still a coarse approximation for vision tokens (providers
/// typically charge per tile or a fixed amount per image rather
/// than by base64 length), but counting *something* keeps a
/// freshly pasted screenshot from looking free until the
/// provider's first usage chunk lands.
pub(crate) fn estimate_prompt_tokens(messages: &[ChatMessage]) -> u32 {
	(message_bytes(messages) / 4) as u32
}

/// Pre-call prompt-token estimate that anchors on the last turn's
/// exact usage figures when available. The new wire prompt is the
/// previous prompt + the previous assistant response + everything
/// appended after it (new user messages and tool results), so:
///
/// ```text
/// estimate = last.prompt_tokens + last.completion_tokens
///          + bytes/4(messages_after_last_assistant)
/// ```
///
/// Anchoring matters because raw bytes/4 systematically undercounts
/// what real tokenizers see — and once Anthropic's `prompt_tokens`
/// includes the cached prefix (see `anthropic::merge_usage`), the
/// previous turn's number is the closest thing we have to ground
/// truth for the next prompt. Without this, the ring would shrink
/// from the exact post-stream figure back to the cruder bytes/4 the
/// moment the user hits send, then jump back when the new usage
/// chunk arrives.
///
/// Falls back to the plain bytes/4 of the full array when:
/// - there's no prior `last_usage` (very first turn of the
///   session, or right after a compaction that reset it), or
/// - the message list no longer contains an assistant turn (e.g.
///   the compaction summary fused the prefix into a system
///   message). In that case `last_usage.prompt_tokens` already
///   covers everything currently in the array, so bytes/4 of the
///   whole thing is a fine — and conservative — fallback.
fn estimate_prompt_with_anchor(last_usage: Option<&TokenUsage>, messages: &[ChatMessage]) -> u32 {
	let Some(last) = last_usage else {
		return estimate_prompt_tokens(messages);
	};
	let Some(last_assistant_idx) = messages
		.iter()
		.rposition(|m| matches!(m, ChatMessage::Assistant { .. }))
	else {
		return estimate_prompt_tokens(messages);
	};
	let tail = &messages[last_assistant_idx + 1..];
	let tail_estimate = (message_bytes(tail) / 4) as u32;
	last
		.prompt_tokens
		.saturating_add(last.completion_tokens)
		.saturating_add(tail_estimate)
}

fn message_bytes(messages: &[ChatMessage]) -> usize {
	let mut bytes: usize = 0;
	for msg in messages {
		match msg {
			ChatMessage::System { content } => bytes += content.len(),
			ChatMessage::User { content, images } => {
				bytes += content.len();
				for img in images {
					bytes += img.data_url.len();
				}
			}
			ChatMessage::Assistant {
				content, tool_calls, ..
			} => {
				bytes += content.as_deref().map(str::len).unwrap_or(0);
				for call in tool_calls {
					bytes += call.function.name.len();
					bytes += call.function.arguments.len();
				}
			}
			ChatMessage::Tool { tool_call_id, content } => {
				bytes += tool_call_id.len();
				bytes += content.len();
			}
		}
	}
	bytes
}

/// `true` iff `base_url`'s host is loopback or `.local`. Used to
/// decide whether a user provider without an API key should still
/// count as "signed in" — local llama.cpp / Ollama / vLLM
/// instances are routinely run without auth, and forcing the user
/// to "configure a key" before the panel would let them send a
/// message would be the wrong UX. Non-local hosts (OpenRouter,
/// anything reachable from the network) still require a key.
///
/// The check is conservative: we extract the host between the
/// scheme and the first path / port separator and only accept
/// `localhost`, `127.0.0.1`, `::1`, or a `.local` mDNS suffix.
/// Anything else — including `0.0.0.0` (which a misconfigured
/// server might bind to) — gets treated as remote.
fn is_local_base_url(base_url: &str) -> bool {
	let after_scheme = base_url.split_once("://").map(|(_, rest)| rest).unwrap_or(base_url);
	let host_end = after_scheme.find(['/', ':', '?', '#']).unwrap_or(after_scheme.len());
	let host = &after_scheme[..host_end];
	matches!(host, "localhost" | "127.0.0.1" | "::1") || host.ends_with(".local")
}

fn estimate_completion_tokens(response: &AssistantResponse) -> u32 {
	let mut bytes: usize = 0;
	bytes += response.content.as_deref().map(str::len).unwrap_or(0);
	bytes += response.thinking.as_deref().map(str::len).unwrap_or(0);
	for call in &response.tool_calls {
		bytes += call.function.name.len();
		bytes += call.function.arguments.len();
	}
	(bytes / 4) as u32
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

	fn assistant_with_call(id: &str, name: &str, arguments: &str) -> ChatMessage {
		ChatMessage::Assistant {
			content: None,
			thinking_blocks: Vec::new(),
			tool_calls: vec![crate::inference::ToolCall {
				id: id.to_string(),
				kind: "function".to_string(),
				function: FunctionCall {
					name: name.to_string(),
					arguments: arguments.to_string(),
				},
			}],
		}
	}

	#[test]
	fn find_recorded_tool_call_returns_name_and_parsed_args() {
		let messages = vec![
			ChatMessage::user("do the thing"),
			assistant_with_call("call_1", "edit_file", r#"{"path":"a.rs","find":"x","replace":"y"}"#),
			ChatMessage::Tool {
				tool_call_id: "call_1".into(),
				content: "ok".into(),
			},
		];
		let (name, args) = find_recorded_tool_call(&messages, "call_1").expect("call should be found");
		assert_eq!(name, "edit_file");
		assert_eq!(args["path"], "a.rs");
		assert_eq!(args["find"], "x");
	}

	#[test]
	fn find_recorded_tool_call_none_for_unknown_id() {
		let messages = vec![assistant_with_call(
			"call_1",
			"write_file",
			r#"{"path":"a.rs","content":"hi"}"#,
		)];
		assert!(find_recorded_tool_call(&messages, "nope").is_none());
	}

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
	fn sanitise_terminal_command_keeps_single_clean_line() {
		assert_eq!(
			sanitise_terminal_command("git cherry-pick feat-x@{1}"),
			"git cherry-pick feat-x@{1}"
		);
		// Strip a leading prompt marker.
		assert_eq!(sanitise_terminal_command("$ ls -la"), "ls -la");
		// Strip a single layer of surrounding backticks.
		assert_eq!(sanitise_terminal_command("`git status`"), "git status");
		// Shell quotes inside the command are preserved.
		assert_eq!(
			sanitise_terminal_command("grep -r \"foo bar\" ."),
			"grep -r \"foo bar\" ."
		);
	}

	#[test]
	fn sanitise_terminal_command_unwraps_code_fence_and_takes_first_command() {
		let fenced = "```bash\ngit fetch origin\n```";
		assert_eq!(sanitise_terminal_command(fenced), "git fetch origin");
		// Drop a trailing explanation line.
		let with_prose = "git rebase main\nThis rebases the current branch.";
		assert_eq!(sanitise_terminal_command(with_prose), "git rebase main");
	}

	#[test]
	fn sanitise_terminal_command_empty_when_only_noise() {
		assert_eq!(sanitise_terminal_command("```\n```"), "");
		assert_eq!(sanitise_terminal_command("   \n  "), "");
	}

	#[test]
	fn sanitise_branch_lowercases_and_kebabs() {
		assert_eq!(sanitise_branch_name("Add Tail Param"), "add-tail-param");
		assert_eq!(sanitise_branch_name("fix_login_bug"), "fix-login-bug");
		assert_eq!(sanitise_branch_name("UPDATE/Docs"), "update-docs");
	}

	#[test]
	fn sanitise_branch_strips_quotes_and_prefix_punctuation() {
		assert_eq!(sanitise_branch_name("`add-bucket-sync`"), "add-bucket-sync");
		assert_eq!(sanitise_branch_name("\"Refactor cache\""), "refactor-cache");
		assert_eq!(sanitise_branch_name("...weird..."), "weird");
	}

	#[test]
	fn sanitise_branch_takes_first_line_only() {
		let raw = "add-bucket-sync\n(I went with this because it's short)";
		assert_eq!(sanitise_branch_name(raw), "add-bucket-sync");
	}

	#[test]
	fn sanitise_branch_collapses_runs_and_drops_unsafe_chars() {
		assert_eq!(sanitise_branch_name("--fix:: bucket   sync!@#"), "fix-bucket-sync");
	}

	#[test]
	fn sanitise_commit_strips_wrappers_and_labels() {
		assert_eq!(
			sanitise_commit_message("\"Add tail param to upload helper\""),
			"Add tail param to upload helper"
		);
		assert_eq!(
			sanitise_commit_message("Subject: refactor cache layer"),
			"refactor cache layer"
		);
		assert_eq!(
			sanitise_commit_message("`Tighten retry budget for uploads`"),
			"Tighten retry budget for uploads"
		);
		assert_eq!(
			sanitise_commit_message("Fix offline auto-fetch flake."),
			"Fix offline auto-fetch flake"
		);
	}

	#[test]
	fn sanitise_commit_takes_first_non_empty_line() {
		let raw = "\n  \nAdd amend prefill to SCM panel\n\nDetails go here.\n";
		assert_eq!(sanitise_commit_message(raw), "Add amend prefill to SCM panel");
	}

	#[test]
	fn sanitise_commit_clamps_runaway_subject() {
		let raw = "this commit message is way too long and the model decided to write a paragraph as if it were a subject line and we should clamp it down before it blows up the composer";
		let out = sanitise_commit_message(raw);
		assert!(out.chars().count() <= 100);
		assert!(!out.ends_with(' '));
		assert!(!out.ends_with('.'));
	}

	#[test]
	fn sanitise_commit_returns_empty_for_blank_input() {
		assert_eq!(sanitise_commit_message(""), "");
		assert_eq!(sanitise_commit_message("   "), "");
		assert_eq!(sanitise_commit_message("\n\n"), "");
	}

	#[test]
	fn build_commit_message_prompt_marks_blank_fields() {
		let p = build_commit_message_prompt("", "");
		assert!(p.contains("Draft commit message (may be empty):\n(none)"));
		assert!(p.contains("Working-tree diff (`git diff HEAD`):\n(none)"));

		let p2 = build_commit_message_prompt("WIP commit", "diff --git a/foo b/foo\n+ bar\n");
		assert!(p2.contains("Draft commit message (may be empty):\nWIP commit"));
		assert!(p2.contains("diff --git a/foo b/foo"));
	}

	#[test]
	fn sanitise_branch_clamps_length_and_trims_trailing_dash() {
		let raw = "really-long-branch-name-that-exceeds-the-cap-on-length-because-the-model-was-too-verbose-today";
		let out = sanitise_branch_name(raw);
		assert!(out.chars().count() <= 60);
		assert!(!out.ends_with('-'));
	}

	#[test]
	fn sanitise_branch_returns_empty_for_garbage() {
		assert_eq!(sanitise_branch_name(""), "");
		assert_eq!(sanitise_branch_name("???"), "");
		assert_eq!(sanitise_branch_name("   "), "");
	}

	#[test]
	fn local_base_url_detection_covers_common_shapes() {
		assert!(is_local_base_url("http://localhost:8080/v1"));
		assert!(is_local_base_url("http://127.0.0.1:11434"));
		assert!(is_local_base_url("http://myhost.local/v1"));
		assert!(is_local_base_url("localhost:8080/v1"));
		assert!(!is_local_base_url("https://openrouter.ai/api/v1"));
		assert!(!is_local_base_url("https://api.anthropic.com/v1"));
		// `0.0.0.0` is a wildcard bind, not actually a reachable
		// loopback — and a server bound there is reachable from
		// the network, so we still want a key.
		assert!(!is_local_base_url("http://0.0.0.0:8080/v1"));
	}

	#[test]
	fn build_branch_name_prompt_marks_blank_fields() {
		let p = build_branch_name_prompt("", "");
		assert!(p.contains("Commit message:\n(none)"));
		assert!(p.contains("Diff summary"));
		assert!(p.contains("(none)"));
		let p2 = build_branch_name_prompt("Add tail param", " src/foo.py | 4 ++--\n 1 file changed");
		assert!(p2.contains("Add tail param"));
		assert!(p2.contains("src/foo.py"));
	}

	#[tokio::test]
	async fn read_agent_rules_returns_none_for_empty_folder() {
		let dir = tempfile::TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		assert!(read_agent_rules(&root).await.is_none());
	}

	#[tokio::test]
	async fn read_agent_rules_returns_agents_md_when_present() {
		let dir = tempfile::TempDir::new().unwrap();
		std::fs::write(dir.path().join("AGENTS.md"), "# Agent rules\n- be concise\n").unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let rules = read_agent_rules(&root).await.expect("AGENTS.md should be picked up");
		assert!(rules.contains("# Agent rules"));
		assert!(rules.contains("be concise"));
	}

	#[tokio::test]
	async fn read_agent_rules_falls_back_to_claude_md_when_agents_md_missing() {
		let dir = tempfile::TempDir::new().unwrap();
		std::fs::write(
			dir.path().join("CLAUDE.md"),
			"# Project conventions\nUse 4-space tabs.\n",
		)
		.unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let rules = read_agent_rules(&root)
			.await
			.expect("CLAUDE.md should be picked up as fallback");
		assert!(rules.contains("Project conventions"));
	}

	#[tokio::test]
	async fn read_agent_rules_prefers_agents_md_when_both_present() {
		let dir = tempfile::TempDir::new().unwrap();
		std::fs::write(dir.path().join("AGENTS.md"), "from-agents\n").unwrap();
		std::fs::write(dir.path().join("CLAUDE.md"), "from-claude\n").unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let rules = read_agent_rules(&root).await.unwrap();
		assert!(rules.contains("from-agents"));
		assert!(!rules.contains("from-claude"));
	}

	#[tokio::test]
	async fn read_agent_rules_matches_case_insensitively() {
		let dir = tempfile::TempDir::new().unwrap();
		std::fs::write(dir.path().join("Claude.md"), "# rules\n").unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		assert!(read_agent_rules(&root).await.is_some());
	}

	#[tokio::test]
	async fn read_agent_rules_truncates_oversized_files_with_sentinel() {
		let dir = tempfile::TempDir::new().unwrap();
		// Build something larger than the cap. ASCII-only so byte
		// length and char length match for the assertion below.
		let body = "x".repeat(AGENT_RULES_MAX_BYTES + 1_000);
		std::fs::write(dir.path().join("AGENTS.md"), &body).unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let rules = read_agent_rules(&root).await.unwrap();
		assert!(rules.contains("... (truncated)"));
		assert!(rules.len() < body.len());
	}

	#[tokio::test]
	async fn read_agent_rules_skips_empty_files() {
		let dir = tempfile::TempDir::new().unwrap();
		std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();
		std::fs::write(dir.path().join("CLAUDE.md"), "# fallback\n").unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		// Empty AGENTS.md falls through to CLAUDE.md.
		let rules = read_agent_rules(&root).await.unwrap();
		assert!(rules.contains("fallback"));
	}

	#[test]
	fn summarise_skips_system_and_tool_messages() {
		let msgs = vec![
			ChatMessage::System {
				content: "system prompt body".into(),
			},
			ChatMessage::user("do thing"),
			ChatMessage::Tool {
				tool_call_id: "x".into(),
				content: "tool body".into(),
			},
			ChatMessage::Assistant {
				content: Some("done".into()),
				thinking_blocks: Vec::new(),
				tool_calls: Vec::new(),
			},
		];
		let summary = summarise_transcript(&msgs);
		assert!(!summary.contains("system prompt body"));
		assert!(!summary.contains("tool body"));
		assert!(summary.contains("user: do thing"));
		assert!(summary.contains("assistant: done"));
	}

	fn header_for(id: &str) -> SessionHeader {
		SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: id.into(),
			cwd: "/tmp/steer-test".into(),
			title: "steer test".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			mode: None,
			subagent_target_folder: None,
			bash_target_override: None,
			worktree_root: None,
			worktree_branch: None,
			committed_branch: None,
		}
	}

	#[tokio::test]
	async fn drain_pending_steers_appends_in_order_and_persists() {
		// Drain has to land queued steers as `ChatMessage::User`
		// at the end of `messages` (so the chat shape stays valid
		// — system → user → … → assistant.tool_calls → tool*) and
		// must persist each as a `SessionRecord::User` in queue
		// order. This test holds both at once: queue two steers
		// behind an existing tool result, drain, check messages
		// + JSONL line up.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = header_for("sess-steer");
		let mut session = Session::new_blank();
		session.header = header.clone();
		session.session_dir = Some(dir.clone());
		session.messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("do thing"),
			ChatMessage::Assistant {
				content: None,
				thinking_blocks: Vec::new(),
				tool_calls: Vec::new(),
			},
			ChatMessage::Tool {
				tool_call_id: "tc-1".into(),
				content: "{}".into(),
			},
		];
		session.pending_steers = vec![
			PendingSteer {
				id: "steer-1".into(),
				text: "also do X".into(),
				images: Vec::new(),
			},
			PendingSteer {
				id: "steer-2".into(),
				text: "and then Y".into(),
				images: Vec::new(),
			},
		];
		let rt = Arc::new(SessionRuntime::new(session));

		let (tx, mut rx) = broadcast::channel::<CoderEventEnvelope>(16);
		let sink = FolderEventSink::new(tx, "/test/folder".to_string(), "sess-steer".to_string());
		drain_pending_steers(&rt, &sink).await;

		let session = rt.session.lock().await;
		assert!(session.pending_steers.is_empty());
		match session.messages.last() {
			Some(ChatMessage::User { content, .. }) => assert_eq!(content, "and then Y"),
			other => panic!("last message should be the second steer, got {other:?}"),
		}
		match &session.messages[session.messages.len() - 2] {
			ChatMessage::User { content, .. } => assert_eq!(content, "also do X"),
			other => panic!("second-to-last should be the first steer, got {other:?}"),
		}
		assert_eq!(session.persisted_records, 2);
		drop(session);

		// Exactly one SteerDrained per drained steer, in queue
		// order — the panel flips the matching rows out of
		// "queued" styling in the order they were sent.
		let mut drained_ids = Vec::new();
		while let Ok(env) = rx.try_recv() {
			if let CoderEvent::SteerDrained { id } = env.event {
				drained_ids.push(id);
			}
		}
		assert_eq!(drained_ids, vec!["steer-1".to_string(), "steer-2".to_string()]);

		let jsonl = tokio::fs::read_to_string(sessions::session_path(&dir, "sess-steer").as_std_path())
			.await
			.unwrap();
		// pi-mono envelopes carry plain-text user prompts in
		// `message.content` as a string, not under `text`.
		assert!(jsonl.contains(r#""content":"also do X""#), "{jsonl}");
		assert!(jsonl.contains(r#""content":"and then Y""#), "{jsonl}");
		// Ordering on disk matches queue order, not timestamp
		// (which is identical for both records anyway).
		let first = jsonl.find("also do X").unwrap();
		let second = jsonl.find("and then Y").unwrap();
		assert!(first < second, "steers persisted out of order: {jsonl}");
	}

	#[tokio::test]
	async fn drain_pending_steers_is_a_noop_when_queue_is_empty() {
		// Iteration top fires `drain_pending_steers` unconditionally;
		// the empty-queue path must not touch `messages`,
		// `persisted_records`, or `updated_at_ms`. Without this
		// guard every iteration would needlessly bump the
		// session header.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let mut session = Session::new_blank();
		session.session_dir = Some(dir);
		let original_len = session.messages.len();
		let original_updated = session.header.updated_at_ms;
		let rt = Arc::new(SessionRuntime::new(session));

		let (tx, _rx) = broadcast::channel::<CoderEventEnvelope>(8);
		let sink = FolderEventSink::new(tx, "/test/folder".to_string(), "sess-empty".to_string());
		drain_pending_steers(&rt, &sink).await;

		let session = rt.session.lock().await;
		assert_eq!(session.messages.len(), original_len);
		assert_eq!(session.header.updated_at_ms, original_updated);
		assert_eq!(session.persisted_records, 0);
	}

	#[tokio::test]
	async fn unqueue_pending_steer_pops_by_id_and_leaves_others() {
		// Pop the middle id; the other two stay in their original
		// order. Returning the popped text+images is how the panel
		// restores the draft + image chips on Ctrl+Up un-queue.
		let mut session = Session::new_blank();
		session.pending_steers = vec![
			PendingSteer {
				id: "a".into(),
				text: "first".into(),
				images: Vec::new(),
			},
			PendingSteer {
				id: "b".into(),
				text: "middle".into(),
				images: vec![ImageAttachment {
					data_url: "data:image/png;base64,xxx".into(),
					mime: "image/png".into(),
				}],
			},
			PendingSteer {
				id: "c".into(),
				text: "last".into(),
				images: Vec::new(),
			},
		];

		let popped = pop_pending_steer(&mut session, "b");
		let popped = popped.expect("pop should succeed for an in-queue id");
		assert_eq!(popped.text, "middle");
		assert_eq!(popped.images.len(), 1);
		assert_eq!(
			session.pending_steers.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
			vec!["a", "c"]
		);
	}

	#[tokio::test]
	async fn unqueue_pending_steer_returns_none_when_unknown() {
		let mut session = Session::new_blank();
		session.pending_steers = vec![PendingSteer {
			id: "a".into(),
			text: "first".into(),
			images: Vec::new(),
		}];
		assert!(pop_pending_steer(&mut session, "missing").is_none());
		assert_eq!(session.pending_steers.len(), 1);
	}

	#[test]
	fn assistant_response_is_empty_flags_real_empties() {
		// All-empty shell: no text, no thinking, no tool calls.
		// Providers occasionally emit one when they bail mid-
		// stream — the runner must not push or persist these.
		let empty = AssistantResponse {
			content: None,
			thinking: None,
			thinking_blocks: Vec::new(),
			tool_calls: Vec::new(),
			usage: None,
			stop_reason: None,
		};
		assert!(assistant_response_is_empty(&empty));

		// Whitespace-only content is empty for our purposes —
		// Anthropic rejects whitespace-only blocks the same way
		// as empty arrays.
		let whitespace = AssistantResponse {
			content: Some("   \n\t".into()),
			thinking: Some("   ".into()),
			thinking_blocks: Vec::new(),
			tool_calls: Vec::new(),
			usage: None,
			stop_reason: None,
		};
		assert!(assistant_response_is_empty(&whitespace));
	}

	#[test]
	fn assistant_response_is_empty_keeps_real_messages() {
		// Text content: keep.
		let text = AssistantResponse {
			content: Some("hello".into()),
			thinking: None,
			thinking_blocks: Vec::new(),
			tool_calls: Vec::new(),
			usage: None,
			stop_reason: None,
		};
		assert!(!assistant_response_is_empty(&text));

		// Thinking only: keep.
		let thinking = AssistantResponse {
			content: None,
			thinking: Some("let me think".into()),
			thinking_blocks: Vec::new(),
			tool_calls: Vec::new(),
			usage: None,
			stop_reason: None,
		};
		assert!(!assistant_response_is_empty(&thinking));

		// Tool calls only (no text): legitimate tool-using turn.
		let tool_only = AssistantResponse {
			content: None,
			thinking: None,
			thinking_blocks: Vec::new(),
			tool_calls: vec![crate::inference::ToolCall {
				id: "call-1".into(),
				kind: "function".into(),
				function: crate::inference::FunctionCall {
					name: "bash".into(),
					arguments: "{}".into(),
				},
			}],
			usage: None,
			stop_reason: None,
		};
		assert!(!assistant_response_is_empty(&tool_only));
	}

	#[test]
	fn estimate_prompt_with_anchor_falls_back_to_bytes_div_4_when_no_last_usage() {
		let messages = vec![
			ChatMessage::System {
				content: "x".repeat(40),
			},
			ChatMessage::User {
				content: "y".repeat(40),
				images: Vec::new(),
			},
		];
		// 80 bytes / 4 = 20.
		assert_eq!(estimate_prompt_with_anchor(None, &messages), 20);
	}

	#[test]
	fn estimate_prompt_with_anchor_anchors_on_prior_usage_and_estimates_tail() {
		let last = TokenUsage {
			prompt_tokens: 10_000,
			completion_tokens: 500,
			total_tokens: 10_500,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 0,
		};
		let messages = vec![
			ChatMessage::System { content: String::new() },
			ChatMessage::User {
				content: String::new(),
				images: Vec::new(),
			},
			ChatMessage::Assistant {
				content: Some(String::new()),
				thinking_blocks: Vec::new(),
				tool_calls: Vec::new(),
			},
			// 80 bytes appended after the last assistant turn:
			// new user message + a tool result.
			ChatMessage::User {
				content: "u".repeat(40),
				images: Vec::new(),
			},
			ChatMessage::Tool {
				tool_call_id: String::new(),
				content: "t".repeat(40),
			},
		];
		// 10_000 (prompt) + 500 (completion) + 80/4 (tail) = 10_520.
		assert_eq!(estimate_prompt_with_anchor(Some(&last), &messages), 10_520);
	}

	#[test]
	fn estimate_prompt_with_anchor_falls_back_when_no_assistant_in_array() {
		// Right after a compaction the prefix collapses into a
		// single system message — no assistant turn left in the
		// array. `last_usage` has been reset by the compaction
		// path, but defensively the helper must still degrade
		// gracefully if called with a stale snapshot.
		let last = TokenUsage {
			prompt_tokens: 99_999,
			completion_tokens: 0,
			total_tokens: 99_999,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 0,
		};
		let messages = vec![
			ChatMessage::System {
				content: "x".repeat(40),
			},
			ChatMessage::User {
				content: "y".repeat(40),
				images: Vec::new(),
			},
		];
		// No assistant → bytes/4 of the whole array (20), not
		// the stale anchor.
		assert_eq!(estimate_prompt_with_anchor(Some(&last), &messages), 20);
	}

	#[tokio::test]
	async fn recover_in_memory_orphans_fills_unpaired_tool_calls() {
		// Simulate the on-disk + in-memory state we land in
		// after the user Esc's a turn mid-tool: the assistant
		// record (with `tool_calls`) is in `messages` and the
		// JSONL, but no matching `Tool` ever landed. Without
		// recovery the very next `chat_completion` request
		// ships an unpaired `tool_use` and gets HTTP 400.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = header_for("sess-orphan");
		let mut session = Session::new_blank();
		session.header = header.clone();
		session.session_dir = Some(dir.clone());
		let assistant = ChatMessage::Assistant {
			content: None,
			thinking_blocks: Vec::new(),
			tool_calls: vec![crate::inference::ToolCall {
				id: "call-1".into(),
				kind: "function".into(),
				function: crate::inference::FunctionCall {
					name: "read_file".into(),
					arguments: "{}".into(),
				},
			}],
		};
		session.messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("read foo.rs"),
			assistant.clone(),
		];
		// Persist the user + assistant records the way a real
		// turn would have, so the disk shape we're testing
		// recovery against matches production.
		sessions::append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "read foo.rs".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();
		sessions::append_record(
			&dir,
			&header,
			&SessionRecord::Assistant {
				content: None,
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: match &assistant {
					ChatMessage::Assistant { tool_calls, .. } => tool_calls.clone(),
					_ => unreachable!(),
				},
				model: None,
				stop_reason: None,
			},
		)
		.await
		.unwrap();
		session.persisted_records = 2;
		let rt = Arc::new(SessionRuntime::new(session));

		let (events, _rx) = broadcast::channel(16);
		let mut rx = events.subscribe();
		let sink = FolderEventSink::new(events, "/tmp/folder", "sess-orphan");

		recover_in_memory_orphans(&rt, &sink).await;

		// In-memory: orphan filled with a synthetic Tool
		// carrying the sentinel JSON.
		let messages = rt.session.lock().await.messages.clone();
		let tail = messages.last().expect("messages non-empty");
		match tail {
			ChatMessage::Tool { tool_call_id, content } => {
				assert_eq!(tool_call_id, "call-1");
				assert_eq!(content, sessions::INTERRUPTED_TOOL_RESULT_JSON);
			}
			other => panic!("expected Tool, got {other:?}"),
		}

		// On disk: a matching SessionRecord::Tool got appended,
		// so a reload from `open_session` finds no orphan to
		// re-synthesise (idempotent).
		let loaded = sessions::load(&dir, "sess-orphan").await.unwrap();
		assert!(sessions::orphan_tool_call_ids(&loaded.records).is_empty());
		let last_record = loaded.records.last().expect("at least one record");
		match last_record {
			SessionRecord::Tool {
				tool_call_id,
				content,
				tool_name: _,
			} => {
				assert_eq!(tool_call_id, "call-1");
				assert_eq!(content, sessions::INTERRUPTED_TOOL_RESULT_JSON);
			}
			other => panic!("expected Tool record on disk, got {other:?}"),
		}

		// Event: panel sees an errored ToolResult so the row
		// flips from "running" to error.
		let envelope = rx.try_recv().expect("ToolResult event emitted");
		match envelope.event {
			CoderEvent::ToolResult { id, is_error, .. } => {
				assert_eq!(id, "call-1");
				assert!(is_error);
			}
			other => panic!("expected ToolResult event, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn recover_in_memory_orphans_is_idempotent_for_completed_calls() {
		// When every tool_call already has a matching Tool
		// message, recovery is a no-op — no extra messages
		// pushed, no events emitted, no disk writes.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = header_for("sess-clean");
		let mut session = Session::new_blank();
		session.header = header.clone();
		session.session_dir = Some(dir.clone());
		session.messages = vec![
			ChatMessage::user("read foo.rs"),
			ChatMessage::Assistant {
				content: None,
				thinking_blocks: Vec::new(),
				tool_calls: vec![crate::inference::ToolCall {
					id: "call-1".into(),
					kind: "function".into(),
					function: crate::inference::FunctionCall {
						name: "read_file".into(),
						arguments: "{}".into(),
					},
				}],
			},
			ChatMessage::Tool {
				tool_call_id: "call-1".into(),
				content: "{\"ok\":true}".into(),
			},
		];
		let before_len = session.messages.len();
		let rt = Arc::new(SessionRuntime::new(session));

		let (events, _rx) = broadcast::channel(16);
		let mut rx = events.subscribe();
		let sink = FolderEventSink::new(events, "/tmp/folder", "sess-clean");

		recover_in_memory_orphans(&rt, &sink).await;

		assert_eq!(rt.session.lock().await.messages.len(), before_len);
		assert!(rx.try_recv().is_err());
	}
}
