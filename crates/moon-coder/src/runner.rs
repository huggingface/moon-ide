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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use moon_core::WorkspaceRegistry;
use serde_json::Value;
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::auth::{Authenticator, DeviceCode, HfIdentity};
use crate::defaults::{
	EMPTY_RESPONSE_RETRIES, MAX_TURN_ITERATIONS, OUTPUT_CAP_CONTINUATIONS, OUTPUT_CAP_CONTINUATION_PROMPT,
	PHASE_6_0_SYSTEM_PROMPT,
};
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
	self, current_time_ms, new_named_session_id, new_session_id, session_title_from_prompt, sessions_dir,
	subagent_session_dir, BashTargetOverride, LoadedSession, SessionHeader, SessionRecord, SessionSummary,
	SESSION_SCHEMA_VERSION,
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
	/// Count of live turn loops across every session in the
	/// process (visible sessions, worktree sessions, coordinator
	/// workers — anything that goes through `spawn_turn_loop`).
	/// The Tauri layer subscribes via
	/// [`CoderHandle::watch_running_turns`] to drive the OS-level
	/// "agent running / finished" indicator (tray icon + window
	/// icon badge). Detached `task` sub-agents are deliberately
	/// not counted — they're fire-and-forget helpers the parent
	/// collects later, not something the user is waiting on.
	running_turns: watch::Sender<usize>,
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
	/// orchestrator session id to the workers it spawned, so the
	/// events-as-messages feeder can filter the broadcast for just
	/// its live workers and forward dispatch packets back to the
	/// orchestrator. Lives on `CoderState` (not the orchestrator's
	/// `SessionRuntime`) so the feeder task can read it without
	/// holding a session lock.
	coordinator_workers: Arc<RwLock<CoordinatorRegistry>>,
	/// Parent → detached-sub-agent registry ([ADR 0053]). Same
	/// lifetime rationale as `coordinator_workers`: the finish
	/// feeder and the collect/abort tools read it without holding
	/// a session lock, and it's in-memory because a restart loses
	/// the live runs anyway.
	detached_tasks: Arc<RwLock<DetachedTaskRegistry>>,
}

/// In-memory orchestrator → worker links (ADR 0030). Not persisted:
/// neither the feeder task nor a background turn survives a process
/// restart, so a restarted coordinator has no live workers anyway.
#[derive(Default)]
struct CoordinatorRegistry {
	by_orchestrator: HashMap<String, CoordinatorWorkers>,
}

/// Tracks one orchestrator's spawned workers + whether the dispatch
/// feeder task is already running for it.
#[derive(Default)]
struct CoordinatorWorkers {
	workers: HashSet<String>,
	feeder_running: bool,
	/// Workers the user explicitly disconnected from this
	/// orchestrator (ADR 0052). Kept in the map (not removed) so
	/// the feeder can deliver one final `TurnComplete` notice after
	/// unhooking — the orchestrator needs to hear that the worker
	/// left the fleet or it would keep waiting on it.
	disconnected: HashSet<String>,
}

impl CoordinatorRegistry {
	/// Register `worker_id` under `orchestrator_id`. Returns `true`
	/// when the caller must spawn the dispatch feeder (first worker
	/// registered for this orchestrator).
	fn register(&mut self, orchestrator_id: &str, worker_id: &str) -> bool {
		let entry = self.by_orchestrator.entry(orchestrator_id.to_string()).or_default();
		entry.workers.insert(worker_id.to_string());
		let spawn_feeder = !entry.feeder_running;
		entry.feeder_running = true;
		spawn_feeder
	}

	/// Whether `orchestrator_id`'s feeder should forward events from
	/// `session_id` — i.e. whether it's one of its workers. A user
	/// message into a worker does **not** unhook it (ADR 0043); the
	/// coordinator keeps receiving its updates. An explicit
	/// disconnect does (ADR 0052).
	fn feeds(&self, orchestrator_id: &str, session_id: &str) -> bool {
		self
			.by_orchestrator
			.get(orchestrator_id)
			.is_some_and(|entry| entry.workers.contains(session_id) && !entry.disconnected.contains(session_id))
	}

	/// The orchestrator that spawned `worker_id`, if any and still
	/// attached. Used to tell a coordinator that the user just
	/// messaged one of its workers (ADR 0043).
	fn orchestrator_of(&self, worker_id: &str) -> Option<&str> {
		self
			.by_orchestrator
			.iter()
			.find(|(_, entry)| entry.workers.contains(worker_id) && !entry.disconnected.contains(worker_id))
			.map(|(orchestrator_id, _)| orchestrator_id.as_str())
	}

	/// All workers registered under `orchestrator_id`, each paired with
	/// its attachment state (`true` = still attached, `false` =
	/// disconnected but not yet fully released). Powers the
	/// coordinator's `list_workers` tool and the fleet counts in its
	/// wake messages. Order is unspecified (a `HashSet`); callers sort.
	fn workers_of(&self, orchestrator_id: &str) -> Vec<(String, bool)> {
		let Some(entry) = self.by_orchestrator.get(orchestrator_id) else {
			return Vec::new();
		};
		entry
			.workers
			.iter()
			.map(|w| (w.clone(), !entry.disconnected.contains(w)))
			.collect()
	}

	/// Count of `orchestrator_id`'s workers that are still attached —
	/// the "N workers still on your fleet" line in a wake message.
	fn attached_count(&self, orchestrator_id: &str) -> usize {
		self.by_orchestrator.get(orchestrator_id).map_or(0, |entry| {
			entry
				.workers
				.iter()
				.filter(|w| !entry.disconnected.contains(*w))
				.count()
		})
	}

	/// Mark `worker_id` disconnected from `orchestrator_id` (ADR
	/// 0052). Returns `true` when the link existed and was still
	/// attached — the caller then notifies the orchestrator.
	fn disconnect(&mut self, orchestrator_id: &str, worker_id: &str) -> bool {
		let Some(entry) = self.by_orchestrator.get_mut(orchestrator_id) else {
			return false;
		};
		entry.workers.contains(worker_id) && entry.disconnected.insert(worker_id.to_string())
	}

	/// Take the orchestrator ↔ worker link out entirely. The feeder
	/// calls this right after its final wake lands so a later
	/// disconnect attempt finds nothing to detach. Returns `true`
	/// when `worker_id` was registered under `orchestrator_id` at
	/// all (attached or already disconnected) — i.e. whether the
	/// caller should let the UI show / hide the affordance.
	fn remove(&mut self, orchestrator_id: &str, worker_id: &str) -> bool {
		let Some(entry) = self.by_orchestrator.get_mut(orchestrator_id) else {
			return false;
		};
		let was_worker = entry.workers.remove(worker_id);
		entry.disconnected.remove(worker_id);
		was_worker
	}

	/// Whether `worker_id` is registered as a worker of any
	/// orchestrator — attached or disconnected. Drives the
	/// session-bar disconnect affordance (ADR 0052), which must
	/// also reach an already-disconnected worker so a second click
	/// can end its current turn.
	fn is_worker(&self, worker_id: &str) -> bool {
		self
			.by_orchestrator
			.values()
			.any(|entry| entry.workers.contains(worker_id))
	}

	/// The orchestrator `worker_id` belongs to — **including** when
	/// already disconnected. The disconnect command targets the
	/// entry regardless of attachment so the control-tool refusal
	/// (`steer_worker` & co.) is keyed off the same lookup.
	fn owning_orchestrator_of(&self, worker_id: &str) -> Option<&str> {
		self
			.by_orchestrator
			.iter()
			.find(|(_, entry)| entry.workers.contains(worker_id))
			.map(|(orchestrator_id, _)| orchestrator_id.as_str())
	}

	/// Whether the coordinator's control tools may still act on
	/// `worker_id` — i.e. it's a registered, still-attached worker
	/// (ADR 0052). Unregistered sessions stay unaffected: nothing
	/// here gates a coordinator from steering a session it never
	/// spawned.
	fn controls(&self, worker_id: &str) -> bool {
		!self
			.by_orchestrator
			.values()
			.any(|entry| entry.disconnected.contains(worker_id))
	}
}

/// Whether a detached sub-agent run has settled, and how.
/// `Aborted` is split from `Failed` so `task_collect` can say
/// "you aborted it" instead of surfacing a generic error.
#[derive(Debug, Clone)]
enum DetachedFinish {
	Done(crate::subagent::SubagentReport),
	Failed(String),
	Aborted,
}

/// One in-flight (or settled, cached) detached sub-agent run.
/// `notify` fires exactly once when the run settles; `task_collect`'s
/// `wait_ms` blocks on it instead of busy-polling.
struct DetachedEntry {
	cancel: CancellationToken,
	notify: Arc<tokio::sync::Notify>,
	finish: Mutex<Option<DetachedFinish>>,
}

/// In-memory parent → detached-sub-agent registry ([ADR 0053]).
/// Maps each parent session id to the detached sub-agents it
/// spawned, so the finish feeder can tell which `SubagentFinished`
/// events belong to which parent, `task_collect` / `task_abort`
/// can find a run by id, and the user-level abort can cascade to
/// a session's live detached runs. Not persisted: a process
/// restart loses the live runs the same way a coordinator's
/// workers are lost (the sub-agent's JSONL stays on disk).
#[derive(Default)]
struct DetachedTaskRegistry {
	/// Parent session id → its detached sub-agent ids (live or
	/// settled-but-cached). The entry sets drive the feeder
	/// subscription lifetime and the abort cascade.
	by_parent: HashMap<String, HashSet<String>>,
	/// Sub-agent id → its run entry. Lives past `settle` so a
	/// later `task_collect` still returns the cached report.
	entries: HashMap<String, Arc<DetachedEntry>>,
}

impl DetachedTaskRegistry {
	/// Register a freshly-spawned detached run under its parent.
	/// Returns the shared entry the spawned task settles into.
	fn register(&mut self, parent_session_id: &str, subagent_id: &str, cancel: CancellationToken) -> Arc<DetachedEntry> {
		let entry = Arc::new(DetachedEntry {
			cancel,
			notify: Arc::new(tokio::sync::Notify::new()),
			finish: Mutex::new(None),
		});
		self
			.by_parent
			.entry(parent_session_id.to_string())
			.or_default()
			.insert(subagent_id.to_string());
		self.entries.insert(subagent_id.to_string(), entry.clone());
		entry
	}

	/// Whether `subagent_id` is a detached run of `parent_session_id`.
	fn is_detached_of(&self, parent_session_id: &str, subagent_id: &str) -> bool {
		self
			.by_parent
			.get(parent_session_id)
			.is_some_and(|set| set.contains(subagent_id))
	}

	/// The run entry for `subagent_id`, if it's a registered
	/// detached run (live or settled-cached). `task_collect` /
	/// `task_abort` look runs up here; ids that were never detached
	/// (or were lost to a restart) miss.
	fn entry(&self, subagent_id: &str) -> Option<Arc<DetachedEntry>> {
		self.entries.get(subagent_id).cloned()
	}

	/// Record a run's terminal state and wake any parked
	/// `task_collect(wait_ms)`. The entry stays in `entries` (and
	/// the parent's set) so a later collect returns the cached
	/// report instantly.
	async fn settle(entry: &DetachedEntry, finish: DetachedFinish) {
		*entry.finish.lock().await = Some(finish);
		entry.notify.notify_waiters();
	}

	/// All live cancel tokens for `parent_session_id`'s detached
	/// runs — the user-level abort cascade cancels each. A run whose
	/// token is already cancelled is harmlessly re-cancelled.
	fn live_tokens_of(&self, parent_session_id: &str) -> Vec<CancellationToken> {
		self
			.by_parent
			.get(parent_session_id)
			.into_iter()
			.flatten()
			.filter_map(|id| self.entries.get(id))
			.map(|entry| entry.cancel.clone())
			.collect()
	}

	/// Drop every entry of `parent_session_id`'s detached runs,
	/// cancelling any still-live ones. Called when the parent
	/// session is deleted — nothing can collect a report once the
	/// parent is gone, and this bound (entries live as long as
	/// their parent session) is what keeps a long-lived process
	/// from accumulating one map entry per detached run forever.
	/// Settled entries must NOT be pruned any earlier: the finish
	/// wake is a pointer, not the report, and the parent may only
	/// get around to `task_collect` several turns later.
	fn prune_parent(&mut self, parent_session_id: &str) {
		let Some(ids) = self.by_parent.remove(parent_session_id) else {
			return;
		};
		for id in ids {
			if let Some(entry) = self.entries.remove(&id) {
				// Harmless on settled runs; stops live ones.
				entry.cancel.cancel();
			}
		}
	}
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
	/// Live mirror of `header.bash_target_override` — shared into
	/// every turn's `ToolContext`, so flipping the per-session
	/// host/container toggle re-routes the next tool dispatch of an
	/// in-flight turn instead of waiting for a fresh one. The header
	/// stays the persisted source of truth; this is its in-memory
	/// shadow.
	force_host_bash: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionRuntime {
	fn new(session: Session) -> Self {
		let force_host = session.header.bash_target_override == Some(BashTargetOverride::ForceHost);
		Self {
			session: Mutex::new(session),
			turn: Mutex::new(TurnState::default()),
			prompts: crate::prompts::PromptRegistry::default(),
			force_host_bash: Arc::new(std::sync::atomic::AtomicBool::new(force_host)),
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

tokio::task_local! {
	/// Turn-scoped event sink so layers without a sink parameter
	/// (the inference client's backoff/rotation loop) can surface
	/// live UI notices. Scoped by `spawn_turn_loop`; deliberately
	/// not scoped for sub-agent loops (their events need inner-
	/// wrapping, and a backoff bar in the pop-out isn't worth that
	/// plumbing yet).
	pub(crate) static TURN_EVENT_SINK: FolderEventSink;
}

/// Send `event` through the current turn's sink, if any. No-op
/// outside a turn scope (probes, title passes, sub-agents).
pub(crate) fn emit_turn_event(event: CoderEvent) {
	let _ = TURN_EVENT_SINK.try_with(|sink| sink.send(event));
}

/// A backoff/rotation notice that's still "current" for a session:
/// the wait hasn't elapsed and no later event has superseded it.
/// `until_ms` is the wall-clock deadline so a client that connects
/// mid-wait gets the *remaining* time, not the original delay.
#[derive(Clone)]
pub(crate) struct ActiveRetry {
	pub(crate) model: String,
	pub(crate) status: u16,
	pub(crate) attempt: u32,
	pub(crate) max_attempts: u32,
	pub(crate) until_ms: i64,
	pub(crate) rotated_to: Option<String>,
}

/// Live retry state per session. `retry_backoff` is a live-only
/// event, so a phone that opens a session *during* a two-minute
/// backoff would otherwise see a spinner with no explanation —
/// the open path replays the current entry as a fresh live event.
fn active_retries() -> &'static std::sync::Mutex<HashMap<String, ActiveRetry>> {
	static MAP: std::sync::OnceLock<std::sync::Mutex<HashMap<String, ActiveRetry>>> = std::sync::OnceLock::new();
	MAP.get_or_init(Default::default)
}

/// Fast path for the hot `send` funnel: skip locking when nothing
/// is waiting anywhere.
static ACTIVE_RETRY_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn record_active_retry(session_id: &str, entry: ActiveRetry) {
	let mut map = active_retries().lock().expect("active retries poisoned");
	map.insert(session_id.to_owned(), entry);
	ACTIVE_RETRY_COUNT.store(map.len(), std::sync::atomic::Ordering::Relaxed);
}

fn clear_active_retry(session_id: &str) {
	if ACTIVE_RETRY_COUNT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
		return;
	}
	let mut map = active_retries().lock().expect("active retries poisoned");
	if map.remove(session_id).is_some() {
		ACTIVE_RETRY_COUNT.store(map.len(), std::sync::atomic::Ordering::Relaxed);
	}
}

pub(crate) fn active_retry_for(session_id: &str) -> Option<ActiveRetry> {
	if ACTIVE_RETRY_COUNT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
		return None;
	}
	active_retries()
		.lock()
		.expect("active retries poisoned")
		.get(session_id)
		.cloned()
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
		// Single funnel for session events, so retry bookkeeping
		// lives here: a backoff notice becomes the session's live
		// retry state, and *any* later event supersedes it (a delta
		// means the retry landed; a terminator means the turn
		// settled). Keeps a mid-backoff reopen honest without a
		// second cleanup path.
		match &event {
			CoderEvent::RetryBackoff {
				model,
				status,
				attempt,
				max_attempts,
				delay_ms,
				rotated_to,
			} => {
				record_active_retry(
					&self.session_id,
					ActiveRetry {
						model: model.clone(),
						status: *status,
						attempt: *attempt,
						max_attempts: *max_attempts,
						until_ms: current_time_ms() + i64::try_from(*delay_ms).unwrap_or(0),
						rotated_to: rotated_to.clone(),
					},
				);
			}
			_ => clear_active_retry(&self.session_id),
		}
		let _ = self.sender.send(CoderEventEnvelope {
			folder: self.folder.clone(),
			session_id: self.session_id.clone(),
			event,
		});
	}

	pub(crate) fn folder(&self) -> &str {
		&self.folder
	}

	/// The session this sink's events are stamped with. For a
	/// coordinator session this is the orchestrator id its workers are
	/// registered under — `list_workers` reads it to look up the fleet.
	pub(crate) fn session_id(&self) -> &str {
		&self.session_id
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
	/// See [`SessionCacheStats`]. Reset only with the session.
	cache_stats: SessionCacheStats,
	/// In-memory todo list maintained by the agent's `todo_write`
	/// tool. Survives compaction (the messages prefix gets
	/// folded; the plan does not) and is reset only when the user
	/// starts a new session. Persisted via
	/// [`SessionRecord::TodosUpdate`] — replay seeds this from
	/// the **last** record on disk.
	todos: Vec<crate::TodoItem>,
	/// User messages typed into the composer while a turn is
	/// already running — plus, on a coordinator session, parked
	/// user-message notices ([`park_coordinator_notice`], ADR 0062),
	/// which may be queued while the session is idle and wait here
	/// for its next turn. The runner drains them into `messages`
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
	/// hit disk (they live here, not in the JSONL), so a process
	/// restart can't recover them. A *session* reopen can, though:
	/// the queue outlives `open_session` (the runtime isn't
	/// remounted for a live turn), so the replay re-emits one
	/// queued [`CoderEvent::UserMessage`] per entry and the panel
	/// gets its muted row, "go now" button and `ArrowUp`-unqueue
	/// back.
	pending_steers: Vec<PendingSteer>,
	/// Last per-turn diff (ADR 0030). Set by `emit_turn_diff` at turn
	/// end; read by `observe_session` so an orchestrator's
	/// `observe_worker` gets the diff as a dispatch-packet artifact
	/// without reading the worker's full transcript. `None` until the
	/// first turn that touches files lands a `TurnDiff`.
	last_turn_diff: Option<(Vec<String>, String)>,
	/// Images held back from the wire because the accumulated
	/// payload crossed the route's budget — bytes or attachment
	/// count, whichever trips first (ADR 0049). Keyed by
	/// payload hash, grows only, and deliberately *not* persisted:
	/// a reopened session starts with a cold prompt cache anyway, so
	/// re-deciding from scratch costs nothing and a raised budget
	/// takes effect on reload.
	elided_images: std::collections::HashSet<u64>,
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
	/// Unix-ms queue time, replayed as the row's `created_at_ms`
	/// so a reopen doesn't restamp the steer to "now".
	queued_at_ms: i64,
	/// Coordinator-originated steer (`steer_worker` into a busy
	/// worker). Carried through the drain so both the queued and
	/// the drained `UserMessage` events — and the persisted
	/// record — keep the mark.
	from_coordinator: bool,
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
				// Stamped post-create by `handle_spawn_worker` for
				// coordinator workers; every other session stays None.
				orchestrator_session_id: None,
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
			cache_stats: SessionCacheStats::default(),
			todos: Vec::new(),
			pending_steers: Vec::new(),
			last_turn_diff: None,
			elided_images: std::collections::HashSet::new(),
		}
	}

	fn summary(&self) -> SessionSummary {
		SessionSummary {
			id: self.header.id.clone(),
			title: self.header.title.clone(),
			created_at_ms: self.header.created_at_ms,
			updated_at_ms: self.header.updated_at_ms,
			worktree_root: self.header.worktree_root.clone(),
			worktree_branch: self.header.worktree_branch.clone(),
			committed_branch: self.header.committed_branch.clone(),
			mode: self.header.mode.clone(),
			last_error: false,
			interrupted: false,
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
	/// active folder (ADR 0028 — per-project session scoping).
	/// `None` when nothing is active.
	async fn coder_root_folder(&self) -> Option<Arc<WorkspaceFolderEntry>> {
		let active = self.workspaces.active_folder().await?;
		Some(self.coder_root_of(active).await)
	}

	/// The folder that owns the coder session list for `folder`
	/// (ADR 0028 — per-project session scoping). A worktree folder
	/// defers to its **parent project root**, so a parent and all its
	/// worktrees share one session list; any other folder is its own
	/// root. The parent fallback to the folder itself covers an
	/// orphaned worktree whose parent isn't bound (shouldn't happen
	/// post-W.3).
	async fn coder_root_of(&self, folder: Arc<WorkspaceFolderEntry>) -> Arc<WorkspaceFolderEntry> {
		if let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = &folder.folder.origin {
			if let Some(parent) = self.workspaces.folder_for_path(parent_path).await {
				return parent;
			}
		}
		folder
	}

	/// Resolve to `(coder-root folder's FolderSession, folder path)`.
	/// Used by commands that operate at the folder level
	/// (`list_sessions`, `new_session`, the runtime-routing inside
	/// `open_session` / `delete_session`). Routes through
	/// [`Self::coder_root_folder`] so worktree folders share their
	/// parent project's session list. When no folder is bound, falls
	/// back to the scratch root ([`Self::no_folder_root`]) so coder
	/// sessions work in an empty workspace.
	async fn active_folder_session(&self) -> Result<(Arc<FolderSession>, Utf8PathBuf), CoderError> {
		let folder_path = match self.coder_root_folder().await {
			Some(folder) => Utf8PathBuf::from(folder.folder.path.clone()),
			None => self.no_folder_root().await?,
		};
		let session = self.folder_session_for(&folder_path).await;
		Ok((session, folder_path))
	}

	/// Sessions-root path for an empty workspace: the user's home
	/// directory, canonicalised. Never registered in
	/// [`WorkspaceRegistry`] — a synthetic [`WorkspaceFolderEntry`]
	/// is built on demand by [`Self::folder_entry_for`] instead, so
	/// the scratch root can't leak into the folder bar, the MCP
	/// roots set, or the registry-keyed fs watcher. Home is the
	/// working directory (a shell's default cwd) and the
	/// sessions-dir slug anchor.
	async fn no_folder_root(&self) -> Result<Utf8PathBuf, CoderError> {
		no_folder_root().await
	}

	/// The registry's `folder_for_path`, plus the scratch root: a
	/// fresh, unregistered entry whose host is a plain
	/// [`moon_core::LocalHost`] rooted at home. Built on demand
	/// (never cached) so the shell-resolver / log-sink wiring a
	/// registry-added folder gets is irrelevant here — a scratch
	/// session's bash never routes to the workspace shell container
	/// (its root is never in the container's applied mount set) and
	/// its writes never hit format-on-save (home has no project
	/// config).
	async fn folder_entry_for(&self, path: &str) -> Option<Arc<WorkspaceFolderEntry>> {
		if let Some(entry) = self.workspaces.folder_for_path(path).await {
			return Some(entry);
		}
		scratch_folder_entry(path).await
	}

	/// Coder-root path for an explicitly-named bound folder — the
	/// folder-targeting mirror of [`Self::coder_root_folder`]. Errors
	/// when `folder` isn't bound in the workspace. Used by the
	/// bridge's folder-scoped session commands (the companion's
	/// project switcher) so the phone can drive any bound folder
	/// without touching the desktop's active-folder selection.
	async fn coder_root_at(&self, folder: &str) -> Result<Utf8PathBuf, CoderError> {
		let entry = self
			.workspaces
			.folder_for_path(folder)
			.await
			.ok_or_else(|| CoderError::Internal(format!("folder not bound in this workspace: {folder}")))?;
		let root = self.coder_root_of(entry).await;
		Ok(Utf8PathBuf::from(root.folder.path.clone()))
	}

	/// [`Self::active_folder_session`] when `folder` is `None`, else
	/// the named folder's coder-root session. The shared resolution
	/// for session commands that take an optional folder target.
	async fn folder_session_or_active(
		&self,
		folder: Option<&str>,
	) -> Result<(Arc<FolderSession>, Utf8PathBuf), CoderError> {
		let Some(path) = folder else {
			return self.active_folder_session().await;
		};
		let root = self.coder_root_at(path).await?;
		let session = self.folder_session_for(&root).await;
		Ok((session, root))
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

	/// `true` when `path` is the scratch root — the sessions anchor
	/// for an empty workspace, not a bound folder. Drives the
	/// folder-level commands that must not materialise it.
	async fn is_no_folder_root(&self, path: &Utf8Path) -> bool {
		match no_folder_root().await {
			Ok(root) => root == path,
			Err(_) => false,
		}
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

/// The scratch root itself: the user's home directory,
/// canonicalised. Free function (not a `CoderState` method) so the
/// synthetic-entry builder and tests can reach it without a state.
async fn no_folder_root() -> Result<Utf8PathBuf, CoderError> {
	let home = std::env::var("HOME")
		.or_else(|_| std::env::var("USERPROFILE"))
		.map_err(|_| CoderError::Internal("no folder bound and HOME is not set — can't anchor a scratch session".into()))?;
	let path = Utf8PathBuf::from(home);
	tokio::fs::canonicalize(path.as_std_path())
		.await
		.map_err(CoderError::from)
		.and_then(|p| {
			Utf8PathBuf::from_path_buf(p).map_err(|p| CoderError::Internal(format!("non-utf8 path: {}", p.display())))
		})
}

/// A synthetic, unregistered [`WorkspaceFolderEntry`] for the
/// scratch root — `Some` only when `path` *is* the scratch root.
async fn scratch_folder_entry(path: &str) -> Option<Arc<WorkspaceFolderEntry>> {
	let scratch = no_folder_root().await.ok()?;
	if path != scratch.as_str() {
		return None;
	}
	let folder = moon_protocol::workspace::WorkspaceFolder {
		path: scratch.to_string(),
		name: scratch.file_name().unwrap_or("home").to_string(),
		host: moon_protocol::workspace::HostKind::Local,
		origin: moon_protocol::workspace::FolderOrigin::UserPicked,
	};
	Some(Arc::new(WorkspaceFolderEntry {
		folder,
		host: Arc::new(moon_core::LocalHost::new(scratch)),
	}))
}

impl CoderHandle {
	pub fn new(
		workspaces: Arc<WorkspaceRegistry>,
		workspaces_dir: Utf8PathBuf,
		coder_sessions_dir: Utf8PathBuf,
		folder_summaries_dir: Utf8PathBuf,
		initial_models: CoderModels,
		terminals: Arc<moon_terminal::TerminalRegistry>,
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
		let tools = ToolRegistry::new(
			workspaces.clone(),
			workspaces_dir.clone(),
			web,
			terminals,
			models.clone(),
		);
		let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
		let (running_turns, _) = watch::channel(0usize);
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
				running_turns,
				sessions_by_folder: Arc::new(RwLock::new(HashMap::new())),
				workspaces,
				workspaces_dir,
				coder_sessions_dir,
				folder_summaries,
				models,
				provider_keys,
				hub_sync,
				coordinator_workers: Arc::new(RwLock::new(CoordinatorRegistry::default())),
				detached_tasks: Arc::new(RwLock::new(DetachedTaskRegistry::default())),
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

	/// Kill a live MCP server connection, if any. Called by the
	/// `coder_mcp_*` commands when the user disables or removes a
	/// server so the child process doesn't linger; the next enable
	/// + call respawns it fresh. Idempotent.
	pub async fn mcp_drop_connection(&self, id: &str) {
		self.state.tools.mcp().drop_connection(id).await;
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

	/// Replace the rate-limit rotation chain (see
	/// [`CoderModels::rotation`]). Empty and whitespace-only slugs
	/// are dropped at this boundary so a sloppy comma list can't
	/// produce a fallback to model "".
	pub async fn set_rotation(&self, rotation: Vec<String>) {
		let cleaned: Vec<String> = rotation
			.into_iter()
			.map(|s| s.trim().to_owned())
			.filter(|s| !s.is_empty())
			.collect();
		let mut m = self.state.models.write().await;
		m.rotation = std::sync::Arc::new(cleaned);
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

	/// Warm the catalog-derived caches ([`CoderModels::context_windows`]
	/// and [`CoderModels::vision`]) for the active route without
	/// waiting for the picker to open.
	pub async fn prime_context_windows(&self) {
		let route = self.state.models.read().await.resolve_route();
		match route {
			ResolvedProvider::HuggingFace => match self.state.inference.list_hf_models().await {
				Ok(catalog) => {
					let windows = models::context_windows_from_catalog(&catalog);
					let vision = models::vision_from_catalog(&catalog);
					{
						let mut m = self.state.models.write().await;
						m.context_windows = models::merge_context_windows(&m.context_windows, windows);
						m.vision = models::merge_vision(&m.vision, vision);
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
				let (usage, cache_stats) = {
					let session = rt.session.lock().await;
					match session.last_usage {
						Some(u) => (u, session.cache_stats),
						None => continue,
					}
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
					session_cache_hits: cache_stats.hits,
					session_requests: cache_stats.requests,
					model: active_model.clone(),
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
		let vision = models::vision_from_catalog(&catalog);
		let mut m = self.state.models.write().await;
		m.context_windows = models::merge_context_windows(&m.context_windows, windows);
		m.vision = models::merge_vision(&m.vision, vision);
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
		let vision = models::vision_from_provider_catalog(&catalog);
		if !windows.is_empty() || !vision.is_empty() {
			let mut m = self.state.models.write().await;
			m.context_windows = models::merge_context_windows(&m.context_windows, windows);
			m.vision = models::merge_vision(&m.vision, vision);
		}
		Ok(catalog)
	}

	/// OpenRouter credit status (account balance + per-key
	/// usage/cap) for a configured provider. Gated on
	/// `ProviderKind::OpenRouter` — the `/key` and `/credits`
	/// endpoints are OpenRouter's management API, not part of the
	/// OpenAI-compat surface — and on a stored key, since the
	/// endpoints authenticate with the inference key itself.
	pub async fn openrouter_credits(
		&self,
		provider_id: &str,
	) -> Result<moon_protocol::coder_models::OpenRouterCredits, CoderError> {
		let snapshot = self.state.models.read().await;
		let entry = snapshot
			.providers
			.iter()
			.find(|p| p.id == provider_id)
			.ok_or_else(|| CoderError::Internal(format!("unknown provider id: {provider_id}")))?;
		if entry.kind != moon_protocol::coder_models::ProviderKind::OpenRouter {
			return Err(CoderError::Internal(format!(
				"provider {provider_id} is not an OpenRouter provider"
			)));
		}
		let base_url = entry.base_url.clone();
		drop(snapshot);
		let api_key = self
			.state
			.provider_keys
			.get(provider_id)
			.ok_or_else(|| CoderError::Internal(format!("no API key configured for provider {provider_id}")))?;
		self.state.inference.openrouter_credits(&base_url, &api_key).await
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
				let target =
					crate::tools::resolve_bash_target(&self.state.workspaces, &self.state.workspaces_dir, force_host, &folder)
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

	/// Session ids in `folder` (or the active folder when `folder`
	/// is `None`) that currently have a turn in flight — the set
	/// the companion's session list lights its "running" pip from.
	/// The phone seeds this on workspace open / refresh: its pip is
	/// otherwise purely event-driven, so sessions already running
	/// when the phone subscribes (or a queued steer that never
	/// emitted a live `user_message`) would never light.
	pub async fn running_sessions_in(&self, folder: Option<&str>) -> Vec<String> {
		let Ok((fs, _)) = self.state.folder_session_or_active(folder).await else {
			return Vec::new();
		};
		let runtimes: Vec<(String, Arc<SessionRuntime>)> = fs
			.runtimes
			.read()
			.await
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		let mut running = Vec::new();
		for (id, rt) in runtimes {
			if rt.turn.lock().await.cancel.is_some() {
				running.push(id);
			}
		}
		running
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
	/// Coder-root folder path for the current active folder —
	/// worktree folders resolve to their parent project root, same
	/// routing every session command uses. `None` when no folder
	/// is active. Exposed so the `coder_open_session` command can
	/// build the last-session pointer key from the opened
	/// summary's own worktree context instead of whatever folder
	/// happens to be active once the open settles.
	pub async fn coder_root_path(&self) -> Option<String> {
		let (_, folder_path) = self.state.active_folder_session().await.ok()?;
		Some(folder_path.into_string())
	}

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
	/// Empty when the folder has none. With no folder bound, lists
	/// the workspace's scratch sessions (filed under the home-rooted
	/// slug).
	pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CoderError> {
		self.list_sessions_in(None).await
	}

	/// Folder-targeted [`Self::list_sessions`]: `folder` is a bound
	/// folder's path (the companion's project switcher), `None`
	/// lists the active folder's. Per-project scoping (ADR 0028):
	/// a worktree folder's sessions live under its parent project
	/// root, so list against that.
	pub async fn list_sessions_in(&self, folder: Option<&str>) -> Result<Vec<SessionSummary>, CoderError> {
		let folder_root = match folder {
			Some(path) => self.state.coder_root_at(path).await?,
			None => match self.state.coder_root_folder().await {
				Some(f) => Utf8PathBuf::from(f.folder.path.clone()),
				// Empty workspace: scratch sessions live under the
				// home-rooted slug (a missing dir lists as empty).
				None => self.state.no_folder_root().await?,
			},
		};
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		let mut summaries = sessions::list_sessions(&dir).await?;
		// A live turn writes the same partial record shapes the
		// `interrupted` fold keys on — clear the flag for sessions
		// whose turn is running right now, so the badge means
		// "died mid-turn", never "working".
		for summary in &mut summaries {
			if !summary.interrupted {
				continue;
			}
			if let Some((rt, _)) = self.state.runtime_for_session(&summary.id).await {
				if rt.turn.lock().await.cancel.is_some() {
					summary.interrupted = false;
				}
			}
		}
		Ok(summaries)
	}

	/// Search the active folder's on-disk sessions for a
	/// case-insensitive substring across titles and transcript
	/// text. Returns matching session ids — the panel filters its
	/// already-loaded summaries client-side, so re-serialising
	/// full summaries here would be wasted work. Same per-project
	/// scoping as [`Self::list_sessions`].
	pub async fn search_sessions(&self, query: &str) -> Result<Vec<String>, CoderError> {
		let folder_root = match self.state.coder_root_folder().await {
			Some(folder) => Utf8PathBuf::from(folder.folder.path.clone()),
			None => self.state.no_folder_root().await?,
		};
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		sessions::search_sessions(&dir, query).await
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
		let folder_root = match self.state.coder_root_folder().await {
			Some(folder) => Utf8PathBuf::from(folder.folder.path.clone()),
			None => self.state.no_folder_root().await?,
		};
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

	/// Folder-targeted [`Self::new_session`] for the bridge: allocate
	/// the blank session under the named bound folder instead of the
	/// active one, and **don't** make it the folder's visible session
	/// — the phone tracks its own open session (`send_to` targets it
	/// by id) and must not hijack what the desktop is looking at.
	pub async fn new_session_in(&self, folder: Option<&str>) -> Result<SessionSummary, CoderError> {
		let (fs, _) = self.state.folder_session_or_active(folder).await?;
		let blank = Session::new_blank();
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id, rt).await;
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

	/// Folder-targeted [`Self::new_coordinator_session`] for the
	/// bridge: allocate the coordinator session under the named
	/// bound folder, mount its runtime, and **don't** make it the
	/// folder's visible session — the phone tracks its own open
	/// session and must not hijack what the desktop is looking at.
	pub async fn new_coordinator_session_in(&self, folder: Option<&str>) -> Result<SessionSummary, CoderError> {
		let (fs, _) = self.state.folder_session_or_active(folder).await?;
		let blank = Session::new_blank_with_mode(CoderMode::Coordinator);
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id, rt).await;
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
	/// spec (fresh `moon/<name>` or `moon/agent-<id>` off HEAD, or
	/// check out an existing `base_branch`), derive the worktree path
	/// under `<parent>/.worktrees/<branch-slug>`, `git worktree add`,
	/// bind it as a nested folder, and mint a session (filed under the
	/// parent) whose tools route to the worktree.
	///
	/// `branch_name` names the fresh branch (ADR 0042): a coordinator
	/// passes the worker's `name` so the branch / worktree / session
	/// chip read as `moon/fix-login-redirect` instead of an opaque
	/// `moon/agent-1a2b3c4d`. Slugged and de-duplicated against the
	/// parent's existing branches + worktree dirs. `None` (the UI
	/// path — nobody has described the work yet) keeps the timestamp
	/// default. Ignored when `base_branch` is given: that session
	/// continues an existing branch, which already has a name.
	///
	/// `mode` selects the new session's top-level mode — `Agent` for
	/// an ordinary worker (the common case), `Coordinator` for a
	/// sub-orchestrator.
	///
	/// `parent_folder` pins the project the worktree is created off
	/// and the session list the worker is filed under. `None` means
	/// UI-driven: resolve the active folder at call time and set the
	/// new session visible so the panel opens it (the historical
	/// Tauri-command behaviour). `Some(path)` means agent-driven
	/// (`spawn_worker`): the coordinator's own folder is used, and
	/// the visible session is left alone — re-reading the active
	/// folder here would file the worker under whatever project the
	/// user happens to be looking at mid-turn, and flipping
	/// visibility would silently redirect the user's composer to the
	/// worker (coder.md: tools close over the session's bound folder,
	/// not the live active folder).
	pub async fn create_worktree_session(
		&self,
		base_branch: Option<String>,
		branch_name: Option<String>,
		mode: CoderMode,
		parent_folder: Option<String>,
	) -> Result<(SessionSummary, moon_protocol::workspace::Workspace), CoderError> {
		use moon_core::host::WorktreeBranch;
		let ui_driven = parent_folder.is_none();
		let parent = match parent_folder {
			Some(path) => self
				.state
				.workspaces
				.folder_for_path(&path)
				.await
				.ok_or_else(|| CoderError::Internal(format!("no bound folder at `{path}`")))?,
			None => self.state.workspaces.require_active_folder().await?,
		};
		let parent_path = parent.folder.path.clone();
		let name_slug = branch_name.as_deref().and_then(worker_branch_slug);
		let spec = match base_branch.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
			Some(existing) => WorktreeBranch::Existing(existing.to_string()),
			None => WorktreeBranch::New(match &name_slug {
				Some(slug) => free_worker_branch(&parent, &parent_path, slug).await,
				None => {
					let now_ms = std::time::SystemTime::now()
						.duration_since(std::time::UNIX_EPOCH)
						.map(|d| d.as_millis())
						.unwrap_or(0) as u64;
					format!("moon/agent-{:08x}", now_ms & 0xffff_ffff)
				}
			}),
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
		// `Agent`. Filed under the resolved parent's coder root —
		// never re-read from the active folder, which may have
		// changed since `parent` was resolved.
		let root = self.state.coder_root_of(parent).await;
		let fs = self.state.folder_session_for(Utf8Path::new(&root.folder.path)).await;
		let mut blank = Session::new_blank_with_mode(mode);
		// A named worker is titled after its name (ADR 0042): the
		// sessions-list row reads `fix-login-redirect` from the moment
		// it spawns, matching its branch chip, instead of a truncated
		// task blob that a cheap-model title replaces a turn later.
		// A pre-set title also suppresses the auto-rename — the
		// coordinator already named this work.
		if let Some(slug) = name_slug {
			blank.header.title = slug.clone();
			// Embed the worker's name in the session id too, so a dispatch
			// packet / `list_workers` row / sessions-dir `ls` reads
			// `sess-fix-login-redirect-…` instead of an opaque timestamp id.
			// The id stays unique (timestamp + random suffix) and remains
			// the stable tool / registry key; the name is for readability.
			blank.header.id = new_named_session_id(&slug);
		}
		blank.header.worktree_root = Some(wt_entry.folder.path.clone());
		blank.header.worktree_branch = Some(branch);
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id.clone(), rt).await;
		if ui_driven {
			fs.set_visible(id).await;
		}
		let workspace = self.state.workspaces.snapshot().await;
		Ok((summary, workspace))
	}

	/// Mint a worker session that runs **in place** — directly
	/// against an existing bound folder, with no worktree and no
	/// branch of its own (`spawn_worker` with `worktree: false`,
	/// ADR 0070). The session is filed under the folder's coder
	/// root exactly like a worktree worker's would be; when the
	/// target folder *is* a worktree (a follow-up worker inside an
	/// existing worker's checkout), the worktree routing header is
	/// stamped so tools route there. Agent-driven only — the
	/// visible session is never touched.
	async fn create_in_place_worker_session(
		&self,
		name: &str,
		parent_folder: &str,
		mode: CoderMode,
	) -> Result<SessionSummary, CoderError> {
		let parent = self
			.state
			.workspaces
			.folder_for_path(parent_folder)
			.await
			.ok_or_else(|| CoderError::Internal(format!("no bound folder at `{parent_folder}`")))?;
		let slug = worker_branch_slug(name).ok_or_else(|| {
			CoderError::invalid_args(
				"spawn_worker",
				"name must contain at least one letter or digit — it becomes the worker's session title",
			)
		})?;
		let root = self.state.coder_root_of(parent.clone()).await;
		let fs = self.state.folder_session_for(Utf8Path::new(&root.folder.path)).await;
		let mut blank = Session::new_blank_with_mode(mode);
		// Same naming discipline as a worktree worker (ADR 0042):
		// title + readable session id from the name; a pre-set
		// title suppresses the auto-rename.
		blank.header.title = slug.clone();
		blank.header.id = new_named_session_id(&slug);
		if let moon_protocol::workspace::FolderOrigin::Worktree { branch, .. } = &parent.folder.origin {
			blank.header.worktree_root = Some(parent.folder.path.clone());
			blank.header.worktree_branch = Some(branch.clone());
		}
		let summary = blank.summary();
		let id = blank.header.id.clone();
		let rt = Arc::new(SessionRuntime::new(blank));
		fs.insert_runtime(id, rt).await;
		Ok(summary)
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
		// Flip the live flag too: an in-flight turn's `ToolContext`
		// shares it, so the toggle applies to the next tool dispatch
		// rather than the next turn.
		rt.force_host_bash
			.store(force_host, std::sync::atomic::Ordering::Relaxed);
		// MCP servers follow the bash target (ADR 0033). A live
		// connection is bound to wherever it spawned, so it can't
		// migrate — kill them all and let the next call respawn on
		// the new target. Cost: a playwright browser session is
		// lost mid-task. That's the toggle's semantics; silently
		// driving a browser in the *old* environment is worse.
		self.state.tools.mcp().drop_all_connections().await;
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

	/// Clear `worktree_root` + `worktree_branch` on every mounted
	/// runtime whose header points at `worktree_root`, persist the
	/// header rewrite, and emit `SessionWorktreeCleared` so the
	/// frontend patches its cached session state without a full
	/// reload. Used after merging a worktree's branch and removing
	/// the checkout — the session now drives its parent folder's
	/// main tree, so the worktree routing must be dropped before
	/// the next turn.
	///
	/// Walks every `FolderSession` in `sessions_by_folder` (the
	/// matching sessions are filed under the parent project root,
	/// but we don't assume that here). Best-effort: a failed
	/// header rewrite logs and continues.
	pub async fn clear_worktree_sessions(&self, worktree_root: &str) {
		let folders: Vec<(Utf8PathBuf, Arc<FolderSession>)> = self
			.state
			.sessions_by_folder
			.read()
			.await
			.iter()
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		for (folder_path, fs) in folders {
			let runtimes: Vec<Arc<SessionRuntime>> = fs.runtimes.read().await.values().cloned().collect();
			for rt in runtimes {
				let (id, session_dir, needs_clear) = {
					let mut session = rt.session.lock().await;
					if session.header.worktree_root.as_deref() == Some(worktree_root) {
						session.header.worktree_root = None;
						session.header.worktree_branch = None;
						(
							session.header.id.clone(),
							session.session_dir.clone(),
							session.header.clone(),
						)
					} else {
						continue;
					}
				};
				if let Some(dir) = session_dir {
					if let Err(err) = sessions::rewrite_header(&dir, &needs_clear).await {
						tracing::warn!(?err, "failed to clear worktree routing on session");
					}
				}
				let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), id.clone());
				sink.send(CoderEvent::SessionWorktreeCleared { id });
			}
		}
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

	/// Session-targeted [`Self::revert_to_message`] for the phone
	/// (bridge RPC). Resolves the runtime by `session_id` instead of
	/// the desktop's visible pointer, and reloads through the
	/// observe path (`focus = false`) so the desktop's panel and
	/// visible-session pointer are untouched. The phone re-opens the
	/// session afterwards (`coder_open_session`) to repaint its
	/// transcript from the truncated JSONL.
	pub async fn revert_to_message_in(
		&self,
		session_id: &str,
		user_ordinal: usize,
	) -> Result<RevertedMessage, CoderError> {
		let (rt, folder_path) = self
			.state
			.runtime_for_session(session_id)
			.await
			.ok_or_else(|| CoderError::Internal(format!("session `{session_id}` is not mounted")))?;
		// Refuse mid-turn — same guard as the desktop path.
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

		// Drop the mounted runtime so the reload rebuilds from the
		// now-shorter JSONL, then remount via the observe path (no
		// visible steal, no broadcast).
		{
			let (fs, _) = self.state.folder_session_or_active(Some(folder_path.as_str())).await?;
			fs.runtimes.write().await.remove(session_id);
		}
		self
			.open_session_impl(Some(folder_path.as_str()), session_id.to_owned(), false, None)
			.await?;

		Ok(RevertedMessage {
			text: reverted.dropped_text,
			images: reverted.dropped_images,
		})
	}

	/// [`Self::revert_to_message_in`] addressed from the **end** of
	/// the transcript: `from_end = 0` is the last user message,
	/// `1` the one before it, and so on. The companion's windowed
	/// transcript can't compute an absolute ordinal (its window
	/// clips the head, and counting rows in the window silently
	/// undercounts — a bug that truncated a 3000-message session
	/// at message ~70). The window always includes the tail, so a
	/// from-end index is exact; the translation to the absolute
	/// ordinal happens here against the on-disk record count.
	pub async fn revert_to_message_from_end_in(
		&self,
		session_id: &str,
		user_from_end: usize,
	) -> Result<RevertedMessage, CoderError> {
		let (rt, folder_path) = self
			.state
			.runtime_for_session(session_id)
			.await
			.ok_or_else(|| CoderError::Internal(format!("session `{session_id}` is not mounted")))?;
		let header_id = rt.session.lock().await.header.id.clone();
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let total = sessions::count_user_records(&dir, &header_id).await?;
		let ordinal = total
			.checked_sub(1 + user_from_end)
			.ok_or_else(|| CoderError::Internal(format!("from-end index {user_from_end} exceeds {total} user messages")))?;
		self.revert_to_message_in(session_id, ordinal).await
	}

	/// [`Self::resume_from_assistant_in`] anchored on a **tool-call
	/// id** instead of an index: the resume targets the assistant
	/// record that issued `tool_call_id`. Tool-call ids are
	/// persisted and globally unique, so this needs no counting on
	/// either side — the two index-based variants had to agree on
	/// exactly which assistant records were countable (only ones
	/// with tool calls) and drifted, which broke resumes from
	/// scrolled-back history. Frontends should prefer this.
	pub async fn resume_from_tool_call_in(&self, session_id: &str, tool_call_id: &str) -> Result<(), CoderError> {
		let (rt, folder_path) = self
			.state
			.runtime_for_session(session_id)
			.await
			.ok_or_else(|| CoderError::Internal(format!("session `{session_id}` is not mounted")))?;
		let header_id = rt.session.lock().await.header.id.clone();
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let ordinal = sessions::resumable_ordinal_of_tool_call(&dir, &header_id, tool_call_id)
			.await?
			.ok_or_else(|| CoderError::Internal(format!("no assistant message issued tool call `{tool_call_id}`")))?;
		self.resume_from_assistant_in(session_id, ordinal).await
	}

	/// [`Self::resume_from_assistant_in`] addressed from the end of
	/// the transcript, counting **only assistant messages that
	/// carry tool calls** — the same set the absolute ordinal
	/// indexes (`0` = the last one). Same windowing rationale as
	/// [`Self::revert_to_message_from_end_in`].
	pub async fn resume_from_assistant_from_end_in(
		&self,
		session_id: &str,
		assistant_from_end: usize,
	) -> Result<(), CoderError> {
		let (rt, folder_path) = self
			.state
			.runtime_for_session(session_id)
			.await
			.ok_or_else(|| CoderError::Internal(format!("session `{session_id}` is not mounted")))?;
		let header_id = rt.session.lock().await.header.id.clone();
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let total = sessions::count_resumable_assistant_records(&dir, &header_id).await?;
		let ordinal = total.checked_sub(1 + assistant_from_end).ok_or_else(|| {
			CoderError::Internal(format!(
				"from-end index {assistant_from_end} exceeds {total} assistant messages"
			))
		})?;
		self.resume_from_assistant_in(session_id, ordinal).await
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
		self.send(dropped.text, dropped.images).await?;
		Ok(())
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
		let (_, session_id, _) = self.state.active_visible_runtime().await?;
		self.resume_from_assistant_in(&session_id, assistant_ordinal).await
	}

	/// [`Self::resume_from_assistant`] against an explicit session —
	/// the bridge path, where the phone drives a session the desktop
	/// isn't showing.
	pub async fn resume_from_assistant_in(&self, session_id: &str, assistant_ordinal: usize) -> Result<(), CoderError> {
		self.ensure_can_send().await?;
		let (rt, folder_path) = self
			.state
			.runtime_for_session(session_id)
			.await
			.ok_or_else(|| CoderError::Internal(format!("session `{session_id}` is not mounted")))?;
		let session_id = session_id.to_owned();
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
			cache_stats,
		} = Self::rebuild_messages_from_records(&records);

		// Mutate the existing runtime in place. We keep the same
		// `Arc<SessionRuntime>` so `abort` / `send` / the turn loop
		// all target the same `rt` — no stale-runtime split.
		{
			let mut session = rt.session.lock().await;
			session.messages = messages;
			session.last_usage = last_usage;
			session.cache_stats = cache_stats;
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
			worktree_root: header.worktree_root.clone(),
			worktree_branch: header.worktree_branch.clone(),
			committed_branch: header.committed_branch.clone(),
			mode: header.mode.clone(),
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
					ref worktree_root,
					ref worker,
					ref detached,
				} => {
					// The resume path refuses mid-turn (guard above), so
					// no sub-agent can still be running here.
					replay_subagent_spawned(
						&mut replay_events,
						&subagent_session_dir(&dir, &header.id),
						tool_call_id.clone(),
						subagent_id.clone(),
						target_folder.clone(),
						mode.clone(),
						worktree_root.clone(),
						*worker,
						*detached,
						false,
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

	/// Re-run the round-trip that failed, without re-typing the
	/// prompt. Anchored on the visible session's live state rather
	/// than an ordinal: the transcript's tail *is* the checkpoint —
	/// everything the turn completed before it died is already
	/// persisted, so retrying is just "call the model again with
	/// the messages we have".
	///
	/// Nothing is truncated. The `Error` record stays on disk and
	/// the error row stays in the transcript: it happened, and the
	/// retry's output appends below it (the panel only offers the
	/// affordance on a *trailing* error row, so a successful retry
	/// retires the button by pushing rows past it). Orphan recovery
	/// runs first because the failure path — unlike abort — leaves
	/// any mid-dispatch tool calls unpaired in `messages`, and the
	/// providers reject a request with an unanswered `tool_use`.
	pub async fn retry_last_turn(&self) -> Result<(), CoderError> {
		let (_, session_id, _) = self.state.active_visible_runtime().await?;
		self.retry_last_turn_in(&session_id).await
	}

	/// [`Self::retry_last_turn`] targeting a session by id — the
	/// phone's retry affordance, which addresses sessions directly
	/// rather than through "the visible one".
	pub async fn retry_last_turn_in(&self, session_id: &str) -> Result<(), CoderError> {
		self.ensure_can_send().await?;
		let Some((rt, folder_path)) = self.state.runtime_for_session(session_id).await else {
			return Err(CoderError::Internal(format!(
				"no mounted runtime for session {session_id}"
			)));
		};
		let session_id = session_id.to_string();
		{
			// Refuse mid-turn — same guard as `resume_from_assistant`.
			let turn = rt.turn.lock().await;
			if turn.cancel.is_some() {
				return Err(CoderError::Internal(
					"cannot retry while a turn is running; stop it first".into(),
				));
			}
		}
		if rt.session.lock().await.messages.is_empty() {
			return Err(CoderError::Internal(
				"nothing to retry: the session has no messages".into(),
			));
		}
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id);
		recover_in_memory_orphans(&rt, &sink).await;
		rt.session.lock().await.header.updated_at_ms = current_time_ms();
		let cancel = CancellationToken::new();
		{
			let mut turn = rt.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}
		let state = self.state.clone();
		spawn_turn_loop(state, rt, sink, folder_path.to_path_buf(), cancel, false, None);
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
		// Route like `run_turn` does: a worktree-backed session
		// reapplies into its worktree checkout, not whatever folder
		// happens to be active (ADR 0028/0040).
		let worktree_root = rt.session.lock().await.header.worktree_root.clone();
		let worktree_folder = match worktree_root {
			Some(root) => self.state.workspaces.folder_for_path(&root).await,
			None => None,
		};
		let cx = match worktree_folder {
			Some(folder) => ToolContext::new(folder, CoderMode::Agent),
			None => self.state.tools.context_for_active(CoderMode::Agent).await?,
		}
		.with_force_host_bash(rt.force_host_bash.clone());
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
		let mut cache_stats = SessionCacheStats::default();
		for record in records {
			match record {
				SessionRecord::WorkerDetached { .. } => {}
				SessionRecord::User { text, images, .. } => {
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
					duration_ms: _,
					images,
				} => {
					messages.push(ChatMessage::Tool {
						tool_call_id: tool_call_id.clone(),
						content: content.clone(),
						images: images.clone(),
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
					let usage = TokenUsage {
						prompt_tokens: *prompt_tokens,
						completion_tokens: *completion_tokens,
						total_tokens: *total_tokens,
						cache_read_input_tokens: *cache_read_input_tokens,
						cache_creation_input_tokens: *cache_creation_input_tokens,
					};
					cache_stats.record(&usage);
					last_usage = Some(usage);
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
				// TurnDiff is a metadata record — it doesn't shape the
				// chat history sent to the model. The diff is a review
				// artifact, not a message.
				SessionRecord::TurnDiff { .. } => {}
			}
		}
		RebuiltMessages {
			messages,
			last_usage,
			last_todos,
			cache_stats,
		}
	}

	pub async fn open_session(&self, id: String) -> Result<SessionSummary, CoderError> {
		let (summary, _) = self.open_session_impl(None, id, true, None).await?;
		Ok(summary)
	}

	/// Observe-open for the bridge (the companion's session view):
	/// load the session from the named bound folder, mount its
	/// runtime (so `send_to` / `abort_session` work), and **return**
	/// the replay instead of broadcasting it. Deliberately does not
	/// touch the folder's visible-session pointer and emits nothing
	/// on the event channel — a phone opening a session must not
	/// hijack the desktop's panel or light background-attention
	/// badges.
	pub async fn observe_session_in(
		&self,
		folder: Option<&str>,
		id: String,
		max_events: Option<usize>,
	) -> Result<ObservedSession, CoderError> {
		let (summary, replay) = self.open_session_impl(folder, id, false, max_events).await?;
		let (events, in_flight, has_more) = replay.unwrap_or_default();
		Ok(ObservedSession {
			summary,
			events,
			in_flight,
			has_more,
		})
	}

	/// Fetch the next-older window of a session's transcript for the
	/// companion's upward-scroll pagination. Replays the window
	/// ending just before the record that produced `before_event_ordinal`
	/// in the full replay, so the phone can prepend it.
	///
	/// Read-only: unlike [`Self::observe_session_in`] this does not
	/// mount a runtime or spawn a resumed prompt; it's a pure disk
	/// replay of a slice. `before_event_ordinal` is the
	/// [`CoderEvent::HistoryWindowStart`] ordinal from the previous
	/// fetch (or `usize::MAX` for the newest window — the same thing
	/// `observe_session_in(.., Some(max))` returns).
	pub async fn session_history_older(
		&self,
		folder: Option<&str>,
		id: String,
		before_event_ordinal: usize,
		max_events: usize,
	) -> Result<HistoryWindow, CoderError> {
		sessions::validate_session_id(&id)?;
		let (_, folder_path) = self.state.folder_session_or_active(folder).await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let LoadedSession {
			records,
			record_timestamps,
			..
		} = sessions::load(&dir, &id).await?;
		// `before_event_ordinal` is an exclusive end in the full
		// event sequence (the start ordinal the previous window
		// reported); replay [0, that) and take its newest max_events.
		let (events, _replayed, total, window_start) = Self::replay_window(
			&dir,
			&id,
			records,
			record_timestamps,
			Some(before_event_ordinal),
			max_events,
			false,
		)
		.await;
		Ok(HistoryWindow {
			events,
			has_more: window_start > 0,
			before_event_ordinal: window_start,
			total_events: total,
		})
	}

	/// Rename a session (the companion's title edit). Sets the
	/// title on the in-memory runtime when mounted and always
	/// persists a `TitleUpdate` record to the JSONL so a re-open
	/// replays it, then broadcasts `SessionTitleUpdated` +
	/// `SessionListChanged` on the folder's event channel — the
	/// desktop panel and any phone subscribed to the workspace see
	/// the new title without a refresh.
	///
	/// The folder is resolved from the optional `folder` target when
	/// given (the phone's project switcher), else from the mounted
	/// runtime (a session the caller opened by id), else the active
	/// folder. Refuses an empty title — the truncated-prompt
	/// fallback is always better than a blank one.
	pub async fn rename_session_in(
		&self,
		folder: Option<&str>,
		id: String,
		title: String,
	) -> Result<SessionSummary, CoderError> {
		sessions::validate_session_id(&id)?;
		let title = title.trim();
		if title.is_empty() {
			return Err(CoderError::Internal("session title cannot be empty".into()));
		}
		// Resolve the owning folder: explicit target first, then a
		// mounted runtime (so an id-only rename finds its folder),
		// else the active folder.
		let (folder_path, mounted) = match folder {
			Some(f) => {
				let (_, path) = self.state.folder_session_or_active(Some(f)).await?;
				let rt = self.state.runtime_for_session(&id).await.map(|(rt, _)| rt);
				(path, rt)
			}
			None => match self.state.runtime_for_session(&id).await {
				Some((rt, path)) => (path, Some(rt)),
				None => {
					let (_, path) = self.state.folder_session_or_active(None).await?;
					(path, None)
				}
			},
		};
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);

		// Update the mounted runtime's header if present; take the
		// header for the disk append from it, else load from disk.
		let header = match &mounted {
			Some(rt) => {
				let mut session = rt.session.lock().await;
				session.header.title = title.to_string();
				session.header.updated_at_ms = current_time_ms();
				session.header.clone()
			}
			None => {
				let LoadedSession { mut header, .. } = sessions::load(&dir, &id).await?;
				header.title = title.to_string();
				header
			}
		};
		sessions::append_record(
			&dir,
			&header,
			&SessionRecord::TitleUpdate {
				title: title.to_string(),
			},
		)
		.await?;

		// Notify every subscriber (desktop panel + observing phones).
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), id.clone());
		sink.send(CoderEvent::SessionTitleUpdated {
			id: id.clone(),
			title: title.to_string(),
		});
		sink.send(CoderEvent::SessionListChanged);

		Ok(SessionSummary {
			id: header.id,
			title: header.title,
			created_at_ms: header.created_at_ms,
			updated_at_ms: header.updated_at_ms,
			worktree_root: header.worktree_root,
			worktree_branch: header.worktree_branch,
			committed_branch: header.committed_branch,
			mode: header.mode,
			last_error: false,
			interrupted: false,
		})
	}

	/// Assemble the replayed event stream for `records`, taking the
	/// newest `max_events` events of the window that ends at
	/// `before_event_ordinal` (`None` = the stream's end).
	///
	/// The window is taken over *events*, not records: the full
	/// record list is replayed through [`emit_replay_events`] (and
	/// the async sub-agent path) into one buffer, then sliced. That
	/// keeps the phone's windows aligned to exactly what the UI
	/// renders, at the cost of materialising the full event vec —
	/// acceptable next to the JSONL parse the open already pays.
	///
	/// Returns `(events, replayed_event_count, total_event_count,
	/// window_start_ordinal)`: `window_start_ordinal` is the index
	/// in the full event sequence where the returned window begins
	/// (0 when the whole transcript fit), and is the
	/// `before_event_ordinal` the next "load older" call passes.
	#[allow(clippy::too_many_arguments)]
	async fn replay_window(
		dir: &Utf8Path,
		session_id: &str,
		records: Vec<SessionRecord>,
		record_timestamps: Vec<i64>,
		before_event_ordinal: Option<usize>,
		max_events: usize,
		in_flight: bool,
	) -> (Vec<CoderEvent>, usize, usize, usize) {
		let mut all: Vec<CoderEvent> = Vec::with_capacity(records.len() + 2);
		// Sub-agents whose `SubagentFinished` record hasn't landed
		// yet are still running when the turn is in flight — their
		// transcripts must not get orphan-error synthesis either.
		let finished_subagent_ids: std::collections::HashSet<&str> = records
			.iter()
			.filter_map(|r| match r {
				SessionRecord::SubagentFinished { subagent_id, .. } => Some(subagent_id.as_str()),
				_ => None,
			})
			.collect();
		let live_subagent_ids: std::collections::HashSet<String> = if in_flight {
			records
				.iter()
				.filter_map(|r| match r {
					SessionRecord::SubagentSpawned { subagent_id, .. }
						if !finished_subagent_ids.contains(subagent_id.as_str()) =>
					{
						Some(subagent_id.clone())
					}
					_ => None,
				})
				.collect()
		} else {
			std::collections::HashSet::new()
		};
		for (record, record_ts) in records.into_iter().zip(record_timestamps) {
			match record {
				SessionRecord::SubagentSpawned {
					ref tool_call_id,
					ref subagent_id,
					ref target_folder,
					ref mode,
					ref worktree_root,
					ref worker,
					ref detached,
				} => {
					let still_running = live_subagent_ids.contains(subagent_id.as_str());
					replay_subagent_spawned(
						&mut all,
						&subagent_session_dir(dir, session_id),
						tool_call_id.clone(),
						subagent_id.clone(),
						target_folder.clone(),
						mode.clone(),
						worktree_root.clone(),
						*worker,
						*detached,
						still_running,
					)
					.await;
				}
				SessionRecord::SubagentFinished {
					subagent_id,
					tokens_used_estimate,
					was_error,
					result_preview: _,
				} => {
					all.push(CoderEvent::SubagentFinished {
						subagent_id,
						tokens_used_estimate,
						was_error,
					});
				}
				other => emit_replay_events(&mut all, other, record_ts),
			}
		}
		let total = all.len();
		let end = before_event_ordinal.map(|o| o.min(total)).unwrap_or(total);
		// Window over the event stream: the newest `max_events`
		// events of `[0, end)`. `start` is where the window begins
		// in the full sequence.
		let window_len = end.min(max_events);
		let start = end - window_len;
		let window: Vec<CoderEvent> = all.into_iter().skip(start).collect();
		let replayed = window.len();
		(window, replayed, total, start)
	}

	/// Shared body of [`Self::open_session`] (focus: broadcast the
	/// replay, set the visible pointer) and
	/// [`Self::observe_session_in`] (return the replay, touch
	/// nothing). Returns the replay payload as `Some` only in
	/// observe mode, carrying `(events, in_flight, has_more)`.
	///
	/// `max_events` (observe mode only) windows the replayed
	/// transcript to its newest `max_events` events — the
	/// companion's open path passes it so a very long session (or
	/// one carrying pasted images) doesn't ship its whole history
	/// over the phone's WS before a single row renders. The window
	/// is taken over *events* (what the UI renders), not records;
	/// the full record list still drives the runtime rebuild, so
	/// the mounted session's `messages` stay complete for the next
	/// turn. `None` replays everything (the desktop's behaviour).
	/// Type-erased [`Self::open_session_impl`] for the fleet-rebuild
	/// task (ADR 0065), which remounts workers from *inside* an
	/// `open_session_impl` call — boxing breaks the recursive opaque-
	/// future cycle that otherwise makes the future's `Send`-ness
	/// unprovable.
	fn open_session_boxed(&self, folder: String, id: String) -> BoxedOpenSession<'_> {
		Box::pin(async move { self.open_session_impl(Some(folder.as_str()), id, false, None).await })
	}

	async fn open_session_impl(
		&self,
		folder: Option<&str>,
		id: String,
		focus: bool,
		max_events: Option<usize>,
	) -> Result<(SessionSummary, Option<(Vec<CoderEvent>, bool, bool)>), CoderError> {
		sessions::validate_session_id(&id)?;
		let (fs, folder_path) = self.state.folder_session_or_active(folder).await?;
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
		// ADR 0065: fold the on-disk fleet for a coordinator remount
		// (below).
		let is_coordinator = CoderMode::from_top_level_wire(header.mode.as_deref()) == CoderMode::Coordinator;
		let worker_fleet: Vec<String> = if is_coordinator {
			fold_worker_fleet(&records)
		} else {
			Vec::new()
		};

		let RebuiltMessages {
			mut messages,
			last_usage,
			last_todos,
			cache_stats,
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
		// A cold restart (`!already_mounted`, below) can resume an
		// interrupted `ask_user` prompt instead of erroring it: the
		// call's args (questions/options) survive in the Assistant
		// record, so re-dispatching it re-parks the prompt and the
		// card re-renders interactive for the user to answer and carry
		// on. Only `ask_user` is safe to re-run this way (it's pure
		// parking — no side effects); any other orphaned tool, or a
		// mixed tail, falls back to the interrupted-result path so we
		// never blindly re-execute `bash` / file writes / sub-agents.
		// Computed only for the cold path; a live (already-mounted)
		// parked prompt is handled by `live_parked_ids` below.
		let resume_ask_user_calls = if already_mounted {
			Vec::new()
		} else {
			sessions::orphaned_ask_user_calls(&records)
		};
		let resume_ask_user_ids: std::collections::HashSet<String> =
			resume_ask_user_calls.iter().map(|c| c.id.clone()).collect();
		for orphan_id in &orphan_tool_call_ids {
			// Skip the ask_user calls we're about to re-dispatch —
			// injecting an "Interrupted" sentinel here would feed the
			// model a stale error result alongside the fresh answer the
			// re-dispatch produces.
			if resume_ask_user_ids.contains(orphan_id) {
				continue;
			}
			messages.push(ChatMessage::Tool {
				tool_call_id: orphan_id.clone(),
				content: sessions::INTERRUPTED_TOOL_RESULT_JSON.to_string(),
				images: Vec::new(),
			});
		}
		// `in_flight` is `true` when this session has a turn still
		// running in the background (already mounted), OR when we're
		// about to spawn a fresh turn to resume a parked `ask_user`
		// prompt the user is reopening into. The frontend uses it to
		// keep the sessions-list "running" / "needs input" badge lit
		// after the user clicks into the session and backs out — the
		// replay's trailing `TurnComplete` terminator would otherwise
		// clear it. It also gates orphan-error synthesis below: a
		// live turn's currently-executing tools (a running `task`
		// sub-agent, a long `bash`, a parked `ask_user` prompt) look
		// like orphans on disk — their `Tool` records aren't written
		// until they finish — but they aren't interrupted, and the
		// running turn will emit the real `ToolResult` when each
		// lands.
		let mut in_flight = false;
		// Undrained steers live on the runtime, not the JSONL, so
		// the disk-driven replay below can't see them. Snapshot them
		// here and re-emit at the tail of the batch, otherwise
		// clicking away from a session with a queued steer and back
		// loses the muted row (and its "go now" / unqueue
		// affordances) even though the backend will still feed the
		// message to the running turn.
		let mut pending_steers: Vec<PendingSteer> = Vec::new();
		if already_mounted {
			if let Some(rt) = fs.runtime(&id).await {
				in_flight = rt.turn.lock().await.cancel.is_some();
				pending_steers = rt.session.lock().await.pending_steers.clone();
			}
		}
		let summary = SessionSummary {
			id: header.id.clone(),
			title: header.title.clone(),
			created_at_ms: header.created_at_ms,
			updated_at_ms: header.updated_at_ms,
			worktree_root: header.worktree_root.clone(),
			worktree_branch: header.worktree_branch.clone(),
			committed_branch: header.committed_branch.clone(),
			mode: header.mode.clone(),
			last_error: false,
			interrupted: false,
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
				cache_stats,
				todos: last_todos,
				pending_steers: Vec::new(),
				last_turn_diff: None,
				elided_images: std::collections::HashSet::new(),
			};
			let rt = Arc::new(SessionRuntime::new(session));
			fs.insert_runtime(id.clone(), rt).await;
		}
		if focus {
			fs.set_visible(id.clone()).await;
		}

		// ADR 0065: the in-memory fleet registry and dispatch feeder
		// die with the process. A coordinator's cold remount rebuilds
		// both from its own records, then quietly remounts surviving
		// workers in the background so its control tools (and the
		// feeder's event filter) work without each worker needing a
		// UI open first. Idempotent: `register` is set-insert, the
		// feeder spawn is guarded, and a live process re-opening the
		// session takes the already-mounted fast path above.
		if !already_mounted && is_coordinator && !worker_fleet.is_empty() {
			let mut spawn_feeder = false;
			{
				let mut registry = self.state.coordinator_workers.write().await;
				for worker in &worker_fleet {
					spawn_feeder |= registry.register(&id, worker);
				}
			}
			if spawn_feeder {
				spawn_dispatch_feeder(self.state.clone(), id.clone());
			}
			let state = self.state.clone();
			let orchestrator_id = id.clone();
			let folder = folder_path.clone();
			let fleet = worker_fleet.clone();
			tokio::spawn(async move {
				let handle = CoderHandle { state };
				for worker in fleet {
					if handle.state.runtime_for_session(&worker).await.is_some() {
						continue;
					}
					// Cross-project workers live under their own
					// project's sessions dir — resolve it rather than
					// assuming the coordinator's.
					let worker_folder = find_session_folder(&handle.state, &worker)
						.await
						.unwrap_or_else(|| folder.clone());
					if let Err(err) = handle
						.open_session_boxed(worker_folder.to_string(), worker.clone())
						.await
					{
						// Deleted / unloadable worker session: drop it
						// from the fleet rather than leaving a ghost the
						// coordinator can list but never reach.
						tracing::warn!(?err, worker = %worker, "fleet rebuild: worker remount failed; unregistering");
						handle
							.state
							.coordinator_workers
							.write()
							.await
							.remove(&orchestrator_id, &worker);
					}
				}
			});
		}

		// Tell the panel to clear + reload, then fan out the
		// records as the same events a live turn would emit.
		// `SessionLoaded` carries the metadata so the sticky
		// header doesn't need a follow-up IPC round trip.
		// Observe mode emits nothing — the desktop panel follows
		// `SessionLoaded`, and a phone-side open must not switch it.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), id.clone());
		if focus {
			sink.send(CoderEvent::SessionLoaded {
				id: summary.id.clone(),
				title: summary.title.clone(),
				created_at_ms: summary.created_at_ms,
				updated_at_ms: summary.updated_at_ms,
				worktree_root: summary.worktree_root.clone(),
				worktree_branch: summary.worktree_branch.clone(),
				committed_branch: summary.committed_branch.clone(),
				mode: summary.mode.clone(),
			});
		}
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
		// When `max_events` is set (the companion's observe path),
		// only the *newest* window of the transcript is replayed —
		// see the fn doc. The replay assembly itself (records →
		// events, sub-agent transcription) is shared with
		// [`Self::session_history_older`] via [`Self::replay_window`].
		let limit = max_events.unwrap_or(usize::MAX);
		let (mut replay_events, _replayed_count, _total_events, window_start) =
			Self::replay_window(&dir, &summary.id, records, record_timestamps, None, limit, in_flight).await;
		let has_more = max_events.is_some() && window_start > 0;
		if has_more {
			// Tell the phone where the visible window starts in the
			// full replay's event sequence, so its "load older" call
			// asks for the slice ending just before it.
			replay_events.insert(
				0,
				CoderEvent::HistoryWindowStart {
					before_event_ordinal: window_start,
				},
			);
		}
		// The window was taken over the *full* event stream; the
		// orphan / usage / terminator events below are appended on
		// top and always belong to the visible tail, so a windowed
		// observe still gets them.
		// Surface every orphan tool call as an errored
		// `ToolResult` event so the panel flips its row from
		// "running" to "error". The synthetic JSON content
		// matches the `{"error": "…"}`-only-key shape that
		// `emit_replay_events` (and the live runtime) treat as
		// `is_error: true`, so the rendering is identical to a
		// genuinely-failed tool.
		//
		// Skipped entirely for a live turn (`in_flight`): its
		// currently-executing tools — a running `task` sub-agent,
		// a long `bash`, a parked `ask_user` prompt — are orphans
		// on disk but not interrupted. Their rows stay in the
		// replayed "running" state and the turn's real `ToolResult`
		// events flip them when each tool lands. Abort persists
		// interrupted sentinels in-process, so a mounted-but-idle
		// session only has genuine orphans here.
		if !in_flight {
			for orphan_id in orphan_tool_call_ids {
				// A cold-resumed `ask_user` prompt is about to be
				// re-dispatched by the spawned turn loop (see below) —
				// it's still waiting for the user, don't error its card.
				if resume_ask_user_ids.contains(&orphan_id) {
					continue;
				}
				replay_events.push(CoderEvent::ToolResult {
					id: orphan_id,
					result: serde_json::json!({ "error": "Interrupted before tool completed." }),
					is_error: true,
					duration_ms: None,
				});
			}
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
			session_cache_hits: cache_stats.hits,
			session_requests: cache_stats.requests,
			model: restore_standard.clone(),
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
		// Filter out `ToolCall` events for the `ask_user` calls we're
		// about to re-dispatch — same reason as `resume_from_assistant`:
		// the re-dispatch emits fresh `ToolCall` events and the panel
		// always pushes a new row (never dedups by id), so leaving the
		// replayed ones in would render duplicate cards.
		if !resume_ask_user_ids.is_empty() {
			replay_events.retain(|event| {
				if let CoderEvent::ToolCall { id, .. } = event {
					!resume_ask_user_ids.contains(id)
				} else {
					true
				}
			});
		}
		// A resumed `ask_user` prompt is a genuine in-flight turn —
		// the spawned loop picks it up within milliseconds. Assert
		// `in_flight` so the frontend keeps the "needs input" / running
		// badge lit instead of the trailing `TurnComplete` clearing it.
		if !resume_ask_user_calls.is_empty() {
			in_flight = true;
		}
		// Queued-but-undrained steers ride at the tail, ahead of the
		// terminator: they're the newest thing in the transcript, and
		// the ids match the live `PendingSteer`s so the eventual
		// `SteerDrained` (and `coder_unqueue_steer` / `drain now`)
		// still target the right row.
		for steer in pending_steers {
			replay_events.push(CoderEvent::UserMessage {
				id: steer.id,
				text: steer.text,
				images: steer.images,
				queued: true,
				created_at_ms: Some(steer.queued_at_ms),
				from_coordinator: steer.from_coordinator,
			});
		}
		replay_events.push(CoderEvent::TurnComplete);
		// Focus: broadcast the batch for the desktop panel's reduce
		// pass. Observe: hand it back to the caller (the bridge ships
		// it in the RPC response) so nothing reaches the desktop.
		let observed_replay = if focus {
			sink.send(CoderEvent::Replay {
				events: replay_events,
				in_flight,
			});
			None
		} else {
			Some((replay_events, in_flight, has_more))
		};
		// Mid-backoff open: re-announce the wait as a *live* event
		// with the remaining time, so a client that arrives during
		// a two-minute sleep sees "retrying in 47s" instead of an
		// unexplained spinner. Live (not part of the replay batch)
		// because the frontends deliberately ignore replayed
		// notices — a stale one from a settled turn must never
		// show. Sent after the replay so it isn't cleared by the
		// batch's own terminator.
		if let Some(retry) = active_retry_for(sink.session_id()) {
			let remaining = retry.until_ms.saturating_sub(current_time_ms()).max(0);
			sink.send(CoderEvent::RetryBackoff {
				model: retry.model,
				status: retry.status,
				attempt: retry.attempt,
				max_attempts: retry.max_attempts,
				delay_ms: u64::try_from(remaining).unwrap_or(0),
				rotated_to: retry.rotated_to,
			});
		}
		// Cold-resume an interrupted `ask_user`: spawn a fresh turn
		// loop whose first iteration re-dispatches the parked prompt.
		// `handle_ask_user` registers a new oneshot on the
		// `PromptRegistry`, the replayed card (re-rendered from the
		// Assistant record above, minus the filtered `ToolCall` — the
		// re-dispatch's fresh one lands momentarily) is already in its
		// interactive "waiting" state, and `coder_respond_to_prompt`
		// resolves the oneshot the moment the user picks an option. The
		// turn then continues exactly as if the prompt had never been
		// interrupted — same code path as `resume_from_assistant`.
		if !resume_ask_user_calls.is_empty() {
			if let Some(rt) = fs.runtime(&id).await {
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
					Some(resume_ask_user_calls),
				);
			}
		}
		Ok((summary, observed_replay))
	}

	/// Delete a persisted session under the active workspace
	/// folder. Idempotent. If the deleted session has a mounted
	/// runtime, cancel its turn (if any) and drop the runtime;
	/// when it was the visible session, fall back to "no visible
	/// session" — the panel reconciles by clearing its bucket and
	/// landing on either the sessions list or a fresh blank
	/// session. Other folders' sessions are untouched.
	pub async fn delete_session(&self, id: String) -> Result<(), CoderError> {
		self.delete_session_in(None, id).await
	}

	/// Folder-targeted [`Self::delete_session`]. Used by the bridge
	/// so the phone's project switcher can delete sessions in any
	/// bound folder.
	pub async fn delete_session_in(&self, folder: Option<&str>, id: String) -> Result<(), CoderError> {
		sessions::validate_session_id(&id)?;
		let (fs, folder_path) = self.state.folder_session_or_active(folder).await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		sessions::delete(&dir, &id).await?;
		// Reap any MCP server instances (headless browsers) the
		// session owned.
		self.state.tools.mcp().drop_session_connections(&id).await;
		// Drop the runtime entry (and cancel its in-flight turn,
		// if any). Other sessions in the same folder keep running.
		let removed = fs.runtimes.write().await.remove(&id);
		if let Some(rt) = removed {
			if let Some(token) = rt.turn.lock().await.cancel.as_ref() {
				token.cancel();
			}
		}
		// Tear down the session's detached sub-agents ([ADR 0053]):
		// cancel live runs and drop cached reports — nothing can
		// collect them once the parent session is gone.
		self.state.detached_tasks.write().await.prune_parent(&id);
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

	pub async fn send(&self, text: String, images: Vec<ImageAttachment>) -> Result<SendTarget, CoderError> {
		// Shrink pasted screenshots before they enter the history —
		// they're re-sent on every subsequent round-trip, so this is
		// the only place it's free (see `crate::images`).
		let images = crate::images::reencode_all(images).await;
		// Bail early if the active route can't authenticate —
		// surface a clean error instead of letting the inference
		// layer fail on the first request. HF needs OAuth; user
		// providers need a configured key (or a localhost
		// `base_url`, where keyless is conventional for Ollama /
		// llama.cpp).
		self.ensure_can_send().await?;
		let (rt, session_id, folder_path) = self.state.active_visible_runtime().await?;
		// Snapshot where this message is going *now*, so callers
		// can persist the "last opened session" pointer against
		// the folder that actually received it — re-reading the
		// active folder after the await races a project switch.
		let target = SendTarget {
			coder_root: folder_path.to_string(),
			worktree_root: rt.session.lock().await.header.worktree_root.clone(),
			session_id: session_id.clone(),
		};
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);

		// A direct user message into a coordinator-spawned worker
		// **tells the coordinator** what the user said (ADR 0043) and
		// changes nothing else — the worker stays hooked up. The
		// turn probe mirrors the steer branch below so the notice's
		// wording tracks delivery (queued vs seen); a race between
		// the two probes only costs wording accuracy.
		let target_turn_running = rt.turn.lock().await.cancel.is_some();
		self
			.notify_coordinator_of_user_message(&session_id, &text, target_turn_running)
			.await;

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
				let queued_at_ms = current_time_ms();
				let mut session = rt.session.lock().await;
				session.pending_steers.push(PendingSteer {
					id: steer_id.clone(),
					text: text.clone(),
					images: images.clone(),
					queued_at_ms,
					from_coordinator: false,
				});
				session.header.updated_at_ms = queued_at_ms;
				drop(session);
				let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id.clone());
				sink.send(CoderEvent::UserMessage {
					id: steer_id,
					text,
					images,
					queued: true,
					created_at_ms: Some(queued_at_ms),
					from_coordinator: false,
				});
				return Ok(target);
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
			// "Nothing on disk yet" is the freshness test, not "no
			// title": a coordinator-spawned worker is pre-titled at
			// creation (ADR 0042) and still has to announce itself.
			let needs_loaded_event = session.persisted_records == 0;
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
				worktree_root: summary.worktree_root.clone(),
				worktree_branch: summary.worktree_branch.clone(),
				committed_branch: summary.committed_branch.clone(),
				mode: summary.mode.clone(),
			});
			sink.send(CoderEvent::SessionListChanged);
		}

		// Older queued steers drain *before* the fresh prompt.
		// Notices parked while no turn was running (worker-message
		// notices on an idle coordinator, ADR 0062) are older than
		// this send; without this the loop-top drain would append
		// them *after* it, so the model, the JSONL, and the
		// transcript would all show the newer message first.
		drain_pending_steers(&rt, &sink).await;

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
				from_coordinator: false,
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
			from_coordinator: false,
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
		Ok(target)
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
		// Cascade to the session's detached sub-agents ([ADR 0053]):
		// they run on their own root tokens precisely so a *turn*
		// end doesn't kill them, but the user hitting "stop" means
		// "stop everything", so we cancel each run's token here.
		for token in self.state.detached_tasks.read().await.live_tokens_of(&id) {
			token.cancel();
		}
		let turn = rt.turn.lock().await;
		if let Some(token) = turn.cancel.as_ref() {
			token.cancel();
		}
	}

	/// "Go now" on a queued steer: the user typed a message mid-
	/// turn (it landed in `pending_steers` and rendered as a
	/// muted "queued" placeholder row), then decided they don't
	/// want to wait for the running turn to settle. Cancels the
	/// current turn (like [`abort`]) and removes the placeholder
	/// row by emitting [`CoderEvent::SteerDrained`]. The spawn
	/// loop's `Err(Aborted)` branch detects the still-pending
	/// steer, recovers orphaned tool-call results, and loops back
	/// into `run_turn`, which drains the steer into chat history
	/// (re-emitting it as a real `UserMessage` at the bottom) and
	/// runs a fresh LLM round-trip. The UI sees an uninterrupted
	/// `busy` stretch: no `Aborted` flash, just the old thinking
	/// fading into the new turn.
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
		self.drain_steer_on(&rt, &session_id, &folder_path, id).await
	}

	/// Session-targeted [`Self::drain_steer_now`] (ADR 0030) — the
	/// companion's "go now", which knows the session by id rather
	/// than "the desktop's visible one". Resolves the runtime by id
	/// across all folders, same as [`Self::send_to`], so a queued
	/// steer on a background session drains too. `false` when no
	/// mounted runtime matches.
	pub async fn drain_steer_now_in(&self, session_id: &str, id: &str) -> bool {
		let Some((rt, folder_path)) = self.state.runtime_for_session(session_id).await else {
			return false;
		};
		self.drain_steer_on(&rt, session_id, &folder_path, id).await
	}

	/// Shared drain-now body: cancel the running turn so the spawn
	/// loop drains the still-pending steer into a fresh turn
	/// immediately. See [`Self::drain_steer_now`] for the full
	/// contract.
	async fn drain_steer_on(&self, rt: &Arc<SessionRuntime>, session_id: &str, folder_path: &Utf8Path, id: &str) -> bool {
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
		// An idle session can hold a parked coordinator notice
		// (ADR 0062). "Go now" on one is a manual wake: claim the
		// turn slot under the lock (so a racing `send` falls into
		// its steer branch instead of double-spawning) and start a
		// fresh turn — its first iteration top drains the queue and
		// re-emits the row as a real `UserMessage`.
		{
			let mut turn = rt.turn.lock().await;
			if turn.cancel.is_none() {
				let cancel = CancellationToken::new();
				turn.cancel = Some(cancel.clone());
				drop(turn);
				let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id);
				spawn_turn_loop(
					self.state.clone(),
					rt.clone(),
					sink,
					folder_path.to_owned(),
					cancel,
					false,
					None,
				);
				return true;
			}
		}
		// Skip any parked `ask_user` prompt so the tool returns
		// and the turn reaches the cancellation point (the
		// iteration boundary's `select!`).
		if rt.prompts.has_pending().await {
			rt.prompts.skip_any().await;
		}
		// Remove the placeholder row now — the spawn loop's drain
		// will re-append it as a real `UserMessage` at the bottom
		// anyway, and the visual transition reads better as
		// "queued → gone → fresh message" the instant the user hits
		// go now.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), session_id);
		sink.send(CoderEvent::SteerDrained { id: id.to_string() });
		// Trip the cancel token: the running `run_turn` returns
		// `Err(Aborted)`, the spawn loop sees the pending steer,
		// recovers orphans, and loops back to drain it. The steer
		// stays in `pending_steers` until that drain — so the queued
		// row the `SteerDrained` above just removed gets re-rendered
		// as the drain emits its fresh `UserMessage`. Between the two
		// the message briefly has no visible row, which reads as the
		// old turn dissolving into the new one.
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
	/// the queued row is removed even if the panel didn't know
	/// about the pop ahead of time (e.g. a sibling window
	/// triggered the unqueue). Unlike the drain path this is a
	/// pure removal — no `UserMessage` follows, because the
	/// message went back into the composer rather than the chat.
	pub async fn unqueue_steer(&self, id: &str) -> Option<UnqueuedSteer> {
		let (rt, session_id, folder_path) = self.state.active_visible_runtime().await.ok()?;
		self.unqueue_steer_on(&rt, &session_id, &folder_path, id).await
	}

	/// Session-targeted [`Self::unqueue_steer`] (ADR 0030) — the
	/// companion's un-queue, which targets the session it has open
	/// by id rather than the desktop's visible one. Resolves the
	/// runtime by id across all folders, same as [`Self::send_to`].
	/// `None` when no mounted runtime matches or the steer already
	/// drained.
	pub async fn unqueue_steer_in(&self, session_id: &str, id: &str) -> Option<UnqueuedSteer> {
		let (rt, folder_path) = self.state.runtime_for_session(session_id).await?;
		self.unqueue_steer_on(&rt, session_id, &folder_path, id).await
	}

	/// Shared un-queue body: pop the matching `PendingSteer`, emit
	/// its `SteerDrained`, and hand the `(text, images)` back for
	/// the composer. See [`Self::unqueue_steer`] for the contract.
	async fn unqueue_steer_on(
		&self,
		rt: &Arc<SessionRuntime>,
		session_id: &str,
		folder_path: &Utf8Path,
		id: &str,
	) -> Option<UnqueuedSteer> {
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

	/// Continue a sub-agent from the pop-out composer, by id
	/// through the process-global steer registry (the sub-agent
	/// runs deep inside its parent's tool dispatch, so there's no
	/// session handle to go through).
	///
	/// - **Running** → mid-flight steer: the text queues, renders
	///   as a muted row immediately, and drains into the
	///   sub-agent's history at the top of its next iteration.
	/// - **Finished** → resume: rebuild the history from the
	///   sub-agent's JSONL and run a fresh detached loop with the
	///   text as a follow-up user message. The parent agent is
	///   deliberately not involved — its history still ends at the
	///   original `task` tool result; follow-ups are user ↔
	///   sub-agent only.
	///
	/// Returns `false` when there's nothing to continue (unknown
	/// id, or no JSONL on disk); the panel keeps the draft.
	pub async fn continue_subagent(&self, subagent_id: &str, text: String) -> Result<bool, CoderError> {
		// Fast path: still running → steer.
		if crate::subagent::queue_subagent_steer(subagent_id, text.clone(), false) {
			return Ok(true);
		}
		sessions::validate_session_id(subagent_id)?;
		let folder_root = match self.state.coder_root_folder().await {
			Some(folder) => Utf8PathBuf::from(folder.folder.path.clone()),
			None => self.state.no_folder_root().await?,
		};
		let slug_dir = sessions_dir(&self.state.coder_sessions_dir, &folder_root);
		let Some(jsonl_path) = sessions::find_subagent_session(&slug_dir, subagent_id).await else {
			return Ok(false);
		};
		let Some(session_dir) = jsonl_path.parent().map(Utf8Path::to_path_buf) else {
			return Ok(false);
		};
		let LoadedSession { header, records, .. } = sessions::load(&session_dir, subagent_id).await?;
		let (Some(parent_session_id), Some(parent_tool_call_id)) =
			(header.parent_session_id.clone(), header.parent_tool_call_id.clone())
		else {
			// Not a sub-agent trace (or a pre-sub-agent schema);
			// nothing to resume.
			return Ok(false);
		};
		let mode = match header.subagent_mode.as_deref() {
			Some(wire) => CoderMode::from_wire(wire)?,
			None => CoderMode::Agent,
		};
		// The sub-agent's tools must land in the same folder they
		// originally operated against; if that folder is no longer
		// bound, refuse rather than silently retargeting. The
		// scratch root resolves without being bound.
		let target = header
			.subagent_target_folder
			.clone()
			.unwrap_or_else(|| header.cwd.clone());
		let Some(target_entry) = self.state.folder_entry_for(&target).await else {
			return Err(CoderError::Internal(format!(
				"sub-agent folder `{target}` is not bound — bind it to follow up with this sub-agent"
			)));
		};
		let task = records
			.iter()
			.find_map(|r| match r {
				SessionRecord::User { text, .. } => Some(text.clone()),
				_ => None,
			})
			.unwrap_or_default();
		let RebuiltMessages {
			mut messages,
			last_usage,
			last_todos,
			cache_stats: _,
		} = Self::rebuild_messages_from_records(&records);
		// The rebuild seeds the parent's system prompt; swap in the
		// sub-agent's own (mode + target folder + original task).
		messages[0] = ChatMessage::System {
			content: crate::subagent::build_subagent_system_prompt(mode, &target_entry, &task),
		};
		let spec = crate::subagent::Subagent {
			id: subagent_id.to_string(),
			parent_session_id: parent_session_id.clone(),
			parent_tool_call_id,
			parent_folder: folder_root.clone(),
			task,
			system_prompt_override: None,
			mode,
			// A user-driven resume re-runs a previously-detached or
			// synchronous sub-agent; the resume itself always runs
			// detached from the caller's perspective (the pop-out
			// owns it), but the *original* detach flag is what the
			// registry recorded at spawn, so we don't restamp here.
			detach: false,
			folder: target_entry,
		};
		// Events land in the parent session's UI bucket, same as
		// the original run.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_root.to_string(), parent_session_id);
		let Some(cancel) = crate::subagent::claim_subagent_for_resume(subagent_id, &text, &sink) else {
			// Raced with a live run — the text went in as an
			// ordinary steer instead.
			return Ok(true);
		};
		let resume = crate::subagent::SubagentResume {
			spec,
			session_dir,
			header,
			messages,
			last_usage,
			todos: last_todos,
			follow_up: text,
		};
		let state = self.state.clone();
		let subagent_id = subagent_id.to_string();
		tokio::spawn(async move {
			let outcome =
				crate::subagent::resume_subagent(&state.tools, &state.inference, &sink, &state.models, resume, cancel).await;
			if let Err(err) = outcome {
				// The pop-out's card already flipped to `error` via
				// `SubagentFinished`; surface the message inside
				// the transcript too (there's no parent tool result
				// to carry it on a user-driven resume).
				if !matches!(err, CoderError::Aborted) {
					sink.send(CoderEvent::SubagentEvent {
						subagent_id: subagent_id.clone(),
						inner: Box::new(CoderEvent::Error {
							message: err.to_string(),
						}),
					});
				}
				tracing::warn!(%err, subagent = %subagent_id, "resumed sub-agent ended with error");
			}
		});
		Ok(true)
	}

	/// Cancel one running sub-agent's own token — the pop-out's
	/// stop button. Never propagates to the parent turn or sibling
	/// sub-agents. Returns `false` when nothing is running under
	/// that id.
	pub fn abort_subagent(&self, subagent_id: &str) -> bool {
		crate::subagent::abort_subagent(subagent_id)
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

	/// Broadcast `WorkspaceFoldersChanged` outside a coder turn — the
	/// bridge's folder-management RPC uses this so every attached UI
	/// (the desktop folder bar, other phones) refreshes its snapshot
	/// after a bind/unbind it didn't initiate.
	pub fn announce_workspace_folders_changed(&self, folder: &str) {
		let sink = FolderEventSink::new(self.state.events.clone(), folder.to_string(), String::new());
		sink.send(CoderEvent::WorkspaceFoldersChanged);
	}

	pub fn subscribe(&self) -> broadcast::Receiver<CoderEventEnvelope> {
		self.state.events.subscribe()
	}

	/// Watch the number of live turn loops in this process. `0 → n`
	/// means "some agent started", `n → 0` means "everything
	/// settled". Consumed by the Tauri layer's OS-level activity
	/// indicator; see the field doc on [`CoderState::running_turns`]
	/// for what does and doesn't count.
	pub fn watch_running_turns(&self) -> watch::Receiver<usize> {
		self.state.running_turns.subscribe()
	}

	// ── Orchestrator-facing client surface (ADR 0030) ───────────
	//
	// By-id variants of the panel-driven methods. An orchestrator
	// session drives its workers by id (not "the visible session"),
	// so it needs `send_to` / `abort_session` / `observe_session`
	// that target a specific runtime regardless of what the user
	// has foregrounded. The visible-session methods above stay
	// unchanged for the UI path.

	/// [`Self::send_to`] for a message the **user** typed (the desktop
	/// composer and the phone's, both of which target a session by id
	/// rather than "the visible one" — ADR 0066). Identical except
	/// that it also tells the coordinator when the target is one of
	/// its workers (ADR 0043). Coordinator-originated traffic uses
	/// plain `send_to`, which stays silent: a coordinator doesn't
	/// need to be told what it just said. Returns the resolved
	/// [`SendTarget`] so the desktop command can persist the
	/// last-opened-session pointer, same as [`Self::send`].
	pub async fn send_to_as_user(
		&self,
		session_id: &str,
		text: String,
		images: Vec<ImageAttachment>,
	) -> Result<SendTarget, CoderError> {
		// Same delivery-tracking probe as `send` — a mid-turn nudge
		// is only queued worker-side, and the notice says so.
		let queued = match self.state.runtime_for_session(session_id).await {
			Some((rt, _)) => rt.turn.lock().await.cancel.is_some(),
			None => false,
		};
		self.notify_coordinator_of_user_message(session_id, &text, queued).await;
		self.send_to_inner(session_id, text, images, false).await
	}

	/// Tell the coordinator that the user just messaged one of its
	/// workers directly, quoting the message (truncated). No-op when
	/// the session isn't a registered worker.
	///
	/// The worker stays hooked up (ADR 0043): the dispatch feeder
	/// keeps forwarding its events and every control tool keeps
	/// working. The notice exists so the coordinator's next turn
	/// accounts for an instruction it didn't issue instead of
	/// contradicting it — and it is **parked, not a wake**
	/// (ADR 0062): it lands in the coordinator's steer queue, so a
	/// running turn drains it at its next iteration boundary and an
	/// idle coordinator holds it until whatever starts its next turn
	/// (a dispatch-packet wake, a direct user message, "go now" on
	/// the queued row). A nudge into a worker is information for the
	/// coordinator's next decision, not worth a turn of its own —
	/// the worker already has the instruction, and its
	/// `TurnComplete` wake follows anyway.
	///
	/// `queued` tracks delivery truthfully: a message into a worker
	/// whose turn is mid-flight only lands in its steer queue, and
	/// the coordinator shouldn't reason as if the worker has already
	/// seen it (it may not for a while — or ever, if the user pops
	/// the queued row).
	async fn notify_coordinator_of_user_message(&self, session_id: &str, text: &str, queued: bool) {
		let trimmed = text.trim();
		if trimmed.is_empty() {
			return;
		}
		let registered = self
			.state
			.coordinator_workers
			.read()
			.await
			.orchestrator_of(session_id)
			.map(str::to_string);
		let orchestrator_id = match registered {
			Some(id) => id,
			// Restart fallback (ADR 0065): the registry died with the
			// process, but the worker's header carries the link.
			// Quietly remount the coordinator — its cold mount
			// rebuilds the fleet (or proves this worker detached) —
			// then re-consult the registry, which now reflects any
			// persisted `WorkerDetached`.
			None => {
				let Some((worker_rt, worker_folder)) = self.state.runtime_for_session(session_id).await else {
					return;
				};
				let Some(orch) = worker_rt.session.lock().await.header.orchestrator_session_id.clone() else {
					return;
				};
				if self.state.runtime_for_session(&orch).await.is_none() {
					let orch_folder = find_session_folder(&self.state, &orch)
						.await
						.unwrap_or_else(|| worker_folder.clone());
					let _ = self.open_session_boxed(orch_folder.to_string(), orch.clone()).await;
				}
				match self
					.state
					.coordinator_workers
					.read()
					.await
					.orchestrator_of(session_id)
					.map(str::to_string)
				{
					Some(id) => id,
					None => return,
				}
			}
		};
		let notice = if queued {
			format!(
				"The user queued a message for worker {session_id} (its turn is mid-flight; the worker \
				 picks it up at its next iteration boundary — it has NOT seen it yet): \"{}\"\n\n\
				 Nothing else changed — its updates keep reaching you and your control tools still \
				 work on it.",
				truncate_for_notice(trimmed, USER_MESSAGE_NOTICE_MAX)
			)
		} else {
			format!(
				"The user sent worker {session_id} a message directly: \"{}\"\n\n\
				 Nothing else changed — its updates keep reaching you and your control tools still \
				 work on it.",
				truncate_for_notice(trimmed, USER_MESSAGE_NOTICE_MAX)
			)
		};
		// Failure (orchestrator unmounted / deleted) only costs the
		// notice.
		let Some((rt, folder_path)) = self.state.runtime_for_session(&orchestrator_id).await else {
			tracing::warn!(
				orchestrator_id = %orchestrator_id,
				"coordinator unmounted; dropping user-message notice"
			);
			return;
		};
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string(), orchestrator_id);
		park_coordinator_notice(&rt, &sink, notice).await;
	}

	/// Send a prompt to a specific session by id (ADR 0030). Unlike
	/// `send` (which targets the active folder's visible session),
	/// this resolves the runtime by id across all folders and seeds
	/// it directly. If the target's turn is already running, the
	/// message is queued as a steer (same as a user steering a
	/// visible session).
	pub async fn send_to(&self, session_id: &str, text: String, images: Vec<ImageAttachment>) -> Result<(), CoderError> {
		self.send_to_inner(session_id, text, images, false).await.map(|_| ())
	}

	/// [`Self::send_to`] for coordinator → worker traffic
	/// (`spawn_worker`'s seed task, `steer_worker`). Identical
	/// mechanics, but the emitted `UserMessage` and the persisted
	/// record carry `from_coordinator: true` so the worker's
	/// transcript badges the orchestrator's instructions apart
	/// from anything the human typed (ADR 0043 lets both land in
	/// the same session). Coordinator-*bound* traffic isn't the
	/// coordinator speaking: dispatch feeder wakes stay on plain
	/// `send_to`, and user-message notices park in the steer queue
	/// ([`park_coordinator_notice`], ADR 0062).
	pub async fn send_to_as_coordinator(
		&self,
		session_id: &str,
		text: String,
		images: Vec<ImageAttachment>,
	) -> Result<(), CoderError> {
		self.send_to_inner(session_id, text, images, true).await.map(|_| ())
	}

	async fn send_to_inner(
		&self,
		session_id: &str,
		text: String,
		images: Vec<ImageAttachment>,
		from_coordinator: bool,
	) -> Result<SendTarget, CoderError> {
		let images = crate::images::reencode_all(images).await;
		self.ensure_can_send().await?;
		let Some((rt, folder_path)) = self.state.runtime_for_session(session_id).await else {
			return Err(CoderError::Internal(format!(
				"no mounted runtime for session {session_id}"
			)));
		};
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		// Snapshot the resolved target (mirrors `send`) so callers
		// can persist the last-opened-session pointer against the
		// folder that actually received the message.
		let target = SendTarget {
			coder_root: folder_path.to_string(),
			worktree_root: rt.session.lock().await.header.worktree_root.clone(),
			session_id: session_id.to_string(),
		};
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
				let queued_at_ms = current_time_ms();
				let mut session = rt.session.lock().await;
				session.pending_steers.push(PendingSteer {
					id: steer_id.clone(),
					text: text.clone(),
					images: images.clone(),
					queued_at_ms,
					from_coordinator,
				});
				session.header.updated_at_ms = queued_at_ms;
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
					created_at_ms: Some(queued_at_ms),
					from_coordinator,
				});
				return Ok(target);
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
			// "Nothing on disk yet" is the freshness test, not "no
			// title": a coordinator-spawned worker is pre-titled at
			// creation (ADR 0042) and still has to announce itself.
			let needs_loaded_event = session.persisted_records == 0;
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
		// Chronology: older parked steers land before this message —
		// same reasoning as the identical call in `send`.
		drain_pending_steers(&rt, &sink).await;
		// Append the user message to in-memory chat history + the
		// session JSONL, mirroring `send`'s persist path. Persisted
		// **before** the `SessionListChanged` announce below: the
		// frontend reacts to that event by re-reading the on-disk
		// session list, and a worker whose seed record hadn't landed
		// yet would be missing from the refreshed list (no row, no
		// running pip) until some unrelated event refreshed it again.
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
				from_coordinator,
			};
			if let Err(err) = sessions::append_record(&dir, &header, &record).await {
				tracing::warn!(error = %err, "failed to persist worker seed message");
			} else {
				let mut session = rt.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}
		if let Some(summary) = &summary_to_announce {
			sink.send(CoderEvent::SessionLoaded {
				id: summary.id.clone(),
				title: summary.title.clone(),
				created_at_ms: summary.created_at_ms,
				updated_at_ms: summary.updated_at_ms,
				worktree_root: summary.worktree_root.clone(),
				worktree_branch: summary.worktree_branch.clone(),
				committed_branch: summary.committed_branch.clone(),
				mode: summary.mode.clone(),
			});
			sink.send(CoderEvent::SessionListChanged);
		}
		sink.send(CoderEvent::UserMessage {
			id: new_message_id(),
			text,
			images,
			queued: false,
			created_at_ms: Some(current_time_ms()),
			from_coordinator,
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
		Ok(target)
	}

	/// Abort a specific session's in-flight turn by id (ADR 0030).
	/// No-op when the session isn't mounted or has no turn running.
	/// Used by an orchestrator's `abort_worker`.
	pub async fn abort_session(&self, session_id: &str) {
		let Some((rt, _)) = self.state.runtime_for_session(session_id).await else {
			return;
		};
		// Same detached cascade as `abort` ([ADR 0053]).
		for token in self.state.detached_tasks.read().await.live_tokens_of(session_id) {
			token.cancel();
		}
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
		// The parked `ask_user`'s questions, recovered from the
		// assistant message that raised it. The prompt registry only
		// holds the call id + oneshot; the args (question ids, option
		// ids) live on the transcript, and the coordinator needs them
		// to key `respond_to_worker_prompt` answers.
		let pending_prompt = if needs_input {
			match rt.prompts.pending_call_id().await {
				Some(call_id) => session.messages.iter().rev().find_map(|m| match m {
					ChatMessage::Assistant { tool_calls, .. } => tool_calls
						.iter()
						.find(|c| c.id == call_id)
						.and_then(|c| serde_json::from_str(&c.function.arguments).ok()),
					_ => None,
				}),
				None => None,
			}
		} else {
			None
		};
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
			pending_prompt,
			last_assistant,
			last_diff: session.last_turn_diff.as_ref().map(|(files, diff)| TurnDiffSummary {
				files: files
					.iter()
					.map(|path| {
						let (added, removed) = count_diff_lines_for_file(diff, path);
						TurnDiffFileSummary {
							path: path.clone(),
							added,
							removed,
						}
					})
					.collect(),
			}),
		})
	}

	/// Whether `session_id` is registered as a coordinator-spawned
	/// worker — attached or already disconnected (ADR 0052). Drives
	/// the session-bar disconnect affordance, which must also reach
	/// an already-disconnected worker so a second click can end its
	/// current turn.
	pub async fn is_coordinator_worker(&self, session_id: &str) -> bool {
		self.state.coordinator_workers.read().await.is_worker(session_id)
	}

	/// Unhook a coordinator-spawned worker from its orchestrator
	/// (ADR 0052). The session itself is never touched: it keeps its
	/// transcript, branch, and worktree, and its in-flight turn (if
	/// any) runs to completion — the feeder drops the link fully once
	/// that final turn lands and tells the orchestrator. When the
	/// worker is idle the orchestrator hears it right away instead.
	///
	/// Clicking the affordance a second time (worker already
	/// disconnected) is the "stop it now" path and cancels the
	/// in-flight turn. A session no coordinator spawned returns
	/// [`DisconnectWorkerOutcome::NotAWorker`] and is left alone.
	pub async fn disconnect_worker(&self, session_id: &str) -> DisconnectWorkerOutcome {
		let orchestrator_id = self
			.state
			.coordinator_workers
			.read()
			.await
			.owning_orchestrator_of(session_id)
			.map(str::to_string);
		let Some(orchestrator_id) = orchestrator_id else {
			return DisconnectWorkerOutcome::NotAWorker;
		};
		let freshly_cut = self
			.state
			.coordinator_workers
			.write()
			.await
			.disconnect(&orchestrator_id, session_id);
		if !freshly_cut {
			// Second click: already unhooked. If its turn is still
			// running, this is the user's "stop it now" — cancel it.
			// The feeder then delivers the final wake right away
			// (the abort emits a `TurnComplete`) and removes the link.
			let Some((rt, _)) = self.state.runtime_for_session(session_id).await else {
				return DisconnectWorkerOutcome::AlreadyDisconnected;
			};
			let token = rt.turn.lock().await.cancel.clone();
			let Some(token) = token else {
				return DisconnectWorkerOutcome::AlreadyDisconnected;
			};
			token.cancel();
			return DisconnectWorkerOutcome::Aborted;
		}
		// First click: the link is cut. Persist it (ADR 0065) so a
		// restart-time fleet rebuild doesn't resurrect the worker —
		// the worker's folder doubles as the coordinator's (same
		// coder root) for the remount hint.
		let folder_hint = self
			.state
			.runtime_for_session(session_id)
			.await
			.map(|(_, folder)| folder.to_string());
		persist_worker_detached(&self.state, &orchestrator_id, session_id, folder_hint.as_deref()).await;
		// If the worker is idle it will never emit the `TurnComplete`
		// the feeder uses to deliver the final wake, so notify the
		// orchestrator now and drop the link entirely.
		let running = match self.state.runtime_for_session(session_id).await {
			Some((rt, _)) => rt.turn.lock().await.cancel.is_some(),
			None => false,
		};
		if running {
			return DisconnectWorkerOutcome::Disconnected;
		}
		self
			.state
			.coordinator_workers
			.write()
			.await
			.remove(&orchestrator_id, session_id);
		let handle = CoderHandle {
			state: self.state.clone(),
		};
		// Snapshot the worker's branch *before* the notice so the
		// coordinator can re-plan from the handover instead of having to
		// remember to audit a black-box branch (ADR 0056).
		let snapshot = worker_branch_snapshot(&self.state, session_id).await;
		let label = worker_label(&self.state, session_id).await;
		// Detached — the click isn't blocked on the orchestrator's
		// wake bookkeeping; failure (orchestrator unmounted /
		// deleted) only costs the notice.
		tokio::spawn(async move {
			let state_line = snapshot.map(|s| format!(" Final state: {s}.")).unwrap_or_default();
			let _ = handle
				.send_to(
					&orchestrator_id,
					format!(
						"Worker {label} was disconnected by the user. It is no longer attached to you: \
						 its updates won't reach you any more and your control tools (steer / abort / commit / \
						 merge / respond) refuse it. Its session, branch, and worktree are untouched — the user \
						 owns it from here. Don't wait on it; adjust your plan.{state_line}"
					),
					Vec::new(),
				)
				.await;
		});
		DisconnectWorkerOutcome::Disconnected
	}
}

/// A short human label for a worker — its title, or the session id when
/// there's no title yet. Used to make wake / disconnect messages read
/// `fix-login-redirect` instead of an opaque `sess-…` id. Best-effort;
/// falls back to the bare id.
async fn worker_label(state: &Arc<CoderState>, worker_id: &str) -> String {
	let Some((rt, _)) = state.runtime_for_session(worker_id).await else {
		return worker_id.to_string();
	};
	let session = rt.session.lock().await;
	let title = session.header.title.trim();
	if title.is_empty() {
		worker_id.to_string()
	} else {
		format!("`{title}` ({worker_id})")
	}
}

/// A one-line git snapshot of a worker's branch for the disconnect
/// notice (ADR 0056 — disconnected-worker audit). The handover used to
/// tell the coordinator only "the user owns it now", leaving the branch
/// a black box the coordinator had to remember to audit. Now the notice
/// carries the state the coordinator needs to re-plan: branch, ahead /
/// behind upstream, uncommitted files, and how far the branch has
/// drifted behind the default. Best-effort — `None` when the folder or
/// git is unavailable, and the notice goes out without it.
async fn worker_branch_snapshot(state: &Arc<CoderState>, worker_id: &str) -> Option<String> {
	let (rt, folder_path) = state.runtime_for_session(worker_id).await?;
	// Prefer the worker's worktree over its session folder.
	let routing_path = {
		let session = rt.session.lock().await;
		match session.header.worktree_root.clone() {
			Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
			_ => folder_path.to_string(),
		}
	};
	let folder = state.workspaces.folder_for_path(&routing_path).await?;
	let branch = folder.host.git_branch().await.unwrap_or_default();
	let entries = folder.host.git_status_entries(&[]).await.unwrap_or_default();
	let uncommitted = entries
		.iter()
		.filter(|e| !matches!(e.status, moon_protocol::git::GitFileStatus::Ignored))
		.count();
	let name = branch.name.as_deref().unwrap_or("(detached)");
	let drift = if branch.default_branch_behind > 0 {
		format!(", {} behind default", branch.default_branch_behind)
	} else {
		String::new()
	};
	Some(format!(
		"branch `{name}` ({} ahead, {} behind upstream{drift}, {uncommitted} uncommitted file(s))",
		branch.ahead, branch.behind,
	))
}

/// Outcome of [`CoderHandle::disconnect_worker`] (ADR 0052).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectWorkerOutcome {
	/// The session is a still-attached worker: the link was cut and
	/// the coordinator is being told (immediately when idle, after
	/// its in-flight turn otherwise — see the dispatch feeder).
	Disconnected,
	/// The session was already disconnected and still had a turn in
	/// flight, which was cancelled — the second-click "stop it now"
	/// path.
	Aborted,
	/// The session was already disconnected with nothing to cancel.
	AlreadyDisconnected,
	/// The session isn't registered as any coordinator's worker;
	/// nothing happened.
	NotAWorker,
}

/// How much of a user's worker message we quote into the coordinator's
/// dispatch notice (ADR 0043). Enough for the intent of a typical
/// nudge; a wall of text would eat the coordinator's context for a
/// message it doesn't own.
const USER_MESSAGE_NOTICE_MAX: usize = 200;

/// Character cap for the `ask_user` questions quoted in a worker's
/// needs-input wake. Generous compared to [`USER_MESSAGE_NOTICE_MAX`]
/// because the packet is the coordinator's only copy of the question
/// ids and option ids it must key `respond_to_worker_prompt` answers
/// by; a clipped packet still resolves via `observe_worker`'s
/// `pending_prompt`.
const WORKER_PROMPT_NOTICE_MAX: usize = 2000;

/// Clamp `text` to `max` characters (not bytes — the cut must land on
/// a char boundary), appending an ellipsis + the dropped-character
/// count so the reader knows it's looking at a fragment.
fn truncate_for_notice(text: &str, max: usize) -> String {
	let total = text.chars().count();
	if total <= max {
		return text.to_string();
	}
	let kept: String = text.chars().take(max).collect();
	format!("{kept}… ({} more characters)", total - max)
}

/// Longest slug we keep from a coordinator-supplied worker name.
/// Branch names have no practical git limit; this is about the UI —
/// the sessions-list branch chip and the worktree directory name stay
/// readable.
const WORKER_BRANCH_SLUG_MAX: usize = 40;

/// Slug a coordinator-supplied worker name into the path component of
/// a `moon/<slug>` branch (ADR 0042). Lowercases, collapses every run
/// of non-alphanumerics into a single `-`, trims separators from both
/// ends, and caps the length. Returns `None` when nothing usable
/// survives (e.g. a name of pure punctuation) so the caller can reject
/// the argument instead of inventing a branch name.
///
/// The output is `[a-z0-9]([a-z0-9-]*[a-z0-9])?`, which sidesteps every
/// `git check-ref-format` trap (`..`, trailing `.`, `.lock`, spaces,
/// leading `-`) without needing to enumerate them.
fn worker_branch_slug(name: &str) -> Option<String> {
	let mut slug = String::with_capacity(name.len());
	for ch in name.chars() {
		if ch.is_ascii_alphanumeric() {
			slug.push(ch.to_ascii_lowercase());
			continue;
		}
		if !slug.ends_with('-') {
			slug.push('-');
		}
	}
	// A `moon-` prefix in the name would otherwise read as
	// `moon/moon-fix-login` once the namespace is prepended.
	let trimmed = slug.trim_matches('-');
	let trimmed = trimmed.strip_prefix("moon-").unwrap_or(trimmed);
	let mut slug = trimmed.to_string();
	if slug.len() > WORKER_BRANCH_SLUG_MAX {
		slug.truncate(WORKER_BRANCH_SLUG_MAX);
		// Prefer a word boundary over a chopped-off word, unless that
		// would throw away most of the name.
		if let Some(boundary) = slug.rfind('-').filter(|at| *at > WORKER_BRANCH_SLUG_MAX / 2) {
			slug.truncate(boundary);
		}
	}
	let slug = slug.trim_end_matches('-').to_string();
	(!slug.is_empty()).then_some(slug)
}

/// Pick a `moon/<slug>` branch name that doesn't collide with an
/// existing branch or a leftover worktree directory in `parent`,
/// suffixing `-2`, `-3`, … as needed. Two workers named the same thing
/// (a retry, or the same fix attempted twice) must not fail the spawn.
///
/// A branch-listing failure degrades to "assume nothing is taken":
/// `git worktree add` still refuses a duplicate, so the worst case is
/// the spawn erroring the way it would have before ADR 0042.
async fn free_worker_branch(parent: &moon_core::WorkspaceFolderEntry, parent_path: &str, slug: &str) -> String {
	let taken = parent.host.git_local_branches().await.unwrap_or_default();
	let worktrees = camino::Utf8Path::new(parent_path).join(moon_core::WORKTREES_DIR_NAME);
	for suffix in 1u32..100 {
		let candidate = match suffix {
			1 => format!("moon/{slug}"),
			n => format!("moon/{slug}-{n}"),
		};
		if taken.iter().any(|b| b == &candidate) {
			continue;
		}
		if worktrees.join(candidate.replace('/', "-")).exists() {
			continue;
		}
		return candidate;
	}
	// Absurd volume of same-named workers: fall back to the
	// timestamp scheme rather than loop forever.
	let now_ms = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis())
		.unwrap_or(0) as u64;
	format!("moon/{slug}-{:08x}", now_ms & 0xffff_ffff)
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
	/// The parked `ask_user`'s arguments (its `questions` array with
	/// ids and options) when `needs_input` — everything the
	/// coordinator needs to build `respond_to_worker_prompt` answers
	/// without reading the worker's transcript. `None` when nothing
	/// is parked, or (defensively) when the raising call can't be
	/// found on the transcript.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pending_prompt: Option<serde_json::Value>,
	/// The worker's most recent assistant message text. Empty when
	/// the worker hasn't produced one yet.
	pub last_assistant: String,
	/// The worker's last per-turn diff (ADR 0030) — the files the
	/// agent's tools touched and the unified diff against the
	/// baseline captured at turn start. `None` until the first turn
	/// that touches files lands a `TurnDiff`. The diff text may be
	/// empty when the turn's writes were identical to the baseline.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub last_diff: Option<TurnDiffSummary>,
}

/// Per-file change summary for the diff in [`WorkerSnapshot`] (ADR
/// 0030). The default `observe_worker` return — enough for the
/// coordinator to decide "on track" vs "sideways" without flooding
/// its context with patch text. The full diff is a deliberate pull
/// via `review_worker_changes`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnDiffFileSummary {
	/// Relative path of the changed file.
	pub path: String,
	/// Lines added (insertions). Approximate — counted from the
	/// unified diff hunk headers.
	pub added: u32,
	/// Lines removed (deletions). Approximate — same source.
	pub removed: u32,
}

/// Compact diff summary carried in [`WorkerSnapshot`] (ADR 0030).
/// Replaces the full patch text in the default observe return so the
/// coordinator's context stays plan-shaped. Use `review_worker_changes`
/// to pull the full diff when you need to actually review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnDiffSummary {
	pub files: Vec<TurnDiffFileSummary>,
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
	cache_stats: SessionCacheStats,
}

/// Session-lifetime prompt-cache scoreboard: `hits` counts
/// provider-reported round-trips whose `cache_read > 0`,
/// `requests` counts all provider-reported round-trips. Estimate
/// fallbacks don't move either counter. Rebuilt from persisted
/// `Usage` records on reopen, so the numbers survive restarts and
/// work retroactively on old sessions.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SessionCacheStats {
	pub(crate) hits: u32,
	pub(crate) requests: u32,
}

impl SessionCacheStats {
	fn record(&mut self, usage: &TokenUsage) {
		self.requests += 1;
		if usage.cache_read_input_tokens > 0 {
			self.hits += 1;
		}
	}
}

/// Where a [`Coder::send`] actually landed, resolved when the send
/// picked its target. Callers that persist the per-folder "last
/// opened session" pointer must key off this instead of re-reading
/// the active folder after the await — a user who hits Enter and
/// immediately switches projects would otherwise pin the pointer
/// under the *new* folder's key and leave the sending folder's
/// pointer stale (the "project switch lands on an old session"
/// bug).
#[derive(Debug, Clone)]
pub struct SendTarget {
	/// Coder-root folder path — worktree sessions are filed under
	/// their parent project root.
	pub coder_root: String,
	/// The session's git-worktree root when it runs in one.
	pub worktree_root: Option<String>,
	/// Session id the message went to.
	pub session_id: String,
}

impl SendTarget {
	/// Key for `AppState.coder.last_session_by_folder`: the actual
	/// folder context the session drives — its worktree path when
	/// it has one, else the coder root.
	pub fn pointer_key(&self) -> &str {
		self.worktree_root.as_deref().unwrap_or(&self.coder_root)
	}
}

/// Result of [`Coder::observe_session_in`] — the session's summary
/// plus its full replay, returned to the caller (the bridge ships it
/// in the `coder_open_session` RPC response) instead of broadcast on
/// the event channel. `in_flight` mirrors `CoderEvent::Replay`'s
/// flag: the session still has a turn streaming in the background.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservedSession {
	pub summary: SessionSummary,
	pub events: Vec<CoderEvent>,
	pub in_flight: bool,
	/// True when `events` is a *windowed* tail of a longer
	/// transcript (the observe path was given `max_events` and the
	/// session had more). The first event is then
	/// [`CoderEvent::HistoryWindowStart`], whose ordinal resumes the
	/// pagination. Absent/`false` for a full replay.
	#[serde(default)]
	pub has_more: bool,
}

/// Result of [`Coder::session_history_older`] — the next-older
/// window of a session's transcript for the companion's upward
/// pagination. `events` are plain replay events (no terminator /
/// usage / orphan synthesis — those belong to the tail the initial
/// observe already shipped); the phone prepends them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryWindow {
	pub events: Vec<CoderEvent>,
	/// True when history older than this window exists.
	pub has_more: bool,
	/// The full-sequence ordinal where this window begins — pass as
	/// `before_event_ordinal` for the next-older page.
	pub before_event_ordinal: usize,
	/// Total events in the full transcript (informational).
	pub total_events: usize,
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
/// Drain the format queue and run `format_file` on each entry.
/// Returns the drained entries so the caller can reuse them (e.g.
/// for per-turn diff computation). Empty vec when nothing was queued.
async fn flush_format_queue(
	state: &Arc<CoderState>,
	queue: &Arc<crate::tools::FormatQueue>,
) -> Vec<(String, Utf8PathBuf)> {
	let entries = queue.drain();
	if entries.is_empty() {
		return Vec::new();
	}
	for (folder_path, rel) in &entries {
		let Some(folder) = state.workspaces.folder_for_path(folder_path.as_str()).await else {
			tracing::warn!(
				folder = %folder_path,
				path = %rel,
				"format-on-save (turn end): bound folder gone before flush; skipping"
			);
			continue;
		};
		if let Err(err) = folder.host.format_file(rel).await {
			tracing::warn!(
				folder = %folder_path,
				path = %rel,
				%err,
				"format-on-save (turn end): format_file failed"
			);
		}
	}
	entries
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

/// Count added/removed lines for a specific file in a unified diff.
/// Scans the diff for hunk headers (`+++` / `---` lines starting with
/// `diff --git`) and counts `+` / `-` lines within that file's hunks.
/// Approximate — doesn't parse hunk ranges, just counts prefix lines.
/// Used by `observe_session` to build the per-file summary without
/// returning the full patch text.
fn count_diff_lines_for_file(diff: &str, path: &str) -> (u32, u32) {
	let mut added = 0u32;
	let mut removed = 0u32;
	let mut in_file = false;
	for line in diff.lines() {
		if line.starts_with("diff --git") {
			in_file = line.contains(&format!(" b/{path}"));
			continue;
		}
		if !in_file {
			continue;
		}
		if line.starts_with('+') && !line.starts_with("+++") {
			added += 1;
		} else if line.starts_with('-') && !line.starts_with("---") {
			removed += 1;
		}
	}
	(added, removed)
}

/// Capture the baseline commit SHA for per-turn diff attribution
/// (ADR 0030). Resolves the session's working-tree folder (worktree
/// when bound, else parent) and calls `git_snapshot_baseline` on it.
/// Returns `None` when not a repo, git unavailable, or the folder
/// can't be resolved — all non-fatal, the turn just doesn't get a
/// diff row.
async fn capture_baseline(state: &Arc<CoderState>, folder_path: &Utf8Path) -> Option<String> {
	let worktree_root = {
		let session = rt_session_header(state, folder_path).await?;
		session.worktree_root.clone()
	};
	let routing_path = match worktree_root {
		Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
		_ => folder_path.to_string(),
	};
	let folder = state.workspaces.folder_for_path(&routing_path).await?;
	folder.host.git_snapshot_baseline().await.ok().flatten()
}

/// Read the session header for the visible runtime under `folder_path`.
/// Helper for `capture_baseline` to get the worktree root without
/// threading the `Arc<SessionRuntime>` through.
async fn rt_session_header(state: &Arc<CoderState>, folder_path: &Utf8Path) -> Option<SessionHeader> {
	let fs_map = state.sessions_by_folder.read().await;
	let fs = fs_map.get(folder_path)?;
	let visible_id = fs.visible_session_id().await?;
	let rt = fs.runtime(&visible_id).await?;
	let session = rt.session.lock().await;
	Some(session.header.clone())
}

/// Compute the per-turn diff and emit + persist it (ADR 0030). Runs
/// `git_diff_against(baseline_sha, files)` against the session's
/// working tree, emits a `CoderEvent::TurnDiff` so the panel renders
/// a collapsible diff row, and appends a `SessionRecord::TurnDiff`
/// to the JSONL so reload + the companion + an orchestrator's
/// `observe_worker` can all read it. Best-effort — git failures
/// produce an empty diff, not an error.
async fn emit_turn_diff(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	folder_path: &Utf8Path,
	baseline_sha: &str,
	files: &[(String, Utf8PathBuf)],
) {
	// Resolve the session's working-tree folder (worktree when bound).
	let worktree_root = rt.session.lock().await.header.worktree_root.clone();
	let routing_path = match worktree_root {
		Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
		_ => folder_path.to_string(),
	};
	let Some(folder) = state.workspaces.folder_for_path(&routing_path).await else {
		return;
	};
	// Collect the relative file paths for the diff scope.
	let file_paths: Vec<String> = files.iter().map(|(_, rel)| rel.to_string()).collect();
	let diff = folder
		.host
		.git_diff_against(baseline_sha, &file_paths)
		.await
		.unwrap_or_default();
	// Emit the live event so the panel renders the diff row.
	sink.send(CoderEvent::TurnDiff {
		files: file_paths.clone(),
		diff: diff.clone(),
	});
	// Store on the session so `observe_session` can return it
	// without re-running git.
	{
		let mut session = rt.session.lock().await;
		session.last_turn_diff = Some((file_paths.clone(), diff.clone()));
	}
	// Persist the record so reload + companion + observe_worker
	// can read it. Best-effort — a JSONL write failure logs but
	// doesn't fail the turn.
	let session = rt.session.lock().await;
	if let Some(dir) = session.session_dir.clone() {
		let header = session.header.clone();
		drop(session);
		let record = SessionRecord::TurnDiff {
			files: file_paths,
			diff,
		};
		if let Err(err) = sessions::append_record(&dir, &header, &record).await {
			tracing::warn!(error = %err, "failed to persist turn diff record");
		}
	}
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
	let session_hint = sink_for_turn.session_id.clone();
	let sink_for_backoff = sink_for_turn.clone();
	tokio::spawn(crate::inference::SESSION_HINT.scope(
		session_hint,
		crate::inference::TURN_STICKY_MODEL.scope(
			std::sync::Mutex::new(None),
			TURN_EVENT_SINK.scope(sink_for_backoff, async move {
				// Scope-tied so every exit path (success, abort, error,
				// steer-drain re-loop) decrements exactly once, even on
				// panic.
				let _running_guard = RunningTurnGuard::acquire(&state.running_turns);
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
					let background = Arc::new(crate::tools::BackgroundProcessRegistry::default());
					// Live settlement notices for detached background
					// processes (ADR 0034): the panel flips the spawning
					// `bash` row out of "detached, still running" the
					// moment the process exits or is reaped.
					{
						let sink = sink_for_turn.clone();
						background.set_event_sink(Arc::new(move |event| sink.send(event)));
					}
					// Capture the baseline SHA at turn start for per-turn
					// diff attribution (ADR 0030). `git stash create`
					// snapshots the working tree without touching it; HEAD
					// is the fallback when the tree is clean. Best-effort —
					// `None` means no git repo / git unavailable, in which
					// case we skip the diff computation at turn end.
					let baseline_sha = capture_baseline(&state, &folder_for_turn).await;
					let result = run_turn(
						&state,
						&rt_for_turn,
						&folder_for_turn,
						&sink_for_turn,
						cancel_outer.clone(),
						format_queue.clone(),
						background.clone(),
						// Only the first run_turn call gets the resume
						// tool calls; subsequent loop iterations (steer
						// drains after an abort) start fresh.
						resume.take(),
					)
					.await;
					let flushed_files = flush_format_queue(&state, &format_queue).await;
					// Kill + reap any detached background processes still
					// running at turn end (ADR 0034). Runs on every
					// termination path, same as `flush_format_queue`.
					background.cleanup().await;
					// Compute + emit the per-turn diff on a successful turn
					// that touched files. Best-effort — a git failure or no
					// baseline just means no diff row, not an error.
					if matches!(result, Ok(())) {
						if let Some(sha) = &baseline_sha {
							if !flushed_files.is_empty() {
								emit_turn_diff(
									&state,
									&rt_for_turn,
									&sink_for_turn,
									&folder_for_turn,
									sha,
									&flushed_files,
								)
								.await;
							}
						}
					}
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
				// A turn's `bash` can remove a worktree checkout behind the
				// registry's back (ADR 0063) — reconcile on every exit path
				// so the project bar drops dead rows at turn end instead of
				// waiting for a manual unbind.
				if !prune_missing_worktrees(&state).await.is_empty() {
					sink_for_turn.send(CoderEvent::WorkspaceFoldersChanged);
				}
				if auto_rename_after {
					spawn_auto_rename(state.clone(), rt_for_turn.clone(), sink_for_turn);
				}
				// Idle-grace MCP reaper: kill this session's MCP server
				// instances (headless browsers, per-session since the
				// tab-fight fix) if no new turn starts within the grace
				// window. Anchored at turn end rather than a periodic
				// sweep: active back-and-forth keeps the browser (and the
				// page the user is iterating on), abandoned standalone
				// sessions get cleaned instead of leaking a chromium until
				// process exit. Workers get this too, on top of the
				// immediate reap at retire.
				let reaper_session = rt_for_turn.session.lock().await.header.id.clone();
				tokio::spawn(async move {
					tokio::time::sleep(MCP_IDLE_GRACE).await;
					// A newer turn is running (or just started): its own
					// end will schedule a fresh reaper. Racing the check
					// against a turn that starts a moment later is
					// harmless - the next MCP call just respawns.
					if rt_for_turn.turn.lock().await.cancel.is_some() {
						return;
					}
					state.tools.mcp().drop_session_connections(&reaper_session).await;
				});
			}),
		),
	));
}

/// How long a session's MCP server instances survive after its
/// last turn ends. Long enough that "open the page" / "now click
/// X" conversations keep their browser between prompts; short
/// enough that walked-away-from sessions don't park a headless
/// chromium until process exit.
const MCP_IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// RAII increment/decrement on [`CoderState::running_turns`]. Held
/// for the whole lifetime of a turn-loop task so the count survives
/// steer-drain re-loops and drops exactly once no matter how the
/// task exits.
struct RunningTurnGuard {
	counter: watch::Sender<usize>,
}

impl RunningTurnGuard {
	fn acquire(counter: &watch::Sender<usize>) -> Self {
		counter.send_modify(|n| *n += 1);
		Self {
			counter: counter.clone(),
		}
	}
}

impl Drop for RunningTurnGuard {
	fn drop(&mut self) {
		self.counter.send_modify(|n| *n = n.saturating_sub(1));
	}
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	folder_path: &Utf8Path,
	sink: &FolderEventSink,
	cancel: CancellationToken,
	format_queue: Arc<crate::tools::FormatQueue>,
	background: Arc<crate::tools::BackgroundProcessRegistry>,
	mut resume_tool_calls: Option<Vec<crate::inference::ToolCall>>,
) -> Result<(), CoderError> {
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
	// The routing path is normally a bound folder; the exception is
	// the scratch root of an empty workspace, which is never
	// registered — `folder_entry_for` synthesises an entry for it.
	let folder_entry = state
		.folder_entry_for(routing_path.as_str())
		.await
		.ok_or(CoderError::NoActiveFolder)?;
	// Snapshot the top-level session mode once at turn-start (same
	// posture as `models` above): a re-stamp mid-turn applies to
	// the *next* turn. The mode drives both the `ToolContext` (so
	// the dispatch-level write gate is right) and the tool-list
	// composition below. The bash-target override is different: the
	// runtime's **live** flag is shared into the context, so the
	// host/container toggle re-routes the next tool dispatch of an
	// in-flight turn (a long turn shouldn't pin the user to the
	// wrong target).
	let mode = {
		let session = rt.session.lock().await;
		CoderMode::from_top_level_wire(session.header.mode.as_deref())
	};
	let force_host_bash = rt.force_host_bash.load(std::sync::atomic::Ordering::Relaxed);
	let cx = ToolContext::with_format_queue(folder_entry, mode, format_queue)
		.with_force_host_bash(rt.force_host_bash.clone())
		.with_background(background);
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
	// MCP meta-tools (ADR 0033): only present when the workspace
	// has enabled servers, so the model never sees a surface that
	// is guaranteed to error.
	tool_defs.extend(state.tools.mcp_definitions().await);
	// Terminal inspection (ADR 0048): present only while this
	// project has a terminal open, so an agent can read the dev
	// server the user already started instead of racing it.
	tool_defs.extend(state.tools.terminal_definitions(&cx).await);
	if mode.allows_task_tool() {
		tool_defs.push(task_tool_definition());
		// Detached-sub-agent companions ([ADR 0053]): advertised to
		// the same `Agent` mode that sees `task`, so sub-agents and
		// coordinators never see them.
		tool_defs.push(crate::subagent::task_collect_tool_definition());
		tool_defs.push(crate::subagent::task_steer_tool_definition());
		tool_defs.push(crate::subagent::task_abort_tool_definition());
	}
	if mode.allows_ask_user() {
		tool_defs.push(ask_user_tool_definition());
	}
	if mode == CoderMode::Coordinator {
		tool_defs.push(crate::coordinator::spawn_worker_tool_definition());
		tool_defs.push(crate::coordinator::observe_worker_tool_definition());
		tool_defs.push(crate::coordinator::list_workers_tool_definition());
		tool_defs.push(crate::coordinator::steer_worker_tool_definition());
		tool_defs.push(crate::coordinator::abort_worker_tool_definition());
		tool_defs.push(crate::coordinator::respond_to_worker_prompt_tool_definition());
		tool_defs.push(crate::coordinator::review_worker_changes_tool_definition());
		tool_defs.push(crate::coordinator::workspace_scm_status_tool_definition());
		tool_defs.push(crate::coordinator::commit_worker_changes_tool_definition());
		tool_defs.push(crate::coordinator::merge_worker_changes_tool_definition());
		tool_defs.push(crate::coordinator::check_worker_base_tool_definition());
		tool_defs.push(crate::coordinator::discard_worker_worktree_tool_definition());
		tool_defs.push(crate::coordinator::retire_worker_tool_definition());
		tool_defs.push(crate::coordinator::clone_repo_tool_definition());
		tool_defs.push(crate::coordinator::init_repo_tool_definition());
		tool_defs.push(crate::coordinator::add_folder_tool_definition());
	}
	// Compose a fresh system prompt and overwrite the session's
	// `messages[0]`: the base prompt plus a "Bound folders"
	// section keyed off whatever summaries are currently cached.
	// Sub-agent dispatch reads the same cache so the model's
	// awareness of bound folders is consistent across parent +
	// sub-agent prompts.
	// Use the routing path (worktree when bound, else parent) as the
	// prompt's "active folder" so the agent is oriented at the
	// checkout its tools actually operate on. A scratch session
	// (empty workspace) passes `None` — no project rules to read,
	// and its "no folders bound" section is written from the
	// scratch root instead.
	let scratch = state.is_no_folder_root(&routing_path).await;
	refresh_system_prompt(state, rt, &routing_path, scratch, force_host_bash, mode).await;
	// Schedule background regeneration for any bound folder whose
	// summary cache is missing or stale. Detached tokio tasks; we
	// don't block the turn waiting for them to land. The next
	// turn will pick up whichever finished in the interim via the
	// fresh `refresh_system_prompt` above.
	kick_off_summary_refresh(state, sink).await;
	// Consecutive empty-shell responses (provider bailed
	// mid-stream). Reset on any real response; when it exceeds
	// `EMPTY_RESPONSE_RETRIES` the turn fails loudly instead of
	// ending as a phantom success.
	let mut empty_shell_attempts: usize = 0;
	// Continuations spent re-asking the model to finish an answer
	// the provider cut off at the output-token ceiling. Capped by
	// `OUTPUT_CAP_CONTINUATIONS`; never reset, so a pathological
	// turn can't loop on it.
	let mut output_cap_continuations: usize = 0;
	for _iter in 0..MAX_TURN_ITERATIONS {
		if cancel.is_cancelled() {
			return Err(CoderError::Aborted);
		}

		// Re-read the user's model picks at the top of every
		// round-trip, matching the per-request route resolution
		// inside `InferenceClient`. The two MUST come from the
		// same snapshot generation: pinning the model slug at
		// turn-start while the route floats per request meant a
		// provider switch mid-turn sent the old provider's slug
		// to the new provider's endpoint (e.g. an HF slug to
		// Anthropic's `/v1/messages` → 404 `model: not found`).
		// A flip mid-turn now cleanly applies to the *next*
		// round-trip: model, route, and the persisted
		// `provider/model` stamp all move together.
		let models = state.models.read().await.clone();
		let standard_model = models.standard().to_owned();
		let pi_model = models.resolve_route().pi_provider_model(&standard_model);

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
		let (last_usage, cache_stats_snapshot) = {
			let session = rt.session.lock().await;
			(session.last_usage, session.cache_stats)
		};
		let mut messages = rt.session.lock().await.messages.clone();
		// Image budget planning runs *before* compaction (ADR 0049):
		// compaction's in-session summary call replays the elision
		// set, and marking fresh screenshots only after compaction
		// let its summary request ship them un-elided — a session
		// that crossed both the token threshold and the image budget
		// in the same iteration 413'd inside compaction before the
		// budget could ever run, and every retry died the same way.
		if let Some(budget) = state.inference.image_wire_budget().await {
			let mut session = rt.session.lock().await;
			let mut marked = std::mem::take(&mut session.elided_images);
			let newly = crate::images::plan_elision(&messages, budget, &mut marked);
			if newly > 0 {
				tracing::info!(
					newly,
					total = marked.len(),
					"image payload over budget; dropping the oldest attachments from the prompt"
				);
			}
			session.elided_images = marked;
		}
		let elided_images = rt.session.lock().await.elided_images.clone();
		let compaction = crate::compaction::compact_if_needed(
			&state.inference,
			sink,
			None,
			&models,
			&tool_defs,
			last_usage.as_ref(),
			&mut messages,
			&elided_images,
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

		// Apply the image elisions planned above (before compaction)
		// to the wire copy. `session.messages` keeps every
		// attachment — the panel and a later reload are unaffected
		// (ADR 0049).
		crate::images::apply_elision(&mut messages, &elided_images);

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
				// Replayed from a checkpoint, so the calls come off
				// disk rather than a live stream — no output cap in
				// play.
				dispatch_tool_calls(state, rt, sink, &cx, &cancel, &calls, false).await?;
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
			session_cache_hits: cache_stats_snapshot.hits,
			session_requests: cache_stats_snapshot.requests,
			model: crate::inference::effective_model(&standard_model),
		});
		// `Mutex` rather than `Cell` because the future the
		// closure participates in is required to be `Send` —
		// `tokio::spawn` requires a `Send` future, and `Cell` is
		// not `Sync`. The closure runs sequentially from a single
		// task so there's no real contention.
		let stream_usage_state = std::sync::Mutex::new((0u32, std::time::Instant::now()));

		let mut response = state
			.inference
			.chat_completion_stream(
				&standard_model,
				&messages,
				&tool_defs,
				crate::defaults::turn_output_budget(prompt_estimate, context_window),
				&cancel,
				|event| match event {
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
							cache_stats_snapshot,
							&standard_model,
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
							cache_stats_snapshot,
							&standard_model,
						);
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

		// Uniquify before anything observes the response — events,
		// history push, persistence, and dispatch must all agree
		// on the remapped ids.
		dedupe_response_tool_call_ids(&messages, &mut response.tool_calls);

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
			empty_shell_attempts = 0;
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
			if let Some(u) = response.usage {
				session.cache_stats.record(&u);
			}
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
		let cache_stats_now = rt.session.lock().await.cache_stats;
		emit_token_usage(sink, &models, &standard_model, &messages, &response, cache_stats_now);

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
			// An empty shell isn't a final answer — the provider
			// bailed. Re-send the same round-trip (nothing was
			// appended to `messages`, so `continue` is a retry);
			// after `EMPTY_RESPONSE_RETRIES` consecutive shells,
			// fail the turn so the user sees an error banner
			// instead of the agent silently stopping mid-work.
			if response_is_empty {
				empty_shell_attempts += 1;
				if empty_shell_attempts > EMPTY_RESPONSE_RETRIES {
					return Err(CoderError::EmptyResponse {
						attempts: empty_shell_attempts as u32,
					});
				}
				tracing::warn!(
					attempt = empty_shell_attempts,
					"provider returned an empty response; retrying the round-trip"
				);
				continue;
			}
			// The provider cut the answer off at the output-token
			// ceiling — what we have is a fragment, not a final
			// message. Ask for the rest instead of ending the turn
			// mid-sentence. For a *content* truncation the partial
			// is already in `messages` (and on disk), so the model
			// sees its own tail and resumes from it. A *thinking-
			// only* truncation is different: reasoning is not
			// replayed to the model, so it has no memory of the cut
			// thread — quote the tail of it in the sentinel or the
			// model re-derives from scratch and hits the cap again.
			if response.hit_output_cap() && output_cap_continuations < OUTPUT_CAP_CONTINUATIONS {
				output_cap_continuations += 1;
				let content_is_empty = response.content.as_deref().map(str::trim).unwrap_or("").is_empty();
				let prompt = if content_is_empty {
					let tail = response
						.thinking
						.as_deref()
						.map(|t| tail_chars(t, 2_000))
						.unwrap_or_default();
					crate::defaults::output_cap_thinking_continuation_prompt(tail)
				} else {
					OUTPUT_CAP_CONTINUATION_PROMPT.to_owned()
				};
				tracing::warn!(
					continuation = output_cap_continuations,
					thinking_only = content_is_empty,
					"assistant message hit the output-token ceiling; asking the model to continue"
				);
				push_sentinel_user_message(rt, sink, prompt).await;
				continue;
			}
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

		dispatch_tool_calls(
			state,
			rt,
			sink,
			&cx,
			&cancel,
			&response.tool_calls,
			response.hit_output_cap(),
		)
		.await?;
	}

	// Iteration cap reached. Rather than just bailing with an
	// error banner — which leaves the user staring at a wall of
	// tool calls and no actual answer — we ask the model for one
	// final, tools-disabled wrap-up turn. It sees the full history
	// it just produced, the tool budget exhausted note, and is
	// instructed to write its best answer with what it has.
	wrap_up_final_answer(state, rt, sink, &cancel, &tool_defs).await
}

/// Append a synthetic user message to the live history, the JSONL,
/// and the panel. Used for the runner's two sentinels — the
/// tool-budget wrap-up and the output-cap continuation. They're real
/// conversation turns, not a hidden side-channel: rereading the
/// session later has to make it obvious why the assistant suddenly
/// stopped calling tools, or why one answer arrived in two bubbles.
/// Persistence is best-effort; a write failure logs and the turn
/// carries on with the in-memory history.
/// Last `n` chars of `s`, char-boundary safe. Used to quote the
/// tail of a truncated reasoning trace into the continuation
/// sentinel without blowing up the prompt.
fn tail_chars(s: &str, n: usize) -> &str {
	match s.char_indices().rev().nth(n.saturating_sub(1)) {
		Some((idx, _)) => &s[idx..],
		None => s,
	}
}

async fn push_sentinel_user_message(rt: &Arc<SessionRuntime>, sink: &FolderEventSink, text: String) {
	{
		let mut session = rt.session.lock().await;
		session.messages.push(ChatMessage::user(text.clone()));
	}
	let (dir, header) = {
		let session = rt.session.lock().await;
		(session.session_dir.clone(), session.header.clone())
	};
	if let Some(dir) = dir {
		let record = SessionRecord::User {
			text: text.clone(),
			images: Vec::new(),
			from_coordinator: false,
		};
		if let Err(err) = sessions::append_record(&dir, &header, &record).await {
			tracing::warn!(error = %err, "failed to persist sentinel user message");
		} else {
			let mut session = rt.session.lock().await;
			session.persisted_records = session.persisted_records.saturating_add(1);
		}
	}
	sink.send(CoderEvent::UserMessage {
		id: new_message_id(),
		text,
		images: Vec::new(),
		queued: false,
		from_coordinator: false,
		created_at_ms: Some(current_time_ms()),
	});
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

	let sentinel_text = format!(
		"[Tool-call budget exhausted: you've used all {MAX_TURN_ITERATIONS} tool-call iterations available for this turn. \
Do not call any more tools. Write a final response now using only what you've already gathered: summarise what was \
done, what's still unfinished, and any uncertainty. If the user needs to take a follow-up action, say so explicitly.]"
	);
	push_sentinel_user_message(rt, sink, sentinel_text).await;

	let messages = rt.session.lock().await.messages.clone();
	let assistant_id = new_message_id();
	let id_for_cb = assistant_id.clone();
	let sink_for_cb = sink.clone();
	let started = std::sync::atomic::AtomicBool::new(false);
	let thinking_emitted = std::sync::atomic::AtomicBool::new(false);
	let output_budget = crate::defaults::turn_output_budget(
		estimate_prompt_tokens(&messages),
		models.context_window(&standard_model),
	);
	let mut response = state
		.inference
		.chat_completion_stream(
			&standard_model,
			&messages,
			&[],
			output_budget,
			cancel,
			|event| match event {
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
			},
		)
		.await?;

	// Defensive: tools are disabled on this round-trip, so a
	// response carrying calls at all is a provider bug — but
	// keep ids session-unique before persisting either way.
	dedupe_response_tool_call_ids(&messages, &mut response.tool_calls);

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

	let cache_stats_now = {
		let mut session = rt.session.lock().await;
		if let Some(u) = response.usage {
			session.cache_stats.record(&u);
		}
		if !response_is_empty {
			session.messages.push(response_to_message(&response));
		}
		session.cache_stats
	};
	if !response_is_empty {
		persist_assistant_record(rt, &response, Some(pi_model)).await;
	}
	// Persist the usage record here too — this path's round-trip
	// must fold into the session cache scoreboard on reopen just
	// like the main loop's.
	persist_usage_record(rt, &response).await;
	emit_token_usage(sink, &models, &standard_model, &messages, &response, cache_stats_now);

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
	hit_output_cap: bool,
) -> Result<(), CoderError> {
	// A batch with a truncated call in it falls through to the
	// sequential path, which refuses that one call and runs the
	// rest. Losing parallelism on a broken batch is a fine price
	// for keeping the refusal in exactly one place.
	//
	// A batch containing a `detach: true` call also falls through:
	// detached runs return their handle immediately, so the
	// batch's `join_all`-then-report shape doesn't apply to them —
	// routing them through the sequential dispatch (where
	// `handle_task` branches to the detached spawn) keeps exactly
	// one detached path.
	let homogeneous_subagent = calls.len() >= 2
		&& calls.iter().all(|c| c.function.name == "task")
		&& calls
			.iter()
			.all(|c| tool_args_or_refusal(&c.function, hit_output_cap).is_ok())
		&& !calls.iter().any(|c| {
			parse_tool_args(&c.function)
				.get("detach")
				.and_then(Value::as_bool)
				.unwrap_or(false)
		});
	if homogeneous_subagent {
		dispatch_subagent_batch(state, rt, sink, cx, cancel, calls).await
	} else {
		for call in calls {
			if cancel.is_cancelled() {
				return Err(CoderError::Aborted);
			}
			let args = match tool_args_or_refusal(&call.function, hit_output_cap) {
				Ok(args) => args,
				Err(err) => {
					// The row still has to appear (and the API
					// still needs a `tool_result` for every
					// `tool_use` block) — it just carries the
					// refusal instead of a result.
					sink.send(CoderEvent::ToolCall {
						id: call.id.clone(),
						name: call.function.name.clone(),
						args: Value::Object(Default::default()),
						started_at_ms: Some(current_time_ms()),
					});
					finish_tool_call(rt, sink, &call.id, &call.function.name, Err(err), None).await?;
					continue;
				}
			};
			sink.send(CoderEvent::ToolCall {
				id: call.id.clone(),
				name: call.function.name.clone(),
				args: args.clone(),
				started_at_ms: Some(current_time_ms()),
			});
			let dispatched_at = std::time::Instant::now();
			let outcome = if call.function.name == "task" {
				handle_task(state, rt, sink, cx, cancel, &call.id, &args).await
			} else if call.function.name == "task_collect" {
				// Detached-sub-agent report fetch ([ADR 0053]).
				handle_task_collect(state, rt, &args).await
			} else if call.function.name == "task_steer" {
				handle_task_steer(state, rt, &args).await
			} else if call.function.name == "task_abort" {
				handle_task_abort(state, rt, &args).await
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
			} else if call.function.name == "list_workers" {
				handle_list_workers(state, sink, &args).await
			} else if call.function.name == "steer_worker" {
				handle_steer_worker(state, &args).await
			} else if call.function.name == "abort_worker" {
				handle_abort_worker(state, &args).await
			} else if call.function.name == "respond_to_worker_prompt" {
				handle_respond_to_worker_prompt(state, &args).await
			} else if call.function.name == "review_worker_changes" {
				handle_review_worker_changes(state, &args).await
			} else if call.function.name == "workspace_scm_status" {
				handle_workspace_scm_status(state, sink, &args).await
			} else if call.function.name == "commit_worker_changes" {
				handle_commit_worker_changes(state, &args).await
			} else if call.function.name == "merge_worker_changes" {
				handle_merge_worker_changes(state, &args).await
			} else if call.function.name == "check_worker_base" {
				handle_check_worker_base(state, &args).await
			} else if call.function.name == "discard_worker_worktree" {
				handle_discard_worker_worktree(state, sink, &args).await
			} else if call.function.name == "retire_worker" {
				handle_retire_worker(state, sink, &args).await
			} else if call.function.name == "clone_repo" {
				handle_clone_repo(state, sink, &args).await
			} else if call.function.name == "init_repo" {
				handle_init_repo(state, sink, &args).await
			} else if call.function.name == "add_folder" {
				handle_add_folder(state, sink, &args).await
			} else {
				state
					.tools
					.dispatch_with_call_id(&call.function.name, &args, cx, cancel, &call.id)
					.await
			};
			let duration_ms = u64::try_from(dispatched_at.elapsed().as_millis()).ok();
			finish_tool_call(rt, sink, &call.id, &call.function.name, outcome, duration_ms).await?;
		}
		Ok(())
	}
}

/// Run N parallel sub-agents under a `Semaphore`, then drain
/// results in the order the model issued them so the conversation
/// history stays deterministic. Cancellation cascades automatically
/// via `cancel.child_token()` (the parent's token is the child's
/// parent).
///
/// Each sub-agent's `ToolResult` **event** fires the moment that
/// sub-agent finishes — not when its earlier siblings do — so the
/// panel can stop that row's elapsed timer even while the rest of
/// the batch is still running. (An earlier implementation awaited
/// the spawn handles in call order, which stranded later rows in
/// the live "running…" state until the longest-running sibling
/// settled.) The `messages` push, by contrast, is reassembled in
/// the model's original call order once the whole batch resolves,
/// so the conversation history the next LLM round-trip sees — and
/// anything persisted from it — is deterministic across replays.
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
	let batch_started_at_ms = current_time_ms();
	for (call, args) in calls.iter().zip(parsed_args.iter()) {
		sink.send(CoderEvent::ToolCall {
			id: call.id.clone(),
			name: call.function.name.clone(),
			args: args.clone(),
			started_at_ms: Some(batch_started_at_ms),
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
		let call_name = call.function.name.clone();
		let task = tokio::spawn(async move {
			let _permit = sem_for_task.acquire().await.expect("semaphore not closed");
			// Timed from permit acquisition, not batch spawn, so a
			// sub-agent queued behind the parallelism cap doesn't
			// book its wait time as execution time.
			let dispatched_at = std::time::Instant::now();
			let outcome = handle_task(
				&state_for_task,
				&rt_for_task,
				&sink_for_task,
				&cx_for_task,
				&cancel_for_task,
				&call_id,
				&args,
			)
			.await;
			let duration_ms = u64::try_from(dispatched_at.elapsed().as_millis()).ok();
			// Emit + persist immediately on completion so this row's
			// timer stops in the UI; the `ChatMessage::Tool` for the
			// conversation history rides back to the caller, which
			// reassembles the batch in call order before pushing.
			let message =
				match emit_tool_result(&rt_for_task, &sink_for_task, &call_id, &call_name, outcome, duration_ms).await {
					Ok(message) => message,
					Err(err) => return Err(err),
				};
			Ok(message)
		});
		tasks.push(task);
	}
	// Await every handle, collecting the per-call tool message (or
	// first error) so the `messages` push below lands in the model's
	// original tool-call order regardless of completion order. A
	// `join_all` (not an early-return `?` loop) guarantees a slow
	// sibling never blocks an already-finished sub-agent's message
	// from being recorded, and a panicking task can't strand the
	// rest of the batch's results.
	let joined = futures_util::future::join_all(tasks).await;
	let mut messages = Vec::with_capacity(joined.len());
	let mut first_err: Option<CoderError> = None;
	for (call, result) in calls.iter().zip(joined) {
		match result {
			// Sub-agent ran to completion and its `ToolResult` already
			// went out over the sink inside the spawned task.
			Ok(Ok(message)) => messages.push(message),
			// The sub-agent's emit was aborted — propagate the
			// short-circuit so the turn loop bails the same way the
			// sequential path does. Sibling messages already collected
			// stay in `messages` and are pushed below before we return.
			Ok(Err(err)) => {
				first_err.get_or_insert(err);
			}
			// Join error (panic / cancellation): surface a synthetic
			// errored tool message so the next LLM round-trip still
			// sees a `tool_result` for every `tool_use` — Anthropic
			// 400s the request otherwise.
			Err(join_err) => {
				first_err.get_or_insert_with(|| {
					CoderError::Internal(format!("sub-agent task join error for {}: {join_err}", call.id))
				});
				messages.push(ChatMessage::Tool {
					tool_call_id: call.id.clone(),
					content: json!({ "error": "sub-agent task failed" }).to_string(),
					images: Vec::new(),
				});
			}
		}
	}
	rt.session.lock().await.messages.extend(messages);
	match first_err {
		Some(err) => Err(err),
		None => Ok(()),
	}
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
	// Detached spawn ([ADR 0053]): register, spawn, return a
	// handle. The sub-agent runs on its own root token; its finish
	// wakes the parent via the feeder and its report is collected
	// via `task_collect`. The synchronous path below is unchanged.
	if spec.detach {
		return handle_task_detached(state, sink, tool_call_id, spec).await;
	}
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
			worktree_root: None,
			worker: false,
			detached: false,
		},
	)
	.await;
	let subagent_id_for_record = spec.id.clone();
	let sub_cancel = cancel.child_token();
	// Sub-agents share their parent's `FolderEventSink` — events
	// arrive in the parent's folder bucket on the frontend, which
	// is exactly the multi-session contract: sub-agents belong to
	// whichever project originated them.
	let outcome = run_subagent(
		&state.tools,
		&state.inference,
		sink,
		&state.coder_sessions_dir,
		&state.models,
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

/// Return shape of [`CoderHandle::open_session_boxed`]: the boxed
/// (type-erased) `open_session_impl` future. See that method's doc
/// for why the erasure exists.
type BoxedOpenSession<'a> = std::pin::Pin<
	Box<
		dyn std::future::Future<Output = Result<(SessionSummary, Option<(Vec<CoderEvent>, bool, bool)>), CoderError>>
			+ Send
			+ 'a,
	>,
>;

/// Locate the bound folder whose sessions dir holds `session_id`'s
/// JSONL (ADR 0065). Cross-project workers (`spawn_worker`'s
/// `folder` arg) file their transcripts under *their* parent
/// project, not the coordinator's — restart-time remounts must not
/// assume a single dir, or cross-project fleet members get dropped
/// as "deleted" on rebuild.
async fn find_session_folder(state: &Arc<CoderState>, session_id: &str) -> Option<Utf8PathBuf> {
	for entry in state.workspaces.folders().await {
		let folder = Utf8PathBuf::from(&entry.folder.path);
		let dir = sessions_dir(&state.coder_sessions_dir, &folder);
		if dir.join(format!("{session_id}.jsonl")).is_file() {
			return Some(folder);
		}
	}
	None
}

/// Fold a coordinator's records into its surviving fleet (ADR 0065):
/// a worker is a `SubagentSpawned` carrying `worker: true` (`task`
/// sub-agents never do); a later `WorkerDetached` removes it. The
/// old discriminator (`worktree_root.is_some()`) broke once in-place
/// workers existed — they have no worktree (ADR 0070).
/// Order-preserving and duplicate-free.
fn fold_worker_fleet(records: &[SessionRecord]) -> Vec<String> {
	let mut fleet: Vec<String> = Vec::new();
	for record in records {
		match record {
			SessionRecord::SubagentSpawned {
				subagent_id,
				worker: true,
				..
			} => {
				if !fleet.contains(subagent_id) {
					fleet.push(subagent_id.clone());
				}
			}
			SessionRecord::WorkerDetached { worker_id } => {
				fleet.retain(|w| w != worker_id);
			}
			_ => {}
		}
	}
	fleet
}

/// Append a [`SessionRecord::WorkerDetached`] to the coordinator's
/// JSONL (ADR 0065) so a restart-time fleet rebuild doesn't
/// resurrect the link. Best-effort: an unmounted coordinator is
/// quietly remounted first when the caller can name its folder;
/// failure logs at warn and costs only rebuild accuracy after the
/// *next* restart.
async fn persist_worker_detached(
	state: &Arc<CoderState>,
	orchestrator_id: &str,
	worker_id: &str,
	folder_hint: Option<&str>,
) {
	if state.runtime_for_session(orchestrator_id).await.is_none() {
		if let Some(folder) = folder_hint {
			let handle = CoderHandle { state: state.clone() };
			let _ = handle
				.open_session_boxed(folder.to_string(), orchestrator_id.to_string())
				.await;
		}
	}
	let Some((rt, _)) = state.runtime_for_session(orchestrator_id).await else {
		tracing::warn!(
			orchestrator_id,
			worker_id,
			"coordinator unmounted; worker detach not persisted"
		);
		return;
	};
	persist_parent_record(
		&rt,
		SessionRecord::WorkerDetached {
			worker_id: worker_id.to_string(),
		},
	)
	.await;
}

/// `task` with `detach: true` ([ADR 0053]). Registers the run,
/// spawns it on a fresh root token, and returns a handle
/// immediately — the parent keeps working. The finish feeder
/// wakes the parent when the run settles; `task_collect` fetches
/// the report; `task_abort` / the user-level abort cancel it.
async fn handle_task_detached(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	tool_call_id: &str,
	spec: crate::subagent::Subagent,
) -> Result<Value, CoderError> {
	let subagent_id = spec.id.clone();
	let parent_session_id = spec.parent_session_id.clone();
	// Fresh root token: the run must outlive the spawning turn.
	// The user-level abort walks the registry and cancels this
	// token directly, so "stop everything" still stops it.
	let cancel = CancellationToken::new();
	let entry = state
		.detached_tasks
		.write()
		.await
		.register(&parent_session_id, &subagent_id, cancel.clone());
	// Persist the spawn into the parent's JSONL (same shape as the
	// synchronous path) so the collapsed card + pop-out rebuild on
	// reload, and the parent's `SubagentFinished` record lands when
	// the run settles. Resolve the runtime by the parent's id.
	if let Some((parent_rt, _)) = state.runtime_for_session(&parent_session_id).await {
		persist_parent_record(
			&parent_rt,
			SessionRecord::SubagentSpawned {
				tool_call_id: tool_call_id.to_string(),
				subagent_id: subagent_id.clone(),
				target_folder: spec.folder.folder.path.clone(),
				mode: spec.mode.as_wire().to_string(),
				worktree_root: None,
				worker: false,
				detached: true,
			},
		)
		.await;
	}
	// First detached run for this parent spawns its finish feeder.
	spawn_detached_finish_feeder(state.clone(), parent_session_id.clone());

	let state_for_run = state.clone();
	let sink_for_run = sink.clone();
	let parent_session_id_for_run = parent_session_id.clone();
	let subagent_id_for_run = subagent_id.clone();
	tokio::spawn(async move {
		let outcome = run_subagent(
			&state_for_run.tools,
			&state_for_run.inference,
			&sink_for_run,
			&state_for_run.coder_sessions_dir,
			&state_for_run.models,
			spec,
			cancel,
		)
		.await;
		let finish = match &outcome {
			Ok(report) => DetachedFinish::Done(report.clone()),
			Err(CoderError::Aborted) => DetachedFinish::Aborted,
			Err(err) => DetachedFinish::Failed(err.to_string()),
		};
		DetachedTaskRegistry::settle(&entry, finish).await;
		// Persist the finish into the parent's JSONL, mirroring the
		// synchronous path so a reloaded parent settles the card
		// without lazy-loading the sub-agent's JSONL.
		if let Some((parent_rt, _)) = state_for_run.runtime_for_session(&parent_session_id_for_run).await {
			let finished_record = match &outcome {
				Ok(report) => SessionRecord::SubagentFinished {
					subagent_id: report.sub_session_id.clone(),
					tokens_used_estimate: report.tokens_used_estimate,
					was_error: false,
					result_preview: result_preview_from(&report.result),
				},
				Err(_) => SessionRecord::SubagentFinished {
					subagent_id: subagent_id_for_run,
					tokens_used_estimate: 0,
					was_error: true,
					result_preview: None,
				},
			};
			persist_parent_record(&parent_rt, finished_record).await;
		}
	});
	Ok(json!({
		"detached": true,
		"subagent_id": subagent_id,
		"status": "running",
	}))
}

/// Resolve `subagent_id` to one of the calling session's detached
/// runs ([ADR 0053]) — the shared ownership gate of `task_collect` /
/// `task_steer` / `task_abort`. `register` / `prune_parent` keep
/// `by_parent` and `entries` in lockstep, so an ownership hit
/// implies a live entry — one miss branch covers both "never yours"
/// and "lost to a restart".
async fn own_detached_entry(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	tool: &str,
	subagent_id: &str,
) -> Result<Arc<DetachedEntry>, CoderError> {
	let parent_session_id = rt.session.lock().await.header.id.clone();
	let registry = state.detached_tasks.read().await;
	if registry.is_detached_of(&parent_session_id, subagent_id) {
		if let Some(entry) = registry.entry(subagent_id) {
			return Ok(entry);
		}
	}
	Err(CoderError::invalid_args(
		tool,
		format!(
			"no detached sub-agent `{subagent_id}` for this session — either the id is not one this session's `task({{ detach: true }})` calls returned (a synchronous `task` has no handle), or the in-memory handle was lost to an IDE restart; a finished run's transcript is on disk under the parent session's sub-agent directory"
		),
	))
}

/// `task_collect` — return a detached run's report, optionally
/// blocking up to `wait_ms` for it to settle. Only the parent that
/// spawned the run may collect it.
async fn handle_task_collect(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct CollectArgs {
		subagent_id: String,
		#[serde(default)]
		wait_ms: Option<u64>,
	}
	let parsed: CollectArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("task_collect", err.to_string()))?;
	let entry = own_detached_entry(state, rt, "task_collect", &parsed.subagent_id).await?;
	// Already settled → return the cached finish immediately.
	if let Some(value) = detached_collect_value(&entry).await {
		return Ok(value);
	}
	// Still running. Either report `running` now, or park on the
	// notify until it settles / the wait cap elapses.
	let wait_ms = parsed.wait_ms.unwrap_or(0).min(60_000);
	if wait_ms > 0 {
		let notified = entry.notify.notified();
		tokio::pin!(notified);
		// Enable the notification *before* the timeout race so a
		// settle between the check above and here isn't lost.
		notified.as_mut().enable();
		let _ = tokio::time::timeout(std::time::Duration::from_millis(wait_ms), notified).await;
		if let Some(value) = detached_collect_value(&entry).await {
			return Ok(value);
		}
	}
	Ok(json!({ "status": "running" }))
}

/// Map a settled [`DetachedEntry`] to the `task_collect` result
/// payload, or `None` while it's still running.
async fn detached_collect_value(entry: &DetachedEntry) -> Option<Value> {
	match entry.finish.lock().await.clone() {
		Some(DetachedFinish::Done(report)) => Some(json!({
			"status": "done",
			"result": report.result,
			"sub_session_id": report.sub_session_id,
			"tokens_used_estimate": report.tokens_used_estimate,
			"mode": report.mode.as_wire(),
			"iterations_used": report.iterations_used,
		})),
		Some(DetachedFinish::Failed(error)) => Some(json!({ "status": "error", "error": error })),
		Some(DetachedFinish::Aborted) => Some(json!({ "status": "aborted" })),
		None => None,
	}
}

/// `task_steer` — queue a steering message into a running detached
/// sub-agent (ADR 0071). Rides the pop-out composer's steer channel
/// ([`crate::subagent::queue_subagent_steer`]), tagged
/// `from_coordinator` so the transcript shows the row as agent-sent.
/// Only the parent that spawned the run may steer it, mirroring
/// `task_collect` / `task_abort` ownership.
async fn handle_task_steer(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct SteerArgs {
		subagent_id: String,
		text: String,
	}
	let parsed: SteerArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("task_steer", err.to_string()))?;
	if parsed.text.trim().is_empty() {
		return Err(CoderError::invalid_args("task_steer", "text must not be empty"));
	}
	own_detached_entry(state, rt, "task_steer", &parsed.subagent_id).await?;
	// The steer channel exists exactly while the run's loop is
	// live, so a queue miss means the run already settled.
	if !crate::subagent::queue_subagent_steer(&parsed.subagent_id, parsed.text, true) {
		return Ok(json!({
			"status": "not_running",
			"hint": "the run already settled; call `task_collect` for its report",
		}));
	}
	Ok(json!({ "status": "steered" }))
}

/// `task_abort` — cancel a running detached sub-agent's own token.
/// Scoped to the one run; never touches the parent turn or its
/// sibling sub-agents.
async fn handle_task_abort(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct AbortArgs {
		subagent_id: String,
	}
	let parsed: AbortArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("task_abort", err.to_string()))?;
	let entry = own_detached_entry(state, rt, "task_abort", &parsed.subagent_id).await?;
	if entry.finish.lock().await.is_some() {
		return Ok(json!({ "status": "not_running" }));
	}
	entry.cancel.cancel();
	Ok(json!({ "status": "aborted" }))
}

/// Per-parent background task that watches the event broadcast for
/// `SubagentFinished` from this parent's detached sub-agents and
/// injects a wake message into the parent's session ([ADR 0053]).
/// The wake is a pointer, not the report — the parent calls
/// `task_collect` for the content, preserving `task`'s
/// context-preservation property. Exits when the broadcast closes.
fn spawn_detached_finish_feeder(state: Arc<CoderState>, parent_session_id: String) {
	// Spawn the feeder once per parent. A first detached spawn for
	// a parent flips this flag; later spawns reuse the running
	// feeder. Reuse the coordinator-workers pattern of "one
	// feeder per orchestrator" rather than a feeder per run.
	static FEEDERS: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> = std::sync::OnceLock::new();
	let feeders = FEEDERS.get_or_init(|| std::sync::Mutex::new(HashSet::new()));
	if !feeders
		.lock()
		.expect("detached feeder registry poisoned")
		.insert(parent_session_id.clone())
	{
		return;
	}
	let handle = CoderHandle { state: state.clone() };
	let mut rx = handle.subscribe();
	tokio::spawn(async move {
		loop {
			let recv = rx.recv().await;
			let Ok(envelope) = recv else { continue };
			// Only detached runs of *this* parent wake it.
			let CoderEvent::SubagentFinished {
				subagent_id, was_error, ..
			} = &envelope.event
			else {
				continue;
			};
			let is_ours = state
				.detached_tasks
				.read()
				.await
				.is_detached_of(&parent_session_id, subagent_id);
			if !is_ours {
				continue;
			}
			// The report is cached under the same registry entry;
			// `task_collect` returns it (or the error) verbatim.
			let status = if *was_error { "error" } else { "done" };
			let text = format!(
				"Detached sub-agent {subagent_id} finished (status: {status}). Call `task_collect(\"{subagent_id}\")` to fetch its report, or ignore it if you no longer need the result."
			);
			// Best-effort: the parent may be gone (its session
			// deleted). The wake is a pointer, not the report —
			// the cached entry must survive it, because the parent
			// is typically mid-turn here and only reaches its
			// `task_collect` one or more LLM round-trips later.
			// Entries are pruned when the parent session is
			// deleted, not before.
			let _ = handle.send_to(&parent_session_id, text, Vec::new()).await;
		}
	});
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
		name: String,
		#[serde(default)]
		base_branch: Option<String>,
		#[serde(default)]
		folder: Option<String>,
		#[serde(default)]
		worktree: Option<bool>,
	}
	let parsed: SpawnWorkerArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("spawn_worker", err.to_string()))?;
	let task = parsed.task.trim();
	if task.is_empty() {
		return Err(CoderError::invalid_args("spawn_worker", "task must not be empty"));
	}
	let use_worktree = parsed.worktree.unwrap_or(true);
	// An in-place worker runs on whatever branch the shared tree
	// has checked out — switching it to `base_branch` would yank
	// the tree out from under the user (and any sibling in-place
	// worker). Refuse the combination instead of guessing.
	if !use_worktree && parsed.base_branch.is_some() {
		return Err(CoderError::invalid_args(
			"spawn_worker",
			"base_branch requires a worktree — an in-place worker (worktree: false) works on the branch the folder already has checked out",
		));
	}
	// The name becomes the branch / worktree / session chip (ADR
	// 0042), so reject one that slugs to nothing rather than
	// silently falling back to an opaque `moon/agent-<id>`.
	if worker_branch_slug(&parsed.name).is_none() {
		return Err(CoderError::invalid_args(
			"spawn_worker",
			"name must contain at least one letter or digit — it becomes the worker's branch name",
		));
	}
	// Resolve the parent folder for the worktree. The coordinator's
	// own folder (the sink's) is the default; `folder` overrides it
	// so a worker can be spawned in a project the coordinator just
	// created via `init_repo` / `clone_repo`. Either way the parent
	// is pinned to the session's bound folder, never the live active
	// folder — the user may have switched projects while this turn
	// was running.
	let parent_folder = match &parsed.folder {
		Some(f) => {
			let path = f.trim();
			if path.is_empty() {
				return Err(CoderError::invalid_args(
					"spawn_worker",
					"folder must not be empty when provided",
				));
			}
			// Verify the folder is bound in the workspace — a
			// folder that isn't bound can't host a worktree.
			if state.workspaces.folder_for_path(path).await.is_none() {
				return Err(CoderError::invalid_args(
					"spawn_worker",
					format!("folder `{path}` is not a bound workspace folder; create it first with `init_repo` or `clone_repo`, or omit `folder` to use the coordinator's own project"),
				));
			}
			path.to_string()
		}
		None => sink.folder().to_string(),
	};
	// Mint the worker as an ordinary `Agent` session — in a worktree
	// (the default) or in place against the target folder itself
	// (ADR 0070). A sub-orchestrator would pass `Coordinator` here,
	// but that's a later-scale concern; v1 workers are plain agents.
	let handle = CoderHandle { state: state.clone() };
	let summary = if use_worktree {
		handle
			.create_worktree_session(
				parsed.base_branch,
				Some(parsed.name),
				CoderMode::Agent,
				Some(parent_folder.clone()),
			)
			.await?
			.0
	} else {
		handle
			.create_in_place_worker_session(&parsed.name, &parent_folder, CoderMode::Agent)
			.await?
	};
	// `target_folder` is the worktree path the worker operates
	// against — the same shape `SubagentSpawned.target_folder`
	// carries for `task` sub-agents (their folder path), not the
	// branch name.
	let parent_folder_for_note = parent_folder.clone();
	let target_folder = summary.worktree_root.clone().unwrap_or(parent_folder);
	// Register the worker, persist the spawn record, and announce the
	// `SubagentSpawned` **before** seeding the worker's first turn.
	// That first `send_to` fires the worker's `SessionLoaded`
	// (`persisted_records == 0`), and the frontend's "don't hijack the
	// visible session for a coordinator-spawned worker" guard detects
	// the worker by the `SubagentSpawned` already sitting in the
	// coordinator's bucket. Emitting the spawn *after* `send_to` (the
	// old order) raced that guard: the worker's `SessionLoaded` landed
	// first, read as a plain open, and the panel jumped to the worker.
	let orchestrator_id = sink.session_id.clone();
	// Stamp the reverse link (ADR 0065) before the seed send persists
	// the header: a restarted process re-links the fleet from disk,
	// and this field is the worker-side half (the coordinator side
	// rebuilds from its own spawn/detach records).
	if let Some((worker_rt, _)) = state.runtime_for_session(&summary.id).await {
		worker_rt.session.lock().await.header.orchestrator_session_id = Some(orchestrator_id.clone());
	}
	let spawn_feeder = state
		.coordinator_workers
		.write()
		.await
		.register(&orchestrator_id, &summary.id);
	if spawn_feeder {
		spawn_dispatch_feeder(state.clone(), orchestrator_id.clone());
	}
	// Persist the spawn into the coordinator's JSONL right away
	// (before the worker's first turn) so a crash / kill mid-worker
	// still leaves a record the coordinator can replay. The on-disk
	// record mirrors `CoderEvent::SubagentSpawned` so replay needs no
	// shape conversion. Best-effort: a write failure logs at warn but
	// doesn't fail the spawn.
	if let Some((orchestrator_rt, _)) = state.runtime_for_session(&orchestrator_id).await {
		persist_parent_record(
			&orchestrator_rt,
			SessionRecord::SubagentSpawned {
				tool_call_id: tool_call_id.to_string(),
				subagent_id: summary.id.clone(),
				target_folder: target_folder.clone(),
				mode: CoderMode::Agent.as_wire().to_string(),
				worktree_root: summary.worktree_root.clone(),
				// The fleet-rebuild discriminator (ADR 0070) — an
				// in-place worker has no worktree_root to fold on.
				worker: true,
				// A coordinator worker is a top-level session, not a
				// detached `task` run — the flag stays off.
				detached: false,
			},
		)
		.await;
	}
	sink.send(CoderEvent::SubagentSpawned {
		tool_call_id: tool_call_id.to_string(),
		subagent_id: summary.id.clone(),
		target_folder,
		mode: CoderMode::Agent.as_wire().to_string(),
		worktree_root: summary.worktree_root.clone(),
		worker: true,
		detached: false,
	});
	// Seed the worker with the task prompt. On failure, roll back the
	// registration + spawn record so the coordinator isn't left holding
	// a worker that never started — the emitted `SubagentSpawned` is a
	// UI concern and is left (the card renders the seed error via the
	// orchestrator's tool_result, which the turn emits on this `Err`).
	if let Err(err) = handle
		.send_to_as_coordinator(&summary.id, task.to_string(), Vec::new())
		.await
	{
		state
			.coordinator_workers
			.write()
			.await
			.remove(&orchestrator_id, &summary.id);
		// The spawn record is already on disk; without a matching
		// detach a restart would resurrect this never-started worker.
		persist_worker_detached(state, &orchestrator_id, &summary.id, None).await;
		return Err(err);
	}
	// The worker's worktree just became a bound folder; the folder bar
	// has no other way to learn about a bind it didn't initiate
	// (ADR 0044). An in-place worker binds nothing — skip the poke.
	if use_worktree {
		sink.send(CoderEvent::WorkspaceFoldersChanged);
	}
	let mut result = json!({
		"worker_id": summary.id,
		"branch": summary.worktree_branch,
		"worktree_path": summary.worktree_root,
		"title": summary.title,
	});
	if !use_worktree {
		// Make the shared-tree situation explicit in the tool result
		// so the coordinator doesn't reason as if the worker had an
		// isolated checkout.
		result.as_object_mut().expect("spawn result object").insert(
			"in_place".into(),
			json!("worker runs directly in the folder's checked-out tree — its edits and commits land on the current branch, alongside any other session working there"),
		);
	}
	// The worktree rides its parent folder's bind mount — if the
	// running container doesn't mount the parent (a repo created via
	// `init_repo` / `clone_repo` after the container came up), tell
	// the coordinator the worker runs with the host toolchain.
	attach_container_mount_note(state, &parent_folder_for_note, &mut result).await;
	Ok(result)
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
			let worker_id = envelope.session_id.clone();
			// Is this envelope from one of our attached workers? A
			// user message into a worker doesn't unhook it (ADR
			// 0043); an explicit disconnect does (ADR 0052).
			let is_our_worker = state
				.coordinator_workers
				.read()
				.await
				.feeds(&orchestrator_id, &worker_id);
			if is_our_worker {
				// Only forward events that warrant a wake — not every
				// streaming delta. `TurnComplete` (the worker finished
				// its turn) is the one the ADR names as the primary
				// wake signal; the orchestrator then calls
				// `observe_worker` for a snapshot. A parked `ask_user`
				// is the other (ADR 0030 names it too): the worker is
				// blocked until someone answers, and without a wake the
				// coordinator would only find out on a poll it has no
				// reason to make — the user ends up answering by hand.
				match &envelope.event {
					CoderEvent::TurnComplete => {
						// Lead with the worker's name (not the opaque id) and carry
						// the live fleet count so the coordinator knows how many
						// workers are still going without polling.
						let label = worker_label(&state, &worker_id).await;
						let remaining = state.coordinator_workers.read().await.attached_count(&orchestrator_id);
						let _ = handle
							.send_to(
								&orchestrator_id,
								format!(
									"Worker {label} completed a turn. Use `observe_worker` to see its current state. \
									 ({remaining} worker(s) still on your fleet — `list_workers` for the full picture.)"
								),
								Vec::new(),
							)
							.await;
					}
					CoderEvent::ToolCall { name, args, .. } if name == "ask_user" => {
						// Carry the questions inline so the coordinator
						// can answer straight away instead of spending a
						// round-trip on `observe_worker` first.
						let label = worker_label(&state, &worker_id).await;
						let questions = truncate_for_notice(&args.to_string(), WORKER_PROMPT_NOTICE_MAX);
						let _ = handle
							.send_to(
								&orchestrator_id,
								format!(
									"Worker {label} is paused on an `ask_user` prompt and waits for an answer:\n\
									 {questions}\n\
									 Answer it with `respond_to_worker_prompt` (answers keyed by question id), \
									 or leave it for the user only if it is genuinely their call."
								),
								Vec::new(),
							)
							.await;
					}
					_ => {}
				}
				continue;
			}
			// Not an attached worker. A **disconnected** one still
			// earns exactly one final `TurnComplete` wake (ADR 0052):
			// the disconnect command cuts the link without touching
			// the worker's in-flight turn, and the orchestrator needs
			// to hear that the worker left its fleet or it would keep
			// waiting on it. `remove` drops the link entirely as part
			// of the same check, so this fires once and everything
			// else the disconnected worker emits stays silent.
			if !matches!(envelope.event, CoderEvent::TurnComplete) {
				continue;
			}
			let was_registered = state
				.coordinator_workers
				.write()
				.await
				.remove(&orchestrator_id, &worker_id);
			if !was_registered {
				continue;
			}
			// Carry the branch snapshot so the coordinator can re-plan from
			// the handover, not a black box (ADR 0056).
			let snapshot = worker_branch_snapshot(&state, &worker_id).await;
			let label = worker_label(&state, &worker_id).await;
			let state_line = snapshot.map(|s| format!(" Final state: {s}.")).unwrap_or_default();
			let _ = handle
				.send_to(
					&orchestrator_id,
					format!(
						"Worker {label} was disconnected by the user and its in-flight turn has now finished. \
						 It is no longer attached to you: its updates won't reach you any more and your control \
						 tools (steer / abort / commit / merge / respond) refuse it. Its session, branch, and \
						 worktree are untouched — the user owns it from here. Don't wait on it; adjust your \
						 plan.{state_line}"
					),
					Vec::new(),
				)
				.await;
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

/// `list_workers` — the coordinator's fleet inventory. Reads the
/// orchestrator → worker registry (the source of truth the feeder also
/// uses) and returns one `WorkerSnapshot` per registered worker plus an
/// attached / disconnected count, so the coordinator can stay on top of
/// its fleet from a single call instead of re-deriving it in `todo_write`
/// / a scratchpad (which can drift from the real registry) or polling
/// `observe_worker` per id. Running / idle / needs-input / attached state
/// and the per-worker `behind_default` make the "re-triage every
/// in-flight worker after a merge" loop one call.
async fn handle_list_workers(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct ListArgs {
		/// Include workers the user disconnected (still registered but
		/// no longer driven). Default false — the live fleet.
		#[serde(default)]
		include_disconnected: bool,
	}
	let parsed: ListArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("list_workers", err.to_string()))?;
	// The coordinator's own session id is the registry key. `sink` is the
	// session the tool is running in, which *is* the orchestrator.
	let orchestrator_id = sink.session_id().to_string();
	let mut workers = state.coordinator_workers.read().await.workers_of(&orchestrator_id);
	// Deterministic order for the model: attached first, then by id.
	workers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
	let handle = CoderHandle { state: state.clone() };
	let mut rows = Vec::new();
	let mut attached = 0usize;
	let mut disconnected = 0usize;
	for (worker_id, is_attached) in workers {
		if is_attached {
			attached += 1;
		} else {
			disconnected += 1;
			if !parsed.include_disconnected {
				continue;
			}
		}
		let Some(snapshot) = handle.observe_session(&worker_id).await else {
			// Unmounted (e.g. a restart dropped the runtime) — keep the
			// row minimal rather than dropping the worker silently.
			rows.push(json!({
				"worker_id": worker_id,
				"attached": is_attached,
				"mounted": false,
			}));
			continue;
		};
		// Cheap staleness signal for the re-triage loop: how far behind
		// the default branch this worker's base is. Best-effort.
		let behind_default = worker_behind_default(state, &worker_id).await;
		let mut row = serde_json::to_value(&snapshot).unwrap_or_else(|_| json!({}));
		row["worker_id"] = json!(worker_id);
		row["attached"] = json!(is_attached);
		row["mounted"] = json!(true);
		if let Some(b) = behind_default {
			row["behind_default"] = json!(b);
		}
		rows.push(row);
	}
	Ok(json!({
		"orchestrator_id": orchestrator_id,
		"attached": attached,
		"disconnected": disconnected,
		"workers": rows,
	}))
}

/// How far a worker's branch is behind the repo's default branch
/// (`default_branch_behind` from `git_branch`), for the fleet re-triage
/// row. Best-effort: `None` when the folder / git is unavailable.
async fn worker_behind_default(state: &Arc<CoderState>, worker_id: &str) -> Option<u32> {
	let (rt, folder_path) = state.runtime_for_session(worker_id).await?;
	let routing_path = {
		let session = rt.session.lock().await;
		match session.header.worktree_root.clone() {
			Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
			_ => folder_path.to_string(),
		}
	};
	let folder = state.workspaces.folder_for_path(&routing_path).await?;
	let branch = folder.host.git_branch().await.ok()?;
	Some(branch.default_branch_behind)
}

/// Refuse a coordinator control tool targeting a worker the user
/// disconnected (ADR 0052): once unhooked, the coordinator may no
/// longer act on it. Sessions no coordinator spawned (or spawned and
/// fully released after the final wake) sail through — nothing here
/// gates a coordinator steering a session that was never its worker.
async fn ensure_worker_still_attached(state: &Arc<CoderState>, tool: &str, worker_id: &str) -> Result<(), CoderError> {
	if state.coordinator_workers.read().await.controls(worker_id) {
		return Ok(());
	}
	Err(CoderError::invalid_args(
		tool,
		format!("worker `{worker_id}` was disconnected by the user — it is no longer attached to you; leave it alone"),
	))
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
	ensure_worker_still_attached(state, "steer_worker", &parsed.worker_id).await?;
	let handle = CoderHandle { state: state.clone() };
	handle
		.send_to_as_coordinator(&parsed.worker_id, parsed.text, Vec::new())
		.await?;
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
	ensure_worker_still_attached(state, "abort_worker", &parsed.worker_id).await?;
	let handle = CoderHandle { state: state.clone() };
	handle.abort_session(&parsed.worker_id).await;
	Ok(json!({ "status": "aborted" }))
}

/// Convert `respond_to_worker_prompt`'s `answers` into the
/// `PromptResponse` sequence shape. Accepts both the documented map
/// form (question id → option id / custom string / array of either)
/// and the raw `ask_user`-response array form. `prompt_args` is the
/// parked `ask_user`'s arguments when recoverable from the worker's
/// transcript — a map value matching one of the question's option
/// ids becomes a `selected` entry, anything else lands in
/// `free_text` (always readable by the worker either way).
fn answers_to_prompt_response(answers: &Value, prompt_args: Option<&Value>) -> Result<Vec<QuestionAnswer>, String> {
	if answers.is_array() {
		return serde_json::from_value(answers.clone()).map_err(|e| e.to_string());
	}
	let Some(map) = answers.as_object() else {
		return Err(
			"`answers` must be a map of question id → answer (option id or custom text), \
			 or an array of {question_id, selected, free_text}"
				.into(),
		);
	};
	// Option ids per question id, recovered from the parked
	// `ask_user` args. Missing args degrade to everything-free-text.
	let mut option_ids: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
		std::collections::HashMap::new();
	if let Some(questions) = prompt_args.and_then(|a| a.get("questions")).and_then(|q| q.as_array()) {
		for q in questions {
			let Some(qid) = q.get("id").and_then(|v| v.as_str()) else {
				continue;
			};
			let ids = q
				.get("options")
				.and_then(|o| o.as_array())
				.map(|opts| {
					opts
						.iter()
						.filter_map(|o| o.get("id").and_then(|v| v.as_str()))
						.collect()
				})
				.unwrap_or_default();
			option_ids.insert(qid, ids);
		}
	}
	let mut out = Vec::new();
	for (qid, value) in map {
		let is_option = |s: &str| option_ids.get(qid.as_str()).is_some_and(|ids| ids.contains(s));
		let mut selected = Vec::new();
		let mut free = Vec::new();
		let mut classify = |s: &str| {
			if is_option(s) {
				selected.push(s.to_string());
			} else {
				free.push(s.to_string());
			}
		};
		match value {
			Value::String(s) => classify(s),
			Value::Array(items) => {
				for item in items {
					let Some(s) = item.as_str() else {
						return Err(format!("answer array for `{qid}` must contain strings"));
					};
					classify(s);
				}
			}
			Value::Bool(b) => free.push(b.to_string()),
			Value::Number(n) => free.push(n.to_string()),
			_ => return Err(format!("answer for `{qid}` must be a string or an array of strings")),
		}
		out.push(QuestionAnswer {
			question_id: qid.clone(),
			selected,
			free_text: free.join("\n"),
		});
	}
	Ok(out)
}

/// `respond_to_worker_prompt` — answer a worker's parked `ask_user`.
/// Routes through the existing `respond_to_prompt` by-call-id scan
/// (which already targets any session, not just the visible one).
async fn handle_respond_to_worker_prompt(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct RespondArgs {
		worker_id: String,
		answers: Value,
	}
	let parsed: RespondArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("respond_to_worker_prompt", err.to_string()))?;
	ensure_worker_still_attached(state, "respond_to_worker_prompt", &parsed.worker_id).await?;
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
	// Recover the parked `ask_user`'s args (same transcript scan as
	// `observe_session`) so map-form answers can tell option ids
	// apart from free text.
	let prompt_args = {
		let session = rt.session.lock().await;
		session.messages.iter().rev().find_map(|m| match m {
			ChatMessage::Assistant { tool_calls, .. } => tool_calls
				.iter()
				.find(|c| c.id == call_id)
				.and_then(|c| serde_json::from_str::<Value>(&c.function.arguments).ok()),
			_ => None,
		})
	};
	let answers = answers_to_prompt_response(&parsed.answers, prompt_args.as_ref())
		.map_err(|err| CoderError::invalid_args("respond_to_worker_prompt", err))?;
	let response = PromptResponse { answers };
	let resolved = handle.respond_to_prompt(&call_id, response).await;
	Ok(json!({ "status": if resolved { "answered" } else { "not_resolved" } }))
}

/// `review_worker_changes` — pull the full per-turn diff for a worker,
/// optionally scoped to specific files. The deliberate-pull complement
/// to `observe_worker`'s diff summary (ADR 0030).
async fn handle_review_worker_changes(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct ReviewArgs {
		worker_id: String,
		#[serde(default)]
		files: Option<Vec<String>>,
	}
	let parsed: ReviewArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("review_worker_changes", err.to_string()))?;
	let Some((rt, _)) = state.runtime_for_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"review_worker_changes",
			format!("no mounted session for worker_id `{}`", parsed.worker_id),
		));
	};
	let session = rt.session.lock().await;
	let Some((all_files, diff)) = session.last_turn_diff.as_ref() else {
		return Ok(json!({ "diff": "", "note": "no turn diff available yet" }));
	};
	// If the caller specified files, filter the diff to just those.
	// Otherwise return the full diff as stored.
	let result_diff = match &parsed.files {
		Some(requested) => {
			let requested_set: std::collections::HashSet<&str> = requested.iter().map(|s| s.as_str()).collect();
			filter_diff_to_files(diff, &requested_set)
		}
		None => diff.clone(),
	};
	let result_files: Vec<&String> = match &parsed.files {
		Some(requested) => all_files.iter().filter(|f| requested.contains(f)).collect(),
		None => all_files.iter().collect(),
	};
	Ok(json!({
		"files": result_files,
		"diff": result_diff,
	}))
}

/// Extract the hunks from `diff` that belong to any file in `files`.
/// Splits on `diff --git` headers and keeps only hunks whose header
/// references a requested file. Returns the filtered diff text.
fn filter_diff_to_files(diff: &str, files: &std::collections::HashSet<&str>) -> String {
	let mut out = String::new();
	let mut current_block: Vec<&str> = Vec::new();
	let mut current_matches = false;
	for line in diff.lines() {
		if line.starts_with("diff --git") {
			if current_matches && !current_block.is_empty() {
				for block_line in &current_block {
					out.push_str(block_line);
					out.push('\n');
				}
			}
			current_block.clear();
			current_matches = files.iter().any(|f| line.contains(&format!(" b/{f}")));
		}
		current_block.push(line);
	}
	if current_matches && !current_block.is_empty() {
		for block_line in &current_block {
			out.push_str(block_line);
			out.push('\n');
		}
	}
	out
}

/// `workspace_scm_status` — read-only SCM state for a worker's
/// worktree (or the main folder). Composes branch info, file change
/// counts, and the file list into one compact snapshot (ADR 0030).
async fn handle_workspace_scm_status(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct ScmStatusArgs {
		#[serde(default)]
		worker_id: Option<String>,
	}
	let parsed: ScmStatusArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("workspace_scm_status", err.to_string()))?;
	// Resolve the folder to query. If `worker_id` is provided,
	// resolve the worker's worktree root; otherwise use the
	// coordinator's own folder (never the live active folder — the
	// user may have switched projects mid-turn).
	let routing_path = match &parsed.worker_id {
		Some(worker_id) => {
			let Some((rt, _)) = state.runtime_for_session(worker_id).await else {
				return Err(CoderError::invalid_args(
					"workspace_scm_status",
					format!("no mounted session for worker_id `{worker_id}`"),
				));
			};
			let session = rt.session.lock().await;
			match session.header.worktree_root.clone() {
				Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
				_ => {
					let Some((_, folder_path)) = state.runtime_for_session(worker_id).await else {
						return Err(CoderError::invalid_args(
							"workspace_scm_status",
							"could not resolve folder for worker",
						));
					};
					folder_path.to_string()
				}
			}
		}
		None => sink.folder().to_string(),
	};
	let Some(folder) = state.folder_entry_for(&routing_path).await else {
		return Err(CoderError::invalid_args(
			"workspace_scm_status",
			format!("no bound folder for path `{routing_path}`"),
		));
	};
	// Compose the three git calls. Best-effort — individual failures
	// produce empty/default values rather than erroring the whole tool.
	let branch = folder.host.git_branch().await.unwrap_or_default();
	let entries = folder.host.git_status_entries(&[]).await.unwrap_or_default();
	// Fold entries into aggregate counts.
	let mut added = 0u32;
	let mut modified = 0u32;
	let mut deleted = 0u32;
	let files: Vec<Value> = entries
		.iter()
		.filter(|e| !matches!(e.status, moon_protocol::git::GitFileStatus::Ignored))
		.map(|e| {
			match e.status {
				moon_protocol::git::GitFileStatus::Added | moon_protocol::git::GitFileStatus::Untracked => added += 1,
				moon_protocol::git::GitFileStatus::Modified | moon_protocol::git::GitFileStatus::Conflicted => modified += 1,
				moon_protocol::git::GitFileStatus::Deleted => deleted += 1,
				moon_protocol::git::GitFileStatus::Ignored => {}
			}
			json!({
				"path": e.path,
				"status": format!("{:?}", e.status).to_lowercase(),
			})
		})
		.collect();
	Ok(json!({
	"branch": {
		"name": branch.name,
		"head_short_sha": branch.head_short_sha,
		"has_upstream": branch.has_upstream,
		"ahead": branch.ahead,
		"behind": branch.behind,
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

/// `check_worker_base` — the rebase-before-merge / rebase-before-PR
/// gate. Resolves the worker's worktree, runs the host's base check
/// (fetch + behind-count + three-dot numstat vs the default branch),
/// then cross-references the diff's deleted files against the files the
/// worker actually touched (its last turn diff) to flag the stale-base
/// revert tripwire: a file with deletions the worker didn't write means
/// merging / PR-ing the branch would re-delete work that landed on the
/// default after the branch's base.
async fn handle_check_worker_base(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct CheckArgs {
		worker_id: String,
	}
	let parsed: CheckArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("check_worker_base", err.to_string()))?;
	let Some((rt, _)) = state.runtime_for_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"check_worker_base",
			format!("no mounted session for worker_id `{}`", parsed.worker_id),
		));
	};
	// Resolve the worker's worktree (falling back to its session folder
	// for a non-worktree worker), and grab the files its last turn
	// touched — the "did the worker write this?" reference set.
	let (routing_path, worker_files) = {
		let session = rt.session.lock().await;
		let touched: HashSet<String> = session
			.last_turn_diff
			.as_ref()
			.map(|(files, _)| files.iter().cloned().collect())
			.unwrap_or_default();
		let path = match session.header.worktree_root.clone() {
			Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
			_ => {
				let Some((_, folder_path)) = state.runtime_for_session(&parsed.worker_id).await else {
					return Err(CoderError::invalid_args(
						"check_worker_base",
						"could not resolve folder for worker",
					));
				};
				folder_path.to_string()
			}
		};
		(path, touched)
	};
	let Some(folder) = state.folder_entry_for(&routing_path).await else {
		return Err(CoderError::invalid_args(
			"check_worker_base",
			format!("no bound folder for path `{routing_path}`"),
		));
	};
	let Some(check) = folder.host.git_base_check().await? else {
		return Ok(json!({
			"has_base": false,
			"note": "no default remote branch (local-only repo) — there's no origin/main to drift from",
		}));
	};
	// The revert tripwire. A diff file with deletions the worker didn't
	// touch is a revert candidate. Two caveats keep this honest:
	// - `last_turn_diff` only covers the *latest* turn, so a worker that
	//   legitimately edited a file several turns back reads as "didn't
	//   touch it". We mark the verdict heuristic, not proof.
	// - A file the worker *did* delete on purpose shows deletions on a
	//   path it touched — correctly not flagged.
	let touched_unknown = worker_files.is_empty();
	let revert_suspects: Vec<Value> = check
		.files
		.iter()
		.filter(|f| f.deletions > 0 && !worker_files.contains(&f.path))
		.map(|f| {
			json!({
				"path": f.path,
				"additions": f.additions,
				"deletions": f.deletions,
				"worker_touched": worker_files.contains(&f.path),
			})
		})
		.collect();
	let stale = check.behind_default > 0;
	let flagged = !revert_suspects.is_empty();
	let verdict = if flagged {
		format!(
				"STALE-BASE REVERT RISK: the branch is {} commit(s) behind {} and its diff deletes lines in {} file(s) the worker didn't write — merging or PR-ing it would re-delete work that merged after its base. Do NOT merge / PR as-is; steer the worker to rebase onto current {} first, then re-check.",
				check.behind_default,
				check.default_branch_remote_ref,
				revert_suspects.len(),
				check.default_branch_remote_ref,
			)
	} else if stale {
		format!(
				"Behind: the branch is {} commit(s) behind {} but its diff doesn't delete files the worker didn't write. Likely safe, but a rebase onto current {} keeps the history clean.",
				check.behind_default, check.default_branch_remote_ref, check.default_branch_remote_ref,
			)
	} else {
		"Fresh: the branch is up to date with the default branch; its diff touches only the worker's own changes. Safe to merge / PR.".to_string()
	};
	Ok(json!({
		"has_base": true,
		"default_branch_remote_ref": check.default_branch_remote_ref,
		"behind_default": check.behind_default,
		"stale": stale,
		"revert_suspects": revert_suspects,
		"flagged": flagged,
		// Heuristic, not proof: based on the worker's *last* turn diff.
		"verdict_basis": if touched_unknown {
			"no worker turn-diff recorded — the revert check couldn't compare against the worker's touched files, so suspects are every deleted file in the diff"
		} else {
			"compared the diff's deleted files against the files the worker touched on its last turn"
		},
		"verdict": verdict,
	}))
}

/// `commit_worker_changes` — checkpoint a worker's uncommitted work
/// with a git commit (ADR 0030). Resolves the worker's worktree,
/// optionally AI-suggests a commit message from the diff, then runs
/// `git add -A` + `git commit` — the same flow the IDE's SCM panel
/// uses.
async fn handle_commit_worker_changes(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct CommitArgs {
		worker_id: String,
		#[serde(default)]
		message: Option<String>,
	}
	let parsed: CommitArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("commit_worker_changes", err.to_string()))?;
	ensure_worker_still_attached(state, "commit_worker_changes", &parsed.worker_id).await?;
	let Some((rt, _)) = state.runtime_for_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"commit_worker_changes",
			format!("no mounted session for worker_id `{}`", parsed.worker_id),
		));
	};
	let worktree_root = rt.session.lock().await.header.worktree_root.clone();
	let routing_path = match worktree_root {
		Some(root) if state.workspaces.folder_for_path(&root).await.is_some() => root,
		_ => {
			let Some((_, folder_path)) = state.runtime_for_session(&parsed.worker_id).await else {
				return Err(CoderError::invalid_args(
					"commit_worker_changes",
					"could not resolve folder for worker",
				));
			};
			folder_path.to_string()
		}
	};
	let Some(folder) = state.workspaces.folder_for_path(&routing_path).await else {
		return Err(CoderError::invalid_args(
			"commit_worker_changes",
			format!("no bound folder for path `{routing_path}`"),
		));
	};
	// Determine the commit message. If the caller provided one, use
	// it. Otherwise, pull the diff and AI-suggest a message.
	let message = match &parsed.message {
		Some(msg) if !msg.trim().is_empty() => msg.trim().to_owned(),
		_ => {
			let diff = folder.host.git_diff_patch().await.unwrap_or_default();
			if diff.is_empty() {
				return Err(CoderError::invalid_args(
					"commit_worker_changes",
					"nothing to commit — working tree is clean",
				));
			}
			suggest_commit_message_from_state(state, &diff).await?
		}
	};
	let result = folder.host.git_commit(&message, false).await?;
	Ok(json!({
		"short_sha": result.short_sha,
		"summary": result.summary,
	}))
}

/// `merge_worker_changes` — merge a worker's branch into a base
/// branch on the parent repo (ADR 0037). Switches the parent repo to
/// `base_branch` (default `main`), then runs `git merge --no-edit
/// <worker_branch>` on the parent's host. The worker's worktree and
/// branch are left intact — this only lands the commits, it doesn't
/// clean up the worktree.
async fn handle_merge_worker_changes(state: &Arc<CoderState>, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct MergeArgs {
		worker_id: String,
		#[serde(default)]
		base_branch: Option<String>,
	}
	let parsed: MergeArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("merge_worker_changes", err.to_string()))?;
	ensure_worker_still_attached(state, "merge_worker_changes", &parsed.worker_id).await?;
	let Some((rt, _)) = state.runtime_for_session(&parsed.worker_id).await else {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			format!("no mounted session for worker_id `{}`", parsed.worker_id),
		));
	};
	let (worktree_root, worktree_branch) = {
		let session = rt.session.lock().await;
		(
			session.header.worktree_root.clone(),
			session.header.worktree_branch.clone(),
		)
	};
	let Some(worktree_path) = worktree_root else {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			"worker has no worktree — an in-place worker's commits land directly on the folder's checked-out branch, so there is nothing to merge",
		));
	};
	let Some(branch) = worktree_branch else {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			"worker has no worktree_branch — cannot merge without a branch name",
		));
	};
	// Resolve the worktree folder to find its parent.
	let Some(wt_entry) = state.workspaces.folder_for_path(&worktree_path).await else {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			format!("worktree folder `{worktree_path}` is not bound"),
		));
	};
	let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = wt_entry.folder.origin.clone() else {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			format!("`{worktree_path}` is not a worktree folder"),
		));
	};
	let Some(parent) = state.workspaces.folder_for_path(&parent_path).await else {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			"the worktree's parent folder is not bound; can't merge",
		));
	};
	let base = parsed.base_branch.as_deref().unwrap_or("main").trim();
	if base.is_empty() {
		return Err(CoderError::invalid_args(
			"merge_worker_changes",
			"base_branch must not be empty",
		));
	}
	// Switch the parent to the base branch, then merge the worker's
	// branch into it. Both run on the parent's host so they share the
	// same git index lock.
	parent
		.host
		.branch_switch(&moon_protocol::git::BranchSwitchTarget::Local { name: base.to_string() })
		.await?;
	parent.host.git_merge_default_branch(&branch).await?;
	Ok(json!({
		"merged_branch": branch,
		"into": base,
	}))
}

/// `discard_worker_worktree` — remove a finished worker's checkout and
/// unbind its folder (ADR 0044). The branch is kept; the worker's
/// session stays in the list and falls back to the parent project.
///
/// This is the agent-side twin of the `coder_discard_worktree` Tauri
/// command, and shares its idempotence: a checkout that's already gone
/// from disk is forgotten (stale git metadata pruned) rather than
/// erroring.
/// Normalise a fleet tool's target list: `worker_id` (one) and/or
/// `worker_ids` (several), deduplicated in order. Erroring on an
/// empty set keeps "forgot both fields" a loud arg failure instead
/// of a silent no-op.
fn parse_worker_ids(tool: &str, args: &Value) -> Result<Vec<String>, CoderError> {
	#[derive(serde::Deserialize)]
	struct IdArgs {
		#[serde(default)]
		worker_id: Option<String>,
		#[serde(default)]
		worker_ids: Vec<String>,
	}
	let parsed: IdArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args(tool, err.to_string()))?;
	let mut ids: Vec<String> = Vec::new();
	for id in parsed.worker_id.into_iter().chain(parsed.worker_ids) {
		if !id.is_empty() && !ids.contains(&id) {
			ids.push(id);
		}
	}
	if ids.is_empty() {
		return Err(CoderError::invalid_args(
			tool,
			"provide `worker_id` or a non-empty `worker_ids`",
		));
	}
	Ok(ids)
}

/// Fold per-worker outcomes for a multi-target fleet tool: each
/// entry is the worker's result stamped with its `worker_id`, or a
/// `{worker_id, error}` row — one bad id doesn't fail the batch.
/// (Single-target calls skip this and return the bare result, the
/// historic shape.)
fn fold_worker_results(results: Vec<(String, Result<Value, CoderError>)>) -> Value {
	let rows: Vec<Value> = results
		.into_iter()
		.map(|(id, outcome)| match outcome {
			Ok(mut value) => {
				if let Some(map) = value.as_object_mut() {
					map.insert("worker_id".into(), json!(id));
				}
				value
			}
			Err(err) => json!({ "worker_id": id, "error": err.to_string() }),
		})
		.collect();
	json!({ "results": rows })
}

async fn handle_discard_worker_worktree(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	args: &Value,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct DiscardArgs {
		#[serde(default)]
		force: bool,
		#[serde(default)]
		retire: bool,
	}
	let parsed: DiscardArgs = serde_json::from_value(args.clone())
		.map_err(|err| CoderError::invalid_args("discard_worker_worktree", err.to_string()))?;
	let ids = parse_worker_ids("discard_worker_worktree", args)?;
	// `retire: true` folds the follow-up `retire_worker` into the
	// same call — discard first (retire refuses a bound worktree),
	// then drop the fleet link. A retire refusal (e.g. the turn
	// started between the two steps) reports on the result instead
	// of erroring the already-done discard.
	let discard_and_retire = |result: &mut Result<Value, CoderError>,
	                          retire_outcome: Option<Result<Value, CoderError>>| {
		let (Ok(value), Some(outcome)) = (result, retire_outcome) else {
			return;
		};
		let Some(map) = value.as_object_mut() else {
			return;
		};
		match outcome {
			Ok(_) => {
				map.insert("retired".into(), json!(true));
			}
			Err(err) => {
				map.insert("retire_error".into(), json!(err.to_string()));
			}
		}
	};
	let single = ids.len() == 1;
	let mut results: Vec<(String, Result<Value, CoderError>)> = Vec::new();
	for worker_id in ids {
		let mut result = discard_one_worker_worktree(state, sink, &worker_id, parsed.force).await;
		let retire_outcome = if parsed.retire && result.is_ok() {
			Some(retire_one_worker(state, sink, &worker_id).await)
		} else {
			None
		};
		discard_and_retire(&mut result, retire_outcome);
		results.push((worker_id, result));
	}
	if single {
		return results.remove(0).1;
	}
	Ok(fold_worker_results(results))
}

async fn discard_one_worker_worktree(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	worker_id: &str,
	force: bool,
) -> Result<Value, CoderError> {
	ensure_worker_still_attached(state, "discard_worker_worktree", worker_id).await?;
	let Some((rt, worker_folder)) = state.runtime_for_session(worker_id).await else {
		return Err(CoderError::invalid_args(
			"discard_worker_worktree",
			format!("no mounted session for worker_id `{worker_id}`"),
		));
	};
	// Never yank a checkout out from under a running turn — the
	// worker's next tool call would write into a deleted directory.
	if rt.turn.lock().await.cancel.is_some() {
		return Err(CoderError::invalid_args(
			"discard_worker_worktree",
			format!("worker `{worker_id}` has a turn in flight — wait for it to finish or `abort_worker` first"),
		));
	}
	let (worktree_root, worktree_branch) = {
		let session = rt.session.lock().await;
		(
			session.header.worktree_root.clone(),
			session.header.worktree_branch.clone(),
		)
	};
	// Idempotent (ADR 0064): a worker whose checkout is already gone
	// — merged-and-removed, auto-reconciled after an out-of-band
	// `git worktree remove` (ADR 0063), or discarded once before —
	// has nothing left to do. Erroring here left coordinators with
	// workers they could never finish cleaning up.
	let Some(worktree_path) = worktree_root else {
		return Ok(json!({
			"status": "already_gone",
			"note": "the worker has no worktree checkout anymore — nothing to discard; its session and branch are untouched",
		}));
	};
	let Some(wt_entry) = state.workspaces.folder_for_path(&worktree_path).await else {
		// The header still points at a checkout the workspace no
		// longer binds (removed out-of-band before the end-of-turn
		// reconciliation caught it). Forget any stale git metadata —
		// it would refuse a later `git worktree add` at the same
		// deterministic path — then clear the stale routing so the
		// session drives the parent tree, and report the same no-op
		// success as the no-root case. The worker's session is filed
		// under the worktree's parent project, so that folder's host
		// runs the prune.
		if let Some(parent) = state.workspaces.folder_for_path(worker_folder.as_str()).await {
			if let Err(err) = parent.host.git_worktree_forget(Utf8Path::new(&worktree_path)).await {
				tracing::warn!(error = %err, worktree = %worktree_path, "git worktree prune failed for an unbound checkout");
			}
		}
		let handle = CoderHandle { state: state.clone() };
		handle.clear_worktree_sessions(&worktree_path).await;
		return Ok(json!({
			"status": "already_gone",
			"worktree_path": worktree_path,
			"note": "the worktree folder was already unbound — cleared the worker's stale routing; its session and branch are untouched",
		}));
	};
	let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = wt_entry.folder.origin.clone() else {
		return Err(CoderError::invalid_args(
			"discard_worker_worktree",
			format!("`{worktree_path}` is not a worktree folder"),
		));
	};
	// The checkout lives on the host under `<parent>/.worktrees/<slug>`
	// (ADR 0029) even when the workspace runs in a container, so
	// `discard_checkout`'s host-side liveness test is valid under
	// either shell target; `git_worktree_remove` translates the path
	// for whichever target the parent runs on. Live / gone /
	// stale-leftover checkouts all discard (ADR 0044 / ADR 0068) —
	// leftovers need `force`, like a dirty tree.
	if let Some(parent) = state.workspaces.folder_for_path(&parent_path).await {
		let path = Utf8Path::new(&worktree_path);
		moon_core::worktree::discard_checkout(parent.host.as_ref(), path, path, force).await?;
	}
	state.workspaces.remove_folder(&worktree_path).await?;
	// Sessions that routed to the checkout drive the parent's main
	// tree from here on (and the panel drops their worktree chip).
	let handle = CoderHandle { state: state.clone() };
	handle.clear_worktree_sessions(&worktree_path).await;
	// The folder bar has no other way to learn about an unbind it
	// didn't initiate.
	sink.send(CoderEvent::WorkspaceFoldersChanged);
	Ok(json!({
		"status": "discarded",
		"worktree_path": worktree_path,
		"branch": worktree_branch,
		"note": "the branch is kept; the worker's session now runs against the parent project",
	}))
}

/// `retire_worker` — drop a fully-done worker from the coordinator's
/// fleet registry (ADR 0064). Pure bookkeeping: the session, its
/// transcript, and its branch are untouched; the registry link is
/// removed so `list_workers` stops listing it, the feeder stops
/// forwarding its events, and the control tools stop treating it as
/// this coordinator's worker. Refuses a running worker (abort first)
/// and one whose worktree is still bound (`discard_worker_worktree`
/// first) so retirement can't strand an in-flight turn or an orphan
/// folder the coordinator no longer has tools to clean up. A
/// disconnected worker may be retired — that only drops the
/// coordinator's own bookkeeping for a session the user already owns.
async fn handle_retire_worker(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	args: &Value,
) -> Result<Value, CoderError> {
	let ids = parse_worker_ids("retire_worker", args)?;
	let single = ids.len() == 1;
	let mut results: Vec<(String, Result<Value, CoderError>)> = Vec::new();
	for worker_id in ids {
		let result = retire_one_worker(state, sink, &worker_id).await;
		results.push((worker_id, result));
	}
	if single {
		return results.remove(0).1;
	}
	Ok(fold_worker_results(results))
}

async fn retire_one_worker(
	state: &Arc<CoderState>,
	sink: &FolderEventSink,
	worker_id: &str,
) -> Result<Value, CoderError> {
	let orchestrator_id = sink.session_id().to_string();
	let owning = state
		.coordinator_workers
		.read()
		.await
		.owning_orchestrator_of(worker_id)
		.map(str::to_string);
	if owning.as_deref() != Some(orchestrator_id.as_str()) {
		return Err(CoderError::invalid_args(
			"retire_worker",
			format!("`{worker_id}` is not a worker of this coordinator"),
		));
	}
	if let Some((rt, _)) = state.runtime_for_session(worker_id).await {
		if rt.turn.lock().await.cancel.is_some() {
			return Err(CoderError::invalid_args(
				"retire_worker",
				format!("worker `{worker_id}` has a turn in flight — wait for it to finish or `abort_worker` first"),
			));
		}
		let worktree_root = rt.session.lock().await.header.worktree_root.clone();
		if let Some(root) = worktree_root {
			if state.workspaces.folder_for_path(&root).await.is_some() {
				return Err(CoderError::invalid_args(
					"retire_worker",
					format!(
						"worker `{worker_id}` still has a bound worktree at `{root}` — land its work and `discard_worker_worktree` first"
					),
				));
			}
		}
	}
	state
		.coordinator_workers
		.write()
		.await
		.remove(&orchestrator_id, worker_id);
	// Persist (ADR 0065): without this a restart-time fleet rebuild
	// would re-register the retired worker.
	// Kill the worker's MCP server instances (its headless
	// browser, per-session since the tab-fight fix) — a retired
	// worker's chromium shouldn't idle until process restart.
	state.tools.mcp().drop_session_connections(worker_id).await;
	persist_worker_detached(state, &orchestrator_id, worker_id, Some(sink.folder.as_str())).await;
	Ok(json!({
		"status": "retired",
		"worker_id": worker_id,
		"note": "the worker left your fleet; its session, transcript, and branch are untouched",
	}))
}

/// Unbind worktree folders whose checkout vanished from disk
/// (ADR 0063). `git worktree remove` run outside the discard flows —
/// a coordinator reaching for `bash` despite the prompt, the user in
/// a terminal — deletes the checkout without telling the workspace
/// registry, leaving a dead row in the project bar. This reconciles:
/// one stat per bound worktree folder, and for each missing checkout
/// it forgets the stale git metadata (best-effort, same rationale as
/// the idempotent discard path in ADR 0044 — stale metadata refuses
/// a later `git worktree add` at the same deterministic path),
/// unbinds the folder, and clears the worktree routing on sessions
/// that pointed there. Returns the pruned paths; the caller
/// announces `WorkspaceFoldersChanged` when non-empty.
async fn prune_missing_worktrees(state: &Arc<CoderState>) -> Vec<String> {
	let mut pruned = Vec::new();
	for entry in state.workspaces.folders().await {
		let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = &entry.folder.origin else {
			continue;
		};
		let worktree_path = entry.folder.path.clone();
		// Host-side stat is valid under either shell target:
		// worktrees live under `<parent>/.worktrees/<slug>` and ride
		// the parent's bind mount (ADR 0029).
		if Utf8Path::new(&worktree_path).is_dir() {
			continue;
		}
		if let Some(parent) = state.workspaces.folder_for_path(parent_path).await {
			if let Err(err) = parent.host.git_worktree_forget(Utf8Path::new(&worktree_path)).await {
				tracing::warn!(error = %err, worktree = %worktree_path, "git worktree prune failed for a vanished checkout");
			}
		}
		if let Err(err) = state.workspaces.remove_folder(&worktree_path).await {
			tracing::warn!(error = %err, worktree = %worktree_path, "failed to unbind a vanished worktree folder");
			continue;
		}
		let handle = CoderHandle { state: state.clone() };
		handle.clear_worktree_sessions(&worktree_path).await;
		pruned.push(worktree_path);
	}
	pruned
}

/// AI-suggest a commit message from a diff patch, using the same
/// cheap-model flow as `CoderHandle::suggest_commit_message` but
/// accessible from a `&Arc<CoderState>`.
async fn suggest_commit_message_from_state(state: &Arc<CoderState>, diff_patch: &str) -> Result<String, CoderError> {
	let prompt = build_commit_message_prompt("", diff_patch);
	let messages = vec![
		ChatMessage::System {
			content: COMMIT_MESSAGE_SYSTEM_PROMPT.to_string(),
		},
		ChatMessage::user(prompt),
	];
	let cheap_model = state.models.read().await.cheap().to_owned();
	let cancel = CancellationToken::new();
	let response = state
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

/// Validate that a git clone URL is safe for the coordinator to
/// use. Rejects `file://` URLs and bare local paths (which would
/// let the agent clone arbitrary host directories into the
/// workspace), and requires a recognized remote scheme (`https://`,
/// `http://`, `ssh://`, or `git@` SSH shorthand). This is a
/// security boundary, not a UX hint — the coordinator must not be
/// able to exfiltrate local filesystem contents via `git clone`.
fn is_safe_clone_url(url: &str) -> bool {
	let trimmed = url.trim();
	if trimmed.is_empty() {
		return false;
	}
	// Reject `file://` and bare local paths outright — cloning a
	// local directory would let the agent read arbitrary host paths
	// via the resulting workspace folder.
	if trimmed.starts_with("file://") || trimmed.starts_with('/') || trimmed.starts_with('.') {
		return false;
	}
	// Allow `https://`, `http://`, `ssh://`, and `git@host:path`
	// (SSH shorthand). These are the only schemes that fetch from a
	// remote, not the local filesystem.
	trimmed.starts_with("https://")
		|| trimmed.starts_with("http://")
		|| trimmed.starts_with("ssh://")
		|| trimmed.starts_with("git@")
}

/// Validate that a host path is safe for the coordinator to clone
/// into or init at. Must be absolute, must not contain `..`
/// components (no traversal), and must not be a system-critical
/// path. This runs *before* the git command — `add_folder`'s
/// existence check happens after the clone/init has already run.
fn is_safe_host_path(path: &Utf8Path) -> bool {
	if !path.is_absolute() {
		return false;
	}
	// Reject any `..` component — prevents traversal outside the
	// intended directory tree.
	for component in path.components() {
		if matches!(component, camino::Utf8Component::ParentDir) {
			return false;
		}
	}
	// Reject system-critical roots. The coordinator should never
	// clone into `/`, `/etc`, `/usr`, `/bin`, `/var`, `/sys`,
	// `/proc`, `/dev`, or `/boot`.
	let depth = path.components().count();
	if depth <= 2 {
		// At or just below the filesystem root — too broad.
		return false;
	}
	true
}

/// `clone_repo` — clone a git repo to a host path and register it
/// as a workspace folder (ADR 0030).
async fn handle_clone_repo(state: &Arc<CoderState>, sink: &FolderEventSink, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct CloneArgs {
		url: String,
		#[serde(default)]
		path: Option<String>,
	}
	let parsed: CloneArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("clone_repo", err.to_string()))?;
	if !is_safe_clone_url(&parsed.url) {
		return Err(CoderError::invalid_args(
			"clone_repo",
			"URL must be https://, http://, ssh://, or git@ (file:// and local paths are rejected)",
		));
	}
	// The coordinator's own folder anchors both the sibling-dest
	// derivation and the host that runs the git command — never the
	// live active folder (the user may have switched projects
	// mid-turn).
	let folder_path = Utf8PathBuf::from(sink.folder());
	// Resolve the destination path. If the caller provided one, use
	// it. Otherwise, clone into a sibling of the coordinator's
	// folder using the repo's basename.
	let dest = match &parsed.path {
		Some(p) => {
			let path = Utf8PathBuf::from(p);
			if !is_safe_host_path(&path) {
				return Err(CoderError::invalid_args(
					"clone_repo",
					"path must be absolute, must not contain `..`, and must not be a system-critical path",
				));
			}
			path
		}
		None => {
			// Derive a safe basename from the URL. Strip `.git`,
			// reject empty / traversal / shell metacharacters.
			let basename = parsed
				.url
				.rsplit('/')
				.next()
				.and_then(|s| s.strip_suffix(".git").unwrap_or(s).strip_suffix('/').or(Some(s)))
				.filter(|s| !s.is_empty() && !s.contains("..") && !s.contains('/'))
				.unwrap_or("repo");
			sibling_dest(&folder_path, basename)
		}
	};
	// Run the clone on the host via the coordinator folder's host.
	// The scratch root (empty workspace) resolves to a synthetic
	// home-rooted host, so a scratch coordinator can bootstrap the
	// workspace's first project.
	let Some(folder) = state.folder_entry_for(folder_path.as_str()).await else {
		return Err(CoderError::invalid_args(
			"clone_repo",
			"could not resolve the session's folder host for git clone",
		));
	};
	folder.host.git_clone(&parsed.url, &dest).await?;
	let entry = state
		.workspaces
		.add_folder(dest)
		.await
		.map_err(|err| CoderError::Internal(format!("add_folder failed: {err}")))?;
	// The folder bar has no other way to learn about a bind it didn't
	// initiate (ADR 0044).
	sink.send(CoderEvent::WorkspaceFoldersChanged);
	let mut result = json!({
		"path": entry.folder.path,
		"name": entry.folder.name,
	});
	attach_container_mount_note(state, &entry.folder.path, &mut result).await;
	Ok(result)
}

/// Validate a directory name for `init_repo`: a single path
/// component, no traversal, no hidden/flag-like prefixes, and a
/// filesystem-safe charset. The name lands as a sibling directory of
/// the coordinator's project — never an arbitrary path (the model,
/// given a free path, picks `/tmp`; new projects belong next to the
/// projects the user already works in).
fn is_valid_repo_name(name: &str) -> bool {
	!name.is_empty()
		&& name.len() <= 100
		&& !name.starts_with(['.', '-'])
		&& name
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The sibling destination for a new repo next to the coordinator's
/// project folder: `<parent-of-coordinator-folder>/<name>`. Shared by
/// `init_repo` (always) and `clone_repo` (when no explicit path is
/// given).
fn sibling_dest(coordinator_folder: &Utf8Path, name: &str) -> Utf8PathBuf {
	coordinator_folder
		.parent()
		.map(Utf8Path::to_path_buf)
		.unwrap_or_else(|| coordinator_folder.to_path_buf())
		.join(name)
}

/// `add_folder` — bind an existing sibling directory as a workspace
/// folder (the already-on-disk counterpart of `clone_repo` /
/// `init_repo`). Sibling-only by construction: the model supplies a
/// single directory name, and the destination is derived from the
/// coordinator's own folder — never an arbitrary host path.
async fn handle_add_folder(state: &Arc<CoderState>, sink: &FolderEventSink, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct AddArgs {
		name: String,
	}
	let parsed: AddArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("add_folder", err.to_string()))?;
	let name = parsed.name.trim();
	if !is_valid_repo_name(name) {
		return Err(CoderError::invalid_args(
			"add_folder",
			"name must be a single directory name (letters, digits, `-`, `_`, `.`; not starting with `.` or `-`)",
		));
	}
	let folder_path = Utf8PathBuf::from(sink.folder());
	let dest = sibling_dest(&folder_path, name);
	// Host-side stat is valid regardless of shell target: bound
	// folders live on the host filesystem (ADR 0029).
	if !dest.is_dir() {
		return Err(CoderError::invalid_args(
			"add_folder",
			format!("`{dest}` is not an existing directory — `clone_repo` or `init_repo` create new ones"),
		));
	}
	// Idempotent: an already-bound folder just reports itself.
	if let Some(existing) = state.workspaces.folder_for_path(dest.as_str()).await {
		return Ok(json!({
			"status": "already_bound",
			"path": existing.folder.path,
			"name": existing.folder.name,
		}));
	}
	let entry = state
		.workspaces
		.add_folder(dest)
		.await
		.map_err(|err| CoderError::Internal(format!("add_folder failed: {err}")))?;
	// The folder bar has no other way to learn about a bind it didn't
	// initiate (ADR 0044); headless persistence rides the same event.
	sink.send(CoderEvent::WorkspaceFoldersChanged);
	let mut result = json!({
		"status": "bound",
		"path": entry.folder.path,
		"name": entry.folder.name,
	});
	attach_container_mount_note(state, &entry.folder.path, &mut result).await;
	Ok(result)
}

/// `init_repo` — initialize a new git repo as a sibling of the
/// coordinator's project folder and register it as a workspace
/// folder (ADR 0030 / ADR 0037).
async fn handle_init_repo(state: &Arc<CoderState>, sink: &FolderEventSink, args: &Value) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct InitArgs {
		name: String,
	}
	let parsed: InitArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("init_repo", err.to_string()))?;
	let name = parsed.name.trim();
	if !is_valid_repo_name(name) {
		return Err(CoderError::invalid_args(
			"init_repo",
			"name must be a single directory name (letters, digits, `-`, `_`, `.`; not starting with `.` or `-`)",
		));
	}
	// The coordinator's own folder anchors the sibling destination
	// and supplies the host that runs the git command — never the
	// live active folder (the user may have switched projects
	// mid-turn).
	let folder_path = Utf8PathBuf::from(sink.folder());
	let dest = sibling_dest(&folder_path, name);
	if dest.exists() {
		return Err(CoderError::invalid_args(
			"init_repo",
			format!("`{dest}` already exists — pick a different name"),
		));
	}
	let Some(folder) = state.folder_entry_for(sink.folder()).await else {
		return Err(CoderError::invalid_args(
			"init_repo",
			"could not resolve the session's folder host for git init",
		));
	};
	folder.host.git_init(&dest).await?;
	let entry = state
		.workspaces
		.add_folder(dest)
		.await
		.map_err(|err| CoderError::Internal(format!("add_folder failed: {err}")))?;
	// The folder bar has no other way to learn about a bind it didn't
	// initiate (ADR 0044).
	sink.send(CoderEvent::WorkspaceFoldersChanged);
	let mut result = json!({
		"path": entry.folder.path,
		"name": entry.folder.name,
	});
	attach_container_mount_note(state, &entry.folder.path, &mut result).await;
	Ok(result)
}

/// When the workspace shell container is running but doesn't mount
/// `folder_root` (the folder was bound after the container came up),
/// attach a `note` to a tool result so the coordinator knows sessions
/// there run with the **host** toolchain until the user re-syncs or
/// restarts the container. Silent when there's no running container
/// or the folder is mounted.
async fn attach_container_mount_note(state: &Arc<CoderState>, folder_root: &str, result: &mut Value) {
	let applied = crate::tools::running_container_applied_folders(&state.workspaces, &state.workspaces_dir).await;
	let Some(applied) = applied else {
		return;
	};
	if applied.iter().any(|p| p.as_str() == folder_root) {
		return;
	}
	if let Some(obj) = result.as_object_mut() {
		obj.insert(
			"note".to_string(),
			json!(
				"the workspace shell container is running but does not mount this folder; \
				 bash / builds for sessions here run on the host toolchain until the user \
				 restarts the workspace container"
			),
		);
	}
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
				images: Vec::new(),
			});
		}
	}
	for (id, name) in &orphans {
		persist_tool_record(rt, id, name, sessions::INTERRUPTED_TOOL_RESULT_JSON, None, &[]).await;
		sink.send(CoderEvent::ToolResult {
			id: id.clone(),
			result: serde_json::json!({ "error": "Interrupted before tool completed." }),
			is_error: true,
			duration_ms: None,
		});
	}
}

/// Emit the `ToolResult` event + persist the tool record for a
/// finished call, and build the `ChatMessage::Tool` that belongs on
/// the session's conversation history. Does **not** push that
/// message — the caller decides *when* and *in what order* it lands
/// on `messages`:
///
/// - The sequential path pushes immediately via [`finish_tool_call`].
/// - The parallel sub-agent batch defers the push so it can
///   reassemble results in the model's original call order, even
///   though the events themselves fire per-completion.
///
/// Returns `Err(CoderError::Aborted)` (without emitting anything)
/// when the tool itself was aborted, matching the turn-loop's
/// short-circuit contract.
async fn emit_tool_result(
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	tool_call_id: &str,
	tool_name: &str,
	outcome: Result<Value, CoderError>,
	duration_ms: Option<u64>,
) -> Result<ChatMessage, CoderError> {
	match outcome {
		Ok(value) => {
			// Images ride typed on `ChatMessage::Tool.images`
			// (and pi `image` content blocks on disk), not as
			// base64 inside the JSON text the model reads — so
			// strip the tool's `images` key from the text
			// projection. The panel event keeps the full value;
			// the UI renders what it wants.
			let (images, text_value) = split_tool_images(value.clone());
			let content = text_value.to_string();
			sink.send(CoderEvent::ToolResult {
				id: tool_call_id.to_string(),
				result: value,
				is_error: false,
				duration_ms,
			});
			persist_tool_record(rt, tool_call_id, tool_name, &content, duration_ms, &images).await;
			Ok(ChatMessage::Tool {
				tool_call_id: tool_call_id.to_string(),
				content,
				images,
			})
		}
		Err(CoderError::Aborted) => Err(CoderError::Aborted),
		Err(err) => {
			let payload = json!({ "error": err.to_string() });
			let content = payload.to_string();
			sink.send(CoderEvent::ToolResult {
				id: tool_call_id.to_string(),
				result: payload,
				is_error: true,
				duration_ms,
			});
			persist_tool_record(rt, tool_call_id, tool_name, &content, duration_ms, &[]).await;
			Ok(ChatMessage::Tool {
				tool_call_id: tool_call_id.to_string(),
				content,
				images: Vec::new(),
			})
		}
	}
}

/// Shared "tool finished, push result + emit events + persist"
/// epilogue used by the sequential dispatch path. Thin wrapper over
/// [`emit_tool_result`] that pushes the resulting message onto the
/// session's conversation history right away (sequential dispatch
/// already runs in call order, so no reassembly is needed).
async fn finish_tool_call(
	rt: &Arc<SessionRuntime>,
	sink: &FolderEventSink,
	tool_call_id: &str,
	tool_name: &str,
	outcome: Result<Value, CoderError>,
	duration_ms: Option<u64>,
) -> Result<(), CoderError> {
	let message = emit_tool_result(rt, sink, tool_call_id, tool_name, outcome, duration_ms).await?;
	rt.session.lock().await.messages.push(message);
	Ok(())
}

/// Split a tool result into the images it returned and the
/// text-only JSON projection the model sees. Convention: a tool
/// advertises images as an `"images": [{ data_url, mime }]` key
/// on its result object (the shape `read_file` emits for image
/// files and `mcp_call` builds from MCP image blocks). Anything
/// else — no key, wrong shape — passes through with no images.
pub(crate) fn split_tool_images(value: Value) -> (Vec<crate::inference::ImageAttachment>, Value) {
	let Value::Object(mut map) = value else {
		return (Vec::new(), value);
	};
	let Some(raw) = map.remove("images") else {
		return (Vec::new(), Value::Object(map));
	};
	let images = serde_json::from_value::<Vec<crate::inference::ImageAttachment>>(raw).unwrap_or_default();
	(images, Value::Object(map))
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
/// Rewrite the session's system prompt for the upcoming turn.
/// `routing_path` is the folder the session's tools operate
/// against (the worktree when bound, else the session's coder
/// root). `scratch` is `true` when that path is the
/// empty-workspace scratch root: the session then gets no
/// project rules, no "active folder" marker, and a prompt
/// section explaining the empty-workspace posture instead.
async fn refresh_system_prompt(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	routing_path: &Utf8Path,
	scratch: bool,
	force_host_bash: bool,
	mode: CoderMode,
) {
	let folders = state.workspaces.folders().await;
	let container_mode = if scratch {
		false
	} else {
		workspace_in_container_mode(&state.tools, force_host_bash, routing_path, &folders).await
	};
	let prompt = compose_system_prompt(
		&folders,
		if scratch { None } else { Some(routing_path.as_str()) },
		if scratch { Some(routing_path) } else { None },
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

/// Probe whether this session's `bash` would route to the workspace
/// shell container. Reuses the same `resolve_bash_target` plumbing
/// the `bash` tool dispatches against — including the per-folder
/// mount check — so the system prompt's "Bound folders" rendering
/// can't drift from how `bash` actually routes commands. `false`
/// when the session's folder isn't among the bound entries (defensive
/// — a just-unbound folder mid-turn).
async fn workspace_in_container_mode(
	tools: &ToolRegistry,
	force_host_bash: bool,
	folder_path: &Utf8Path,
	folders: &[Arc<WorkspaceFolderEntry>],
) -> bool {
	let Some(folder) = folders.iter().find(|f| f.folder.path == folder_path.as_str()) else {
		return false;
	};
	tools.bash_target_is_container(force_host_bash, folder).await
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
		// A worktree checkout is the same codebase as its parent;
		// the prompt composer falls back to the parent's summary, so
		// generating a duplicate here would be pure model spend.
		if matches!(
			entry.folder.origin,
			moon_protocol::workspace::FolderOrigin::Worktree { .. }
		) {
			continue;
		}
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
	scratch_root: Option<&Utf8Path>,
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

	if let Some(scratch) = scratch_root {
		out.push('\n');
		out.push_str("## No folders bound\n\n");
		out.push_str(&format!(
			"This workspace has no folders bound, so this session is a scratch session: relative paths and `bash` run from your home directory (`{scratch}`). Address files anywhere on the host with absolute paths — `read_file` / `list_dir` / `write_file` / `edit_file` accept them; `grep` takes an absolute root. Ask before running destructive commands outside a project directory. The user can bind a project folder at any time; new sessions there are separate from this one.\n"
		));
	}
	if folders.is_empty() {
		return out;
	}
	// When the session runs in a worktree, its parent project gets a
	// dedicated annotation below (paths there re-route into the
	// worktree, ADR 0040) instead of the generic "sibling" one.
	let active_worktree: Option<(&str, &str)> = folders
		.iter()
		.find(|f| active_path == Some(f.folder.path.as_str()))
		.and_then(|f| match &f.folder.origin {
			moon_protocol::workspace::FolderOrigin::Worktree { parent_path, branch } => {
				Some((parent_path.as_str(), branch.as_str()))
			}
			_ => None,
		});
	// Look up cached summaries up-front so the rendered section
	// never half-blocks on disk reads inside a `for` loop.
	let mut entries: Vec<(String, String, Option<String>, bool)> = Vec::with_capacity(folders.len());
	let mut any_cached = false;
	for folder in folders {
		let folder_path = folder.folder.path.clone();
		let folder_name = folder.folder.name.clone();
		let mut cached = summaries.cached(Utf8Path::new(&folder_path)).await;
		// A worktree is the same codebase as its parent — reuse the
		// parent's summary rather than showing "(summary still
		// generating)" for the checkout the agent works in.
		if cached.is_none() {
			if let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = &folder.folder.origin {
				cached = summaries.cached(Utf8Path::new(parent_path)).await;
			}
		}
		if cached.is_some() {
			any_cached = true;
		}
		// Container mode renders each folder at the path the shell
		// container actually exposes. A worktree has no mount of its
		// own — it rides the parent's at
		// `/workspace/<parent>/.worktrees/<slug>` (ADR 0029), so
		// `/workspace/<name>` would advertise a path that doesn't
		// exist and push the model toward the parent's instead.
		let display_path = if container_mode {
			match &folder.folder.origin {
				moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } => {
					moon_core::worktree::worktree_container_path(Utf8Path::new(parent_path), Utf8Path::new(&folder_path))
						.map(|p| p.to_string())
						.unwrap_or_else(|| format!("/workspace/{folder_name}"))
				}
				_ => format!("/workspace/{folder_name}"),
			}
		} else {
			folder_path.clone()
		};
		let is_active = active_path == Some(folder_path.as_str());
		entries.push((folder_path, display_path, cached.map(|s| s.description), is_active));
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
	for (path, display_path, description, is_active) in &entries {
		out.push_str("- `");
		out.push_str(display_path);
		out.push('`');
		if *is_active {
			match active_worktree {
				Some((_, branch)) => {
					out.push_str(&format!(
						" **(active — your tools operate here; an isolated git worktree on branch `{branch}`)**"
					));
				}
				None => out.push_str(" **(active — your tools operate here)**"),
			}
		} else if active_worktree.is_some_and(|(parent, _)| parent == path) {
			out.push_str(
				" — the parent checkout your worktree was created from. Do **not** work here: paths addressing this folder resolve into your active worktree, and all your work must stay on your worktree's branch",
			);
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
/// Park a coordinator-bound notice in the coordinator's steer queue
/// without ever starting a turn (ADR 0062). A running turn drains it
/// at its next iteration top ([`drain_pending_steers`]) — same
/// delivery as before; an idle coordinator keeps it queued, visible
/// as a queued row with the usual "go now" / unqueue affordances,
/// until its next wake (a worker's dispatch packet, a direct user
/// message). Deliberately does **not** skip a parked `ask_user`
/// prompt the way a real steer does: a notice is information, not an
/// answer, and must not blow away a question the coordinator is
/// waiting on.
async fn park_coordinator_notice(rt: &Arc<SessionRuntime>, sink: &FolderEventSink, notice: String) {
	let steer_id = new_message_id();
	let queued_at_ms = current_time_ms();
	{
		let mut session = rt.session.lock().await;
		session.pending_steers.push(PendingSteer {
			id: steer_id.clone(),
			text: notice.clone(),
			images: Vec::new(),
			queued_at_ms,
			from_coordinator: false,
		});
		session.header.updated_at_ms = queued_at_ms;
	}
	sink.send(CoderEvent::UserMessage {
		id: steer_id,
		text: notice,
		images: Vec::new(),
		queued: true,
		created_at_ms: Some(queued_at_ms),
		from_coordinator: false,
	});
}

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
	// Tell the panel the queued rows just graduated. The contract
	// is remove-then-append, not flip-in-place: `SteerDrained`
	// drops the provisional queued bubble (which was parked at
	// send position, above the in-flight answer), and a fresh
	// `UserMessage { queued: false }` re-inserts the message at
	// the **bottom** of the transcript — matching where it lands
	// in `messages` and on disk (after the answer that was already
	// streaming when the user typed it). A new id + drain-time
	// timestamp make the appended row indistinguishable from a
	// normally-sent message and avoid colliding with the removed
	// placeholder's id. Emitted before persistence so the UI
	// update is immediate regardless of disk latency.
	for steer in &steers {
		sink.send(CoderEvent::SteerDrained { id: steer.id.clone() });
		sink.send(CoderEvent::UserMessage {
			id: new_message_id(),
			text: steer.text.clone(),
			images: steer.images.clone(),
			queued: false,
			created_at_ms: Some(current_time_ms()),
			from_coordinator: steer.from_coordinator,
		});
	}
	let Some(dir) = dir else {
		return;
	};
	for steer in steers {
		let record = SessionRecord::User {
			text: steer.text,
			images: steer.images,
			from_coordinator: steer.from_coordinator,
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

async fn persist_tool_record(
	rt: &Arc<SessionRuntime>,
	tool_call_id: &str,
	tool_name: &str,
	content: &str,
	duration_ms: Option<u64>,
	images: &[crate::inference::ImageAttachment],
) {
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
		duration_ms,
		images: images.to_vec(),
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
		// Fleet bookkeeping (ADR 0065) — nothing to render.
		SessionRecord::WorkerDetached { .. } => {}
		SessionRecord::User {
			text,
			images,
			from_coordinator,
		} => {
			out.push(CoderEvent::UserMessage {
				id: new_message_id(),
				text,
				images,
				queued: false,
				created_at_ms: Some(created_at_ms),
				from_coordinator,
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
					// The assistant record is written immediately
					// before the batch dispatches, so its line
					// timestamp is the batch's start. Exact for a
					// parallel batch (and for the single-call case
					// that covers a long-running `task`); for a
					// sequentially-dispatched batch the later calls
					// over-report until their live `ToolCall`
					// re-baselines them.
					started_at_ms: Some(created_at_ms),
				});
			}
		}
		SessionRecord::Tool {
			tool_call_id,
			tool_name: _,
			content,
			duration_ms,
			images,
		} => {
			// `content` may not be valid JSON (the model wrote
			// raw bytes for a tool output we serialised as a
			// fallback). In that case, surface the raw string —
			// the panel renders it inside a `<pre>` either way.
			let mut result = match serde_json::from_str::<Value>(&content) {
				Ok(value) => value,
				Err(_) => Value::String(content),
			};
			// Re-attach persisted images to the replayed payload
			// so the panel can render them (live results carry
			// them under the same key).
			if !images.is_empty() {
				if let Value::Object(map) = &mut result {
					map.insert("images".into(), serde_json::to_value(images).unwrap_or_default());
				}
			}
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
				duration_ms,
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
		SessionRecord::TurnDiff { files, diff } => {
			// Re-emit the per-turn diff so the reopened transcript
			// shows the collapsible diff row where it originally
			// appeared. A metadata record — doesn't shape `messages`.
			out.push(CoderEvent::TurnDiff { files, diff });
		}
	}
}

/// Replay one persisted [`SessionRecord::SubagentSpawned`] record:
/// emit the `SubagentSpawned` event so the parent's panel rebuilds
/// the collapsed card, then read the sub-agent's own JSONL (if it
/// exists) and re-emit each of its records as `SubagentEvent`s so
/// the popped-out transcript matches what the user originally saw.
///
/// `sub_dir` is the parent's sub-agent directory
/// (`<parent_sessions_dir>/<parent_session_id>/`, via
/// [`subagent_session_dir`]) — we just probe
/// `<sub_dir>/<subagent_id>.jsonl` and skip gracefully if it's
/// missing (manual deletion, partial write, older session that
/// pre-dated subagent persistence).
#[allow(clippy::too_many_arguments)]
async fn replay_subagent_spawned(
	out: &mut Vec<CoderEvent>,
	sub_dir: &Utf8Path,
	tool_call_id: String,
	subagent_id: String,
	target_folder: String,
	mode: String,
	worktree_root: Option<String>,
	worker: bool,
	detached: bool,
	still_running: bool,
) {
	out.push(CoderEvent::SubagentSpawned {
		tool_call_id,
		subagent_id: subagent_id.clone(),
		target_folder,
		mode,
		worktree_root,
		worker,
		detached,
	});

	let loaded = match sessions::load(sub_dir, &subagent_id).await {
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
	// Not for a still-running sub-agent (`still_running`): its
	// in-flight tools are orphans on disk but not interrupted —
	// the live sub-agent's own events flip the rows as they land.
	if still_running {
		return;
	}
	for orphan_id in orphan_tool_call_ids {
		out.push(CoderEvent::SubagentEvent {
			subagent_id: subagent_id.clone(),
			inner: Box::new(CoderEvent::ToolResult {
				id: orphan_id,
				result: serde_json::json!({ "error": "Interrupted before tool completed." }),
				is_error: true,
				duration_ms: None,
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
		SessionRecord::WorkerDetached { .. } => Vec::new(),
		SessionRecord::User {
			text,
			images,
			from_coordinator,
		} => vec![CoderEvent::UserMessage {
			id: new_message_id(),
			text,
			images,
			queued: false,
			created_at_ms: Some(created_at_ms),
			from_coordinator,
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
					started_at_ms: Some(created_at_ms),
				});
			}
			out
		}
		SessionRecord::Tool {
			tool_call_id,
			tool_name: _,
			content,
			duration_ms,
			images,
		} => {
			let mut result = match serde_json::from_str::<Value>(&content) {
				Ok(value) => value,
				Err(_) => Value::String(content),
			};
			if !images.is_empty() {
				if let Value::Object(map) = &mut result {
					map.insert("images".into(), serde_json::to_value(images).unwrap_or_default());
				}
			}
			let is_error = matches!(&result, Value::Object(map) if map.contains_key("error") && map.len() == 1);
			vec![CoderEvent::ToolResult {
				id: tool_call_id,
				result,
				is_error,
				duration_ms,
			}]
		}
		SessionRecord::Error { message } => vec![CoderEvent::Error { message }],
		SessionRecord::TitleUpdate { .. }
		| SessionRecord::Usage { .. }
		| SessionRecord::TodosUpdate { .. }
		| SessionRecord::Compaction { .. }
		| SessionRecord::SubagentSpawned { .. }
		| SessionRecord::SubagentFinished { .. }
		| SessionRecord::TurnDiff { .. } => Vec::new(),
	}
}

fn response_to_message(response: &AssistantResponse) -> ChatMessage {
	ChatMessage::Assistant {
		content: response.content.clone(),
		thinking_blocks: response.thinking_blocks.clone(),
		tool_calls: response.tool_calls.clone(),
	}
}

/// Remap tool-call ids on a freshly streamed response that collide
/// with an id already used earlier in the conversation. Some
/// OpenAI-compat providers mint per-message ids (`bash:0`,
/// `bash:1`, …, resetting every message; seen live from Kimi-K3
/// via Baseten), but the whole pipeline — the panel reducer's
/// upsert-by-id contract, orphan recovery, `rerun_tool_call`,
/// `ask_user` prompt routing — treats the id as session-wide
/// identity. A recycled id makes the new call invisible in the
/// transcript (the upsert matches the old, already-finished row
/// and drops the event) and its result silently overwrites the
/// old row.
///
/// Must run before the response is observed anywhere: the events,
/// the `messages` push, the persisted record, and the dispatch all
/// have to agree on the remapped id. The pairing the provider sees
/// on the next round-trip stays consistent because the assistant
/// `tool_calls` entry and the tool result reference the same
/// remapped id. No-op for providers with globally unique ids
/// (Anthropic's `toolu_…`, OpenAI's `call_…`).
pub(crate) fn dedupe_response_tool_call_ids(messages: &[ChatMessage], tool_calls: &mut [crate::inference::ToolCall]) {
	if tool_calls.is_empty() {
		return;
	}
	let mut used: std::collections::HashSet<String> = messages
		.iter()
		.filter_map(|m| match m {
			ChatMessage::Assistant { tool_calls, .. } => Some(tool_calls.iter().map(|c| c.id.clone())),
			_ => None,
		})
		.flatten()
		.collect();
	for call in tool_calls.iter_mut() {
		let unique = sessions::unique_tool_call_id(&call.id, &used);
		if unique != call.id {
			tracing::debug!(
				original = %call.id,
				remapped = %unique,
				tool = %call.function.name,
				"provider recycled a tool-call id; remapping to keep ids session-unique"
			);
			call.id = unique.clone();
		}
		used.insert(unique);
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
	cache_stats: SessionCacheStats,
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
		session_cache_hits: cache_stats.hits,
		session_requests: cache_stats.requests,
		model: crate::inference::effective_model(model_slug),
	});
}

/// Throttled mid-stream token-usage emission. Counts up
/// `delta_len` bytes into `state`'s byte counter, then emits a
/// fresh [`CoderEvent::TokenUsage`] (Estimate-tagged) only when
/// at least `throttle` has elapsed since the previous emission.
/// Cheap enough to call on every content / thinking delta — the
/// throttle keeps the event rate to ~2 Hz no matter how fast
/// the provider streams.
#[allow(clippy::too_many_arguments)]
fn maybe_emit_stream_usage(
	sink: &FolderEventSink,
	state: &std::sync::Mutex<(u32, std::time::Instant)>,
	throttle: std::time::Duration,
	delta_len: usize,
	prompt_estimate: u32,
	context_window: u32,
	cache_stats: SessionCacheStats,
	model_slug: &str,
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
		session_cache_hits: cache_stats.hits,
		session_requests: cache_stats.requests,
		model: crate::inference::effective_model(model_slug),
	});
}

/// Flat per-image token cost for the estimate. Vision input is
/// billed per tile, not per byte: the 1440x900 screenshots in a
/// measured session cost ~1,730 tokens each whether their base64 ran
/// 90 kB or 500 kB. The old bytes/4 rule counted the data URL and so
/// overstated one screenshot by more than 10x — enough to make the
/// context ring jump ~24k tokens the instant an image landed, which
/// is what sent us looking for a context bug that wasn't there
/// (ADR 0049).
const IMAGE_TOKENS: u32 = 1_700;

/// Rough token estimate for a chat history — covers system / user /
/// assistant / tool. Text goes at bytes/4, which undercounts what a
/// real tokenizer sees on code and JSON; images go at a flat
/// [`IMAGE_TOKENS`] each. Only used until the provider's own usage
/// numbers land (and to seed the compaction guard right after a
/// compaction, where there is no fresh usage yet).
pub(crate) fn estimate_prompt_tokens(messages: &[ChatMessage]) -> u32 {
	let text = (message_bytes(messages) / 4) as u32;
	text.saturating_add(IMAGE_TOKENS.saturating_mul(image_count(messages)))
}

fn image_count(messages: &[ChatMessage]) -> u32 {
	messages
		.iter()
		.map(|message| match message {
			ChatMessage::User { images, .. } | ChatMessage::Tool { images, .. } => images.len() as u32,
			_ => 0,
		})
		.sum()
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
	let tail_estimate = estimate_prompt_tokens(tail);
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
			// Image payloads are counted per attachment by
			// `estimate_prompt_tokens`, not by their base64 length.
			ChatMessage::User { content, images: _ } => bytes += content.len(),
			ChatMessage::Assistant {
				content, tool_calls, ..
			} => {
				bytes += content.as_deref().map(str::len).unwrap_or(0);
				for call in tool_calls {
					bytes += call.function.name.len();
					bytes += call.function.arguments.len();
				}
			}
			ChatMessage::Tool {
				tool_call_id,
				content,
				images: _,
			} => {
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
/// Parse a tool call's arguments, or refuse the call outright when
/// the JSON doesn't parse.
///
/// Unparseable arguments almost always mean the response was cut off
/// at the output-token ceiling *inside* the arguments blob — a big
/// `write_file` is the classic case. Dispatching anyway (the old
/// behaviour: warn, pass `{}`) turns a truncation into a schema
/// error like "missing field `path`", which tells the model nothing
/// about what actually happened, so it retries the same oversized
/// call and loops until the iteration cap.
///
/// `hit_output_cap` comes from the response's stop reason and only
/// changes the wording — the refusal itself is driven by the JSON
/// being broken, which is the precise signal. A complete call in a
/// truncated response (the cap landed after this block closed) still
/// parses and still runs.
pub(crate) fn tool_args_or_refusal(call: &FunctionCall, hit_output_cap: bool) -> Result<Value, CoderError> {
	if call.arguments.trim().is_empty() {
		return Ok(Value::Object(Default::default()));
	}
	match serde_json::from_str::<Value>(&call.arguments) {
		Ok(value) => Ok(value),
		Err(_) if hit_output_cap => {
			tracing::warn!(
				tool = %call.name,
				bytes = call.arguments.len(),
				"tool-call arguments were cut off at the output-token ceiling; refusing the call"
			);
			Err(CoderError::invalid_args(
				call.name.clone(),
				format!(
					"the arguments were cut off by the output-token limit after {} bytes and did not parse as JSON, \
so the call was NOT executed — nothing was written or run. Retry with a smaller payload: split a large `write_file` \
into a first chunk plus follow-up `edit_file` calls that append the rest, or make a targeted `edit_file` instead of \
rewriting the whole file.",
					call.arguments.len()
				),
			))
		}
		Err(err) => {
			tracing::warn!(
				tool = %call.name,
				error = %err,
				bytes = call.arguments.len(),
				"could not parse tool-call arguments as JSON; refusing the call"
			);
			Err(CoderError::invalid_args(
				call.name.clone(),
				format!("arguments were not valid JSON ({err}); the call was not executed"),
			))
		}
	}
}

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

pub(crate) fn new_message_id() -> String {
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
	fn parse_worker_ids_accepts_single_multiple_and_both() {
		let one = parse_worker_ids("t", &json!({ "worker_id": "w-1" })).unwrap();
		assert_eq!(one, vec!["w-1"]);
		let many = parse_worker_ids("t", &json!({ "worker_ids": ["w-1", "w-2", "w-1"] })).unwrap();
		assert_eq!(many, vec!["w-1", "w-2"]);
		// Both fields merge (worker_id first), deduplicated.
		let both = parse_worker_ids("t", &json!({ "worker_id": "w-2", "worker_ids": ["w-1", "w-2"] })).unwrap();
		assert_eq!(both, vec!["w-2", "w-1"]);
		// Neither, or only empties, is a loud arg error.
		assert!(parse_worker_ids("t", &json!({})).is_err());
		assert!(parse_worker_ids("t", &json!({ "worker_ids": [] })).is_err());
	}

	#[test]
	fn fold_worker_results_stamps_ids_and_keeps_errors() {
		let folded = fold_worker_results(vec![
			("w-1".into(), Ok(json!({ "status": "retired" }))),
			("w-2".into(), Err(CoderError::invalid_args("t", "nope"))),
		]);
		let rows = folded["results"].as_array().unwrap();
		assert_eq!(rows[0]["worker_id"], "w-1");
		assert_eq!(rows[0]["status"], "retired");
		assert_eq!(rows[1]["worker_id"], "w-2");
		assert!(rows[1]["error"].as_str().unwrap().contains("nope"));
	}

	#[test]
	fn tail_chars_is_boundary_safe() {
		assert_eq!(super::tail_chars("hello", 2), "lo");
		assert_eq!(super::tail_chars("hello", 99), "hello");
		// Multi-byte: never split a char.
		assert_eq!(super::tail_chars("héllo", 5), "héllo");
		assert_eq!(super::tail_chars("aé", 1), "é");
	}

	#[test]
	fn fold_worker_fleet_tracks_spawns_and_detaches() {
		let spawn = |id: &str, worker: bool, worktree: Option<&str>| SessionRecord::SubagentSpawned {
			tool_call_id: "c".into(),
			subagent_id: id.into(),
			target_folder: "/f".into(),
			mode: "agent".into(),
			worktree_root: worktree.map(str::to_string),
			worker,
			detached: false,
		};
		let records = vec![
			spawn("w-1", true, Some("/f/.worktrees/a")),
			// `task` sub-agent: never part of the fleet.
			spawn("sub-1", false, None),
			spawn("w-2", true, Some("/f/.worktrees/b")),
			// In-place worker (ADR 0070): no worktree, still fleet.
			spawn("w-3", true, None),
			SessionRecord::WorkerDetached {
				worker_id: "w-1".into(),
			},
		];
		assert_eq!(fold_worker_fleet(&records), vec!["w-2".to_string(), "w-3".to_string()]);
		// A detach with no matching spawn is a no-op.
		let only_detach = vec![SessionRecord::WorkerDetached {
			worker_id: "w-9".into(),
		}];
		assert!(fold_worker_fleet(&only_detach).is_empty());
	}

	#[test]
	fn respond_answers_accepts_map_and_sequence_forms() {
		let prompt_args = json!({
			"questions": [
				{ "id": "q1", "options": [{ "id": "yes" }, { "id": "no" }] },
				{ "id": "q2", "allow_multiple": true, "options": [{ "id": "a" }, { "id": "b" }] },
			]
		});
		// Map form: option id → selected, unknown string → free text,
		// array → multi-select with mixed classification.
		let answers = json!({ "q1": "yes", "q2": ["a", "also do the docs"] });
		let mut got = answers_to_prompt_response(&answers, Some(&prompt_args)).unwrap();
		got.sort_by(|x, y| x.question_id.cmp(&y.question_id));
		assert_eq!(got[0].selected, vec!["yes"]);
		assert!(got[0].free_text.is_empty());
		assert_eq!(got[1].selected, vec!["a"]);
		assert_eq!(got[1].free_text, "also do the docs");
		// Unknown value with no recoverable prompt args → free text.
		let got = answers_to_prompt_response(&json!({ "q1": "yes" }), None).unwrap();
		assert!(got[0].selected.is_empty());
		assert_eq!(got[0].free_text, "yes");
		// Sequence form passes through untouched.
		let seq = json!([{ "question_id": "q1", "selected": ["no"], "free_text": "" }]);
		let got = answers_to_prompt_response(&seq, Some(&prompt_args)).unwrap();
		assert_eq!(got[0].selected, vec!["no"]);
		// Non-string array member is a hard error, not silent data loss.
		assert!(answers_to_prompt_response(&json!({ "q1": [1] }), None).is_err());
	}

	#[test]
	fn split_tool_images_extracts_the_convention_key() {
		let value = json!({
			"path": "shot.png",
			"content": "[image file — image/png, attached]",
			"images": [{ "data_url": "data:image/png;base64,QUJD", "mime": "image/png" }],
		});
		let (images, text) = split_tool_images(value);
		assert_eq!(images.len(), 1);
		assert_eq!(images[0].mime, "image/png");
		// The pixels leave the text projection the model reads.
		assert!(text.get("images").is_none());
		assert_eq!(
			text.get("content").and_then(Value::as_str),
			Some("[image file — image/png, attached]")
		);
	}

	#[test]
	fn split_tool_images_passes_plain_results_through() {
		let value = json!({ "content": "plain text result" });
		let (images, text) = split_tool_images(value.clone());
		assert!(images.is_empty());
		assert_eq!(text, value);

		// A malformed `images` key degrades to no images rather
		// than failing the tool call.
		let value = json!({ "content": "x", "images": "not-an-array" });
		let (images, text) = split_tool_images(value);
		assert!(images.is_empty());
		assert!(text.get("images").is_none());
	}

	mod coordinator_registry {
		use super::super::CoordinatorRegistry;

		#[test]
		fn register_requests_feeder_only_for_first_worker() {
			let mut reg = CoordinatorRegistry::default();
			assert!(reg.register("orch-1", "w-1"));
			assert!(!reg.register("orch-1", "w-2"));
			// A different orchestrator gets its own feeder.
			assert!(reg.register("orch-2", "w-3"));
		}

		#[test]
		fn workers_of_and_attached_count_track_disconnects() {
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			reg.register("orch-1", "w-2");
			reg.register("orch-1", "w-3");
			assert_eq!(reg.attached_count("orch-1"), 3);
			assert_eq!(reg.workers_of("orch-1").len(), 3);
			// An unknown orchestrator has an empty fleet.
			assert_eq!(reg.attached_count("orch-x"), 0);
			assert!(reg.workers_of("orch-x").is_empty());

			// Disconnecting one drops it from the attached count but keeps
			// it in the inventory, marked detached.
			reg.disconnect("orch-1", "w-2");
			assert_eq!(reg.attached_count("orch-1"), 2);
			let mut workers = reg.workers_of("orch-1");
			workers.sort();
			assert_eq!(
				workers,
				vec![
					("w-1".to_string(), true),
					("w-2".to_string(), false),
					("w-3".to_string(), true),
				]
			);

			// Removing one (the feeder's final release) drops it entirely.
			reg.remove("orch-1", "w-1");
			assert_eq!(reg.attached_count("orch-1"), 1);
			assert_eq!(reg.workers_of("orch-1").len(), 2);
		}

		#[test]
		fn feeds_only_workers_of_the_right_orchestrator() {
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			assert!(reg.feeds("orch-1", "w-1"));
			assert!(!reg.feeds("orch-1", "w-other"));
			assert!(!reg.feeds("orch-2", "w-1"));
		}

		#[test]
		fn a_user_message_never_unhooks_a_worker() {
			// ADR 0043: the user messaging a worker is a notice, not
			// a handover — the feeder keeps forwarding forever.
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			for _ in 0..3 {
				assert_eq!(reg.orchestrator_of("w-1"), Some("orch-1"));
				assert!(reg.feeds("orch-1", "w-1"));
			}
		}

		#[test]
		fn orchestrator_of_ignores_sessions_that_are_not_workers() {
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			// An ordinary session (or the coordinator itself) is not
			// a worker — messaging it notifies nobody.
			assert_eq!(reg.orchestrator_of("sess-ordinary"), None);
			assert_eq!(reg.orchestrator_of("orch-1"), None);
		}

		#[test]
		fn disconnect_unhooks_feeds_notifies_and_controls() {
			// ADR 0052: an explicit disconnect — unlike a user
			// message (ADR 0043) — cuts every channel.
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			assert!(reg.disconnect("orch-1", "w-1"));
			assert!(!reg.feeds("orch-1", "w-1"));
			assert_eq!(reg.orchestrator_of("w-1"), None);
			assert!(!reg.controls("w-1"));
			// …but the membership itself is still visible so the
			// UI can offer the second-click abort, and a repeated
			// disconnect reports "already cut".
			assert!(reg.is_worker("w-1"));
			assert_eq!(reg.owning_orchestrator_of("w-1"), Some("orch-1"));
			assert!(!reg.disconnect("orch-1", "w-1"));
		}

		#[test]
		fn disconnect_ignores_sessions_that_are_not_workers() {
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			assert!(!reg.disconnect("orch-1", "sess-ordinary"));
			assert!(!reg.disconnect("orch-2", "w-1"));
			assert!(reg.controls("sess-ordinary"));
		}

		#[test]
		fn remove_after_disconnect_drops_all_memory_of_the_link() {
			// The feeder removes the link once the final wake
			// lands; afterwards a disconnect attempt finds nothing.
			let mut reg = CoordinatorRegistry::default();
			reg.register("orch-1", "w-1");
			reg.disconnect("orch-1", "w-1");
			assert!(reg.remove("orch-1", "w-1"));
			assert!(!reg.is_worker("w-1"));
			assert_eq!(reg.owning_orchestrator_of("w-1"), None);
			assert!(reg.controls("w-1"));
			assert!(!reg.remove("orch-1", "w-1"));
		}
	}

	mod detached_task_registry {
		use super::super::{DetachedFinish, DetachedTaskRegistry};
		use tokio_util::sync::CancellationToken;

		fn reg_with(parent: &str, sub: &str) -> (DetachedTaskRegistry, CancellationToken) {
			let mut reg = DetachedTaskRegistry::default();
			let cancel = CancellationToken::new();
			reg.register(parent, sub, cancel.clone());
			(reg, cancel)
		}

		#[test]
		fn is_detached_of_scopes_ids_to_their_parent() {
			let (reg, _) = reg_with("sess-a", "sub-1");
			assert!(reg.is_detached_of("sess-a", "sub-1"));
			assert!(!reg.is_detached_of("sess-b", "sub-1"));
			assert!(!reg.is_detached_of("sess-a", "sub-other"));
		}

		#[test]
		fn entry_round_trips_the_cancel_token() {
			let (reg, cancel) = reg_with("sess-a", "sub-1");
			let entry = reg.entry("sub-1").expect("entry registered");
			entry.cancel.cancel();
			assert!(cancel.is_cancelled());
		}

		#[tokio::test]
		async fn settle_caches_the_finish_and_wakes_collect() {
			let (reg, _) = reg_with("sess-a", "sub-1");
			let entry = reg.entry("sub-1").expect("entry registered");
			assert!(entry.finish.lock().await.is_none());
			// Mirror the collect path: park + enable the listener
			// *before* the settle lands, so the wake can't be lost.
			let notified = entry.notify.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			DetachedTaskRegistry::settle(&entry, DetachedFinish::Aborted).await;
			assert!(matches!(
				entry.finish.lock().await.as_ref(),
				Some(DetachedFinish::Aborted)
			));
			assert!(tokio::time::timeout(std::time::Duration::from_millis(50), notified)
				.await
				.is_ok());
		}

		#[test]
		fn live_tokens_of_returns_only_the_parents_runs() {
			let mut reg = DetachedTaskRegistry::default();
			reg.register("sess-a", "sub-1", CancellationToken::new());
			reg.register("sess-a", "sub-2", CancellationToken::new());
			reg.register("sess-b", "sub-3", CancellationToken::new());
			assert_eq!(reg.live_tokens_of("sess-a").len(), 2);
			assert_eq!(reg.live_tokens_of("sess-b").len(), 1);
			assert_eq!(reg.live_tokens_of("sess-none").len(), 0);
		}

		/// Regression: the finish feeder used to prune settled
		/// entries right after sending the wake, so the parent's
		/// later `task_collect` found nothing. The report must
		/// stay collectable until the parent session is deleted.
		#[tokio::test]
		async fn settled_runs_stay_collectable_until_the_parent_is_pruned() {
			let mut reg = DetachedTaskRegistry::default();
			let settled = reg.register("sess-a", "sub-done", CancellationToken::new());
			DetachedTaskRegistry::settle(&settled, DetachedFinish::Failed("boom".into())).await;
			assert!(reg.is_detached_of("sess-a", "sub-done"));
			assert!(reg.entry("sub-done").is_some());
			reg.prune_parent("sess-a");
			assert!(!reg.is_detached_of("sess-a", "sub-done"));
			assert!(reg.entry("sub-done").is_none());
		}

		#[test]
		fn prune_parent_cancels_live_runs_and_spares_other_parents() {
			let mut reg = DetachedTaskRegistry::default();
			let live_a = CancellationToken::new();
			let live_b = CancellationToken::new();
			reg.register("sess-a", "sub-1", live_a.clone());
			reg.register("sess-b", "sub-2", live_b.clone());
			reg.prune_parent("sess-a");
			assert!(live_a.is_cancelled());
			assert!(reg.entry("sub-1").is_none());
			assert!(!live_b.is_cancelled());
			assert!(reg.is_detached_of("sess-b", "sub-2"));
			// Idempotent on an unknown / already-pruned parent.
			reg.prune_parent("sess-a");
			reg.prune_parent("sess-none");
		}
	}

	mod user_message_notice {
		use super::super::{truncate_for_notice, USER_MESSAGE_NOTICE_MAX};

		#[test]
		fn short_messages_are_quoted_whole() {
			assert_eq!(truncate_for_notice("skip the e2e tests", 200), "skip the e2e tests");
		}

		#[test]
		fn long_messages_are_clamped_and_say_how_much_was_dropped() {
			let msg = "x".repeat(USER_MESSAGE_NOTICE_MAX + 42);
			let out = truncate_for_notice(&msg, USER_MESSAGE_NOTICE_MAX);
			assert!(out.starts_with(&"x".repeat(USER_MESSAGE_NOTICE_MAX)));
			assert!(out.ends_with("… (42 more characters)"));
		}

		#[test]
		fn the_cut_lands_on_a_char_boundary() {
			// Byte-slicing a multi-byte char would panic.
			let msg = "é".repeat(10);
			assert_eq!(truncate_for_notice(&msg, 4), "éééé… (6 more characters)");
		}
	}

	mod worker_branch_names {
		use super::super::worker_branch_slug;

		#[test]
		fn kebab_case_names_pass_through() {
			assert_eq!(
				worker_branch_slug("fix-login-redirect").as_deref(),
				Some("fix-login-redirect")
			);
		}

		#[test]
		fn prose_names_collapse_to_a_slug() {
			assert_eq!(
				worker_branch_slug("Fix the login redirect!").as_deref(),
				Some("fix-the-login-redirect")
			);
			assert_eq!(
				worker_branch_slug("  spaces  and   tabs\t").as_deref(),
				Some("spaces-and-tabs")
			);
			assert_eq!(worker_branch_slug("feat/add retry").as_deref(), Some("feat-add-retry"));
		}

		#[test]
		fn a_leading_moon_prefix_is_dropped() {
			// Otherwise the namespace doubles up: `moon/moon-fix`.
			assert_eq!(worker_branch_slug("moon/fix-login").as_deref(), Some("fix-login"));
			assert_eq!(worker_branch_slug("moon-fix-login").as_deref(), Some("fix-login"));
		}

		#[test]
		fn long_names_are_capped_without_a_trailing_separator() {
			let slug = worker_branch_slug("port the s3 client over to the new endpoints and delete the old one").unwrap();
			assert!(slug.len() <= super::super::WORKER_BRANCH_SLUG_MAX);
			assert!(!slug.ends_with('-'));
			// Cut on a word boundary, not mid-word.
			assert_eq!(slug, "port-the-s3-client-over-to-the-new");
		}

		#[test]
		fn unusable_names_are_rejected() {
			// `spawn_worker` errors instead of silently falling back
			// to an opaque `moon/agent-<id>`.
			assert_eq!(worker_branch_slug("###"), None);
			assert_eq!(worker_branch_slug(""), None);
			assert_eq!(worker_branch_slug("   "), None);
		}

		#[test]
		fn slugs_are_git_ref_safe() {
			// Every trap `git check-ref-format` cares about reduces to
			// `[a-z0-9-]` with alphanumeric ends.
			for raw in [
				"../escape",
				"a..b",
				"trailing.lock",
				"-leading",
				"trailing.",
				"with space",
			] {
				let slug = worker_branch_slug(raw).unwrap();
				assert!(
					slug
						.chars()
						.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
					"{slug}"
				);
				assert!(!slug.starts_with('-') && !slug.ends_with('-'), "{slug}");
				assert!(!slug.contains("--"), "{slug}");
			}
		}
	}

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
				images: Vec::new(),
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
	fn truncated_write_file_call_is_refused_not_dispatched_empty() {
		// A `write_file` cut off mid-`content` by the output-token
		// ceiling. The old behaviour passed `{}` through, which the
		// tool rejected as "missing field `path`" — a message that
		// sends the model straight back into the same oversized
		// call.
		let call = FunctionCall {
			name: "write_file".into(),
			arguments: r#"{"path":"big.rs","content":"fn main() {\n    let x ="#.into(),
		};
		let err = tool_args_or_refusal(&call, true).expect_err("truncated args must be refused");
		let message = err.to_string();
		assert!(message.contains("cut off"), "{message}");
		assert!(message.contains("NOT executed"), "{message}");
		// The recovery advice is the point of the message.
		assert!(message.contains("edit_file"), "{message}");
	}

	#[test]
	fn complete_call_in_a_truncated_response_still_runs() {
		// The ceiling landed after this block closed — the JSON is
		// intact, so refusing it would cost a pointless retry.
		let call = FunctionCall {
			name: "grep".into(),
			arguments: r#"{"pattern":"storage"}"#.into(),
		};
		let args = tool_args_or_refusal(&call, true).expect("valid JSON must dispatch");
		assert_eq!(args["pattern"], "storage");
	}

	#[test]
	fn empty_arguments_are_an_empty_object_not_a_refusal() {
		let call = FunctionCall {
			name: "workspace_scm_status".into(),
			arguments: String::new(),
		};
		let args = tool_args_or_refusal(&call, false).expect("no-arg tools must dispatch");
		assert_eq!(args, serde_json::json!({}));
	}

	#[test]
	fn malformed_arguments_without_a_length_stop_are_refused_too() {
		let call = FunctionCall {
			name: "read_file".into(),
			arguments: "not json at all".into(),
		};
		let err = tool_args_or_refusal(&call, false).expect_err("garbage args must be refused");
		assert!(err.to_string().contains("not valid JSON"), "{err}");
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

	#[tokio::test]
	async fn compose_system_prompt_scratch_session_explains_empty_workspace() {
		let cache = tempfile::TempDir::new().unwrap();
		let summaries = Arc::new(FolderSummaryService::new(
			Utf8PathBuf::from_path_buf(cache.path().to_path_buf()).unwrap(),
		));
		let scratch = Utf8PathBuf::from("/home/dev");
		let prompt = compose_system_prompt(&[], None, Some(scratch.as_path()), &summaries, false, CoderMode::Agent).await;
		assert!(prompt.contains("## No folders bound"));
		assert!(prompt.contains("/home/dev"));
		assert!(!prompt.contains("## Bound folders"));
		assert!(!prompt.contains("## Project rules"));
	}

	#[tokio::test]
	async fn compose_system_prompt_bound_session_has_no_scratch_section() {
		let cache = tempfile::TempDir::new().unwrap();
		let summaries = Arc::new(FolderSummaryService::new(
			Utf8PathBuf::from_path_buf(cache.path().to_path_buf()).unwrap(),
		));
		let prompt = compose_system_prompt(&[], Some("/proj"), None, &summaries, false, CoderMode::Agent).await;
		assert!(!prompt.contains("## No folders bound"));
	}

	#[tokio::test]
	async fn no_folder_root_resolves_an_existing_home() {
		let dir = tempfile::TempDir::new().unwrap();
		let canonical = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
		// Env mutation is process-global; serialise against every
		// other test that touches HOME via a mutex. Tokio's mutex so
		// the guard can be held across the awaits below without
		// tripping `clippy::await_holding_lock`.
		static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
		let _guard = HOME_LOCK.lock().await;
		let prev = std::env::var("HOME").ok();
		// SAFETY: single-threaded under the mutex; no other thread
		// reads HOME concurrently in this test binary.
		unsafe { std::env::set_var("HOME", dir.path()) };
		let root = no_folder_root().await.unwrap();
		// A synthetic entry resolves for exactly the scratch path,
		// and nothing else.
		let entry = scratch_folder_entry(canonical.as_str()).await.expect("scratch entry");
		assert_eq!(entry.folder.path, canonical.as_str());
		assert!(scratch_folder_entry("/definitely/not/the/scratch-root").await.is_none());
		match prev {
			Some(v) => unsafe { std::env::set_var("HOME", v) },
			None => unsafe { std::env::remove_var("HOME") },
		}
		assert_eq!(root, canonical);
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
				images: Vec::new(),
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
			orchestrator_session_id: None,
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
	async fn park_coordinator_notice_queues_without_starting_a_turn() {
		// ADR 0062: a user-message notice into an idle coordinator
		// parks in the steer queue — a queued `UserMessage` event,
		// a `PendingSteer` entry, and **no** turn (the function has
		// no spawn path; assert the queue + event shape and that
		// the turn slot stays empty).
		let header = header_for("sess-coord");
		let mut session = Session::new_blank();
		session.header = header;
		let rt = Arc::new(SessionRuntime::new(session));

		let (tx, mut rx) = broadcast::channel::<CoderEventEnvelope>(16);
		let sink = FolderEventSink::new(tx, "/test/folder".to_string(), "sess-coord".to_string());
		park_coordinator_notice(&rt, &sink, "the user said a thing".to_string()).await;

		let session = rt.session.lock().await;
		assert_eq!(session.pending_steers.len(), 1);
		assert_eq!(session.pending_steers[0].text, "the user said a thing");
		assert!(!session.pending_steers[0].from_coordinator);
		drop(session);
		assert!(rt.turn.lock().await.cancel.is_none());

		let envelope = rx.try_recv().expect("one queued UserMessage event");
		match envelope.event {
			CoderEvent::UserMessage {
				queued,
				text,
				from_coordinator,
				..
			} => {
				assert!(queued);
				assert_eq!(text, "the user said a thing");
				assert!(!from_coordinator);
			}
			other => panic!("expected a queued UserMessage, got {other:?}"),
		}
		assert!(
			rx.try_recv().is_err(),
			"no further events — parking must not wake anything"
		);
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
				images: Vec::new(),
			},
		];
		session.pending_steers = vec![
			PendingSteer {
				id: "steer-1".into(),
				text: "also do X".into(),
				images: Vec::new(),
				queued_at_ms: 0,
				from_coordinator: false,
			},
			PendingSteer {
				id: "steer-2".into(),
				text: "and then Y".into(),
				images: Vec::new(),
				queued_at_ms: 0,
				from_coordinator: false,
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

		// Per drained steer the panel gets a `SteerDrained` (remove
		// the placeholder) immediately followed by a fresh
		// `UserMessage { queued: false }` (re-append the real message
		// at the bottom) — in queue order. Assert the interleaved
		// sequence, not just the drained ids.
		let mut sequence = Vec::new();
		while let Ok(env) = rx.try_recv() {
			match env.event {
				CoderEvent::SteerDrained { id } => sequence.push(("drained".to_string(), id)),
				CoderEvent::UserMessage { id, text, queued, .. } => {
					assert!(!queued, "re-appended steer must not be flagged queued");
					sequence.push((format!("user:{text}"), id));
				}
				_ => {}
			}
		}
		let kinds: Vec<&str> = sequence.iter().map(|(k, _)| k.as_str()).collect();
		assert_eq!(kinds, vec!["drained", "user:also do X", "drained", "user:and then Y"]);
		// The placeholder ids are the ones removed; the re-appended
		// rows carry fresh ids distinct from them.
		assert_eq!(sequence[0].1, "steer-1");
		assert_ne!(sequence[1].1, "steer-1");
		assert_eq!(sequence[2].1, "steer-2");
		assert_ne!(sequence[3].1, "steer-2");

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
				queued_at_ms: 0,
				from_coordinator: false,
			},
			PendingSteer {
				id: "b".into(),
				text: "middle".into(),
				images: vec![ImageAttachment {
					data_url: "data:image/png;base64,xxx".into(),
					mime: "image/png".into(),
				}],
				queued_at_ms: 0,
				from_coordinator: false,
			},
			PendingSteer {
				id: "c".into(),
				text: "last".into(),
				images: Vec::new(),
				queued_at_ms: 0,
				from_coordinator: false,
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
			queued_at_ms: 0,
			from_coordinator: false,
		}];
		assert!(pop_pending_steer(&mut session, "missing").is_none());
		assert_eq!(session.pending_steers.len(), 1);
	}

	#[test]
	fn dedupe_response_tool_call_ids_remaps_recycled_ids() {
		// The Kimi-via-Baseten shape: the provider mints
		// per-message ids (`bash:0`, `bash:1`, …) that reset
		// every assistant message, so the second response's
		// `bash:0` collides with the first's.
		let messages = vec![ChatMessage::Assistant {
			content: None,
			thinking_blocks: Vec::new(),
			tool_calls: vec![
				crate::inference::ToolCall {
					id: "bash:0".into(),
					kind: "function".into(),
					function: crate::inference::FunctionCall {
						name: "bash".into(),
						arguments: "{}".into(),
					},
				},
				crate::inference::ToolCall {
					id: "bash:1".into(),
					kind: "function".into(),
					function: crate::inference::FunctionCall {
						name: "bash".into(),
						arguments: "{}".into(),
					},
				},
			],
		}];
		let mut fresh = vec![
			crate::inference::ToolCall {
				id: "bash:0".into(),
				kind: "function".into(),
				function: crate::inference::FunctionCall {
					name: "bash".into(),
					arguments: "{}".into(),
				},
			},
			crate::inference::ToolCall {
				id: "bash:1".into(),
				kind: "function".into(),
				function: crate::inference::FunctionCall {
					name: "bash".into(),
					arguments: "{}".into(),
				},
			},
		];
		dedupe_response_tool_call_ids(&messages, &mut fresh);
		assert_eq!(fresh[0].id, "bash:0-dup2");
		assert_eq!(fresh[1].id, "bash:1-dup2");
	}

	#[test]
	fn dedupe_response_tool_call_ids_leaves_unique_ids_alone() {
		let messages = vec![ChatMessage::Assistant {
			content: None,
			thinking_blocks: Vec::new(),
			tool_calls: vec![crate::inference::ToolCall {
				id: "call_1".into(),
				kind: "function".into(),
				function: crate::inference::FunctionCall {
					name: "bash".into(),
					arguments: "{}".into(),
				},
			}],
		}];
		let mut fresh = vec![crate::inference::ToolCall {
			id: "call_2".into(),
			kind: "function".into(),
			function: crate::inference::FunctionCall {
				name: "bash".into(),
				arguments: "{}".into(),
			},
		}];
		dedupe_response_tool_call_ids(&messages, &mut fresh);
		assert_eq!(fresh[0].id, "call_2");
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
				images: Vec::new(),
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
				from_coordinator: false,
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
			ChatMessage::Tool {
				tool_call_id, content, ..
			} => {
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
				duration_ms: _,
				images: _,
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
				images: Vec::new(),
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

	// ── Path sanitization for clone_repo / init_repo (ADR 0030) ──

	#[test]
	fn safe_clone_url_accepts_remote_schemes() {
		assert!(is_safe_clone_url("https://github.com/foo/bar.git"));
		assert!(is_safe_clone_url("http://example.com/repo"));
		assert!(is_safe_clone_url("ssh://git@github.com/foo/bar"));
		assert!(is_safe_clone_url("git@github.com:foo/bar.git"));
	}

	#[test]
	fn safe_clone_url_rejects_file_and_local_paths() {
		assert!(!is_safe_clone_url("file:///etc/passwd"));
		assert!(!is_safe_clone_url("/home/user/secrets"));
		assert!(!is_safe_clone_url("./relative/path"));
		assert!(!is_safe_clone_url(""));
		assert!(!is_safe_clone_url("   "));
	}

	#[test]
	fn safe_host_path_accepts_deep_absolute() {
		assert!(is_safe_host_path(Utf8Path::new("/home/user/projects/new-repo")));
		assert!(is_safe_host_path(Utf8Path::new("/Users/dev/code/scratch")));
	}

	#[test]
	fn safe_host_path_rejects_relative() {
		assert!(!is_safe_host_path(Utf8Path::new("relative/path")));
		assert!(!is_safe_host_path(Utf8Path::new("./relative")));
	}

	#[test]
	fn safe_host_path_rejects_traversal() {
		assert!(!is_safe_host_path(Utf8Path::new("/home/user/../../../etc")));
		assert!(!is_safe_host_path(Utf8Path::new("/home/../root/secrets")));
	}

	#[test]
	fn safe_host_path_rejects_system_roots() {
		assert!(!is_safe_host_path(Utf8Path::new("/")));
		assert!(!is_safe_host_path(Utf8Path::new("/etc")));
		assert!(!is_safe_host_path(Utf8Path::new("/usr")));
		assert!(!is_safe_host_path(Utf8Path::new("/var")));
	}

	#[test]
	fn repo_name_accepts_plain_directory_names() {
		assert!(is_valid_repo_name("my-service"));
		assert!(is_valid_repo_name("scratch_2"));
		assert!(is_valid_repo_name("dashboard.v2"));
	}

	#[test]
	fn repo_name_rejects_paths_traversal_and_flags() {
		assert!(!is_valid_repo_name(""));
		assert!(!is_valid_repo_name("a/b"));
		assert!(!is_valid_repo_name("/tmp/foo"));
		assert!(!is_valid_repo_name(".."));
		assert!(!is_valid_repo_name(".hidden"));
		assert!(!is_valid_repo_name("-rf"));
		assert!(!is_valid_repo_name("has space"));
	}

	#[test]
	fn sibling_dest_lands_next_to_the_coordinator_folder() {
		assert_eq!(
			sibling_dest(Utf8Path::new("/home/me/code/moon-landing"), "dashboard"),
			Utf8Path::new("/home/me/code/dashboard")
		);
	}
}
