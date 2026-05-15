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
use crate::inference::{AssistantResponse, ChatMessage, FunctionCall, InferenceClient, StreamEvent, TokenUsage};
use crate::models::{self, CoderModels, ResolvedProvider, SharedCoderModels};
use crate::providers::{self, ProviderKeyring};
use crate::sessions::{
	self, current_time_ms, new_session_id, session_title_from_prompt, sessions_dir, LoadedSession, SessionHeader,
	SessionRecord, SessionSummary, SESSION_SCHEMA_VERSION,
};
use crate::subagent::{build_subagent_spec, run_subagent, spawn_subagent_tool_definition, SubagentReport};
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
}

/// Per-folder runtime: one in-memory `Session` plus one
/// `TurnState`. Kept under separate mutexes so `abort` and `send`
/// race on the same `TurnState` lock without holding the session
/// while waiting for it (and inversely, the session can be
/// updated mid-turn without contending with abort).
struct FolderSession {
	session: Mutex<Session>,
	turn: Mutex<TurnState>,
}

impl FolderSession {
	fn new() -> Self {
		Self {
			session: Mutex::new(Session::new_blank()),
			turn: Mutex::new(TurnState::default()),
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
/// turn / sub-agent / auto-rename pass — captures the folder
/// string once so emit sites don't have to thread it through
/// every send call. Sub-agents share their parent's sink so
/// their events arrive in the parent's folder bucket on the
/// frontend (sub-agents belong to whichever project originated
/// them).
#[derive(Clone)]
pub(crate) struct FolderEventSink {
	sender: broadcast::Sender<CoderEventEnvelope>,
	folder: String,
}

impl FolderEventSink {
	pub(crate) fn new(sender: broadcast::Sender<CoderEventEnvelope>, folder: impl Into<String>) -> Self {
		Self {
			sender,
			folder: folder.into(),
		}
	}

	pub(crate) fn send(&self, event: CoderEvent) {
		let _ = self.sender.send(CoderEventEnvelope {
			folder: self.folder.clone(),
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
				subagent_target_folder: None,
			},
			session_dir: None,
			messages: vec![ChatMessage::System {
				content: PHASE_6_0_SYSTEM_PROMPT.to_string(),
			}],
			persisted_records: 0,
			auto_rename_pending: false,
			last_usage: None,
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

	/// Resolve to `(active folder's FolderSession, folder path)`
	/// or error with `NoActiveFolder`. Used by every command that
	/// the user triggers from the panel — `send`, `abort`,
	/// `list_sessions`, `new_session`, etc. Background tasks
	/// (`run_turn`, `run_subagent`, `spawn_auto_rename`) close
	/// over an `Arc<FolderSession>` from when they were spawned
	/// and never re-resolve through this helper, so a folder
	/// switch mid-turn doesn't redirect them.
	async fn active_folder_session(&self) -> Result<(Arc<FolderSession>, Utf8PathBuf), CoderError> {
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let folder_path = Utf8PathBuf::from(folder.folder.path.clone());
		let session = self.folder_session_for(&folder_path).await;
		Ok((session, folder_path))
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
			}),
		})
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
		let mut m = self.state.models.write().await;
		m.standard = standard;
		m.cheap = cheap;
		m.bill_to = bill_to;
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
	pub async fn set_providers(
		&self,
		mut providers: Vec<moon_protocol::coder_models::CoderProviderConfig>,
		active: Option<String>,
	) {
		for p in &mut providers {
			p.has_api_key = self.state.provider_keys.has_key(&p.id);
		}
		let mut m = self.state.models.write().await;
		m.providers = providers;
		m.active_provider = active;
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
		api_key: Option<&str>,
	) -> Result<moon_protocol::coder_models::ProviderProbeResult, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(CoderError::from)?;
		providers::probe_provider(&http, base_url, api_key).await
	}

	/// Current `CoderModels` snapshot. The Tauri layer reads this
	/// on `coder_status` so the panel can render the active picks
	/// without keeping a parallel cache.
	pub async fn current_models(&self) -> CoderModels {
		self.state.models.read().await.clone()
	}

	/// Catalog the picker renders.
	///
	/// - HF active: forward the rich `/v1/models` shape from the
	///   router (preserves provider/route/pricing detail).
	/// - User provider active: returns an `Err(NoActiveFolder)`-
	///   shaped error (`Internal(...)`) so the Tauri command can
	///   route the picker to `coder_list_provider_models`
	///   instead. We keep one entrypoint per shape so the
	///   frontend never has to disambiguate the union type.
	///
	/// Side effect: when HF is active, refreshes
	/// [`CoderModels::context_windows`] so subsequent turns can
	/// size the usage ring / compaction threshold against
	/// authoritative numbers instead of the static fallback
	/// table.
	pub async fn list_models(&self) -> Result<Vec<moon_protocol::coder_models::RouterModel>, CoderError> {
		let route = self.state.models.read().await.resolve_route();
		match route {
			ResolvedProvider::HuggingFace => {
				let catalog = self.state.inference.list_hf_models().await?;
				let windows = models::context_windows_from_catalog(&catalog);
				self.state.models.write().await.context_windows = Arc::new(windows);
				Ok(catalog)
			}
			ResolvedProvider::Custom { .. } => Err(CoderError::Internal(
				"active provider is not Hugging Face; call coder_list_provider_models instead".into(),
			)),
		}
	}

	/// Flat catalog for a user-added provider. `id` matches one
	/// of `CoderModels::providers[].id`; the runner looks up the
	/// `base_url` and the (optional) API key, then calls
	/// `/v1/models` against the endpoint. Errors propagate
	/// verbatim — a 404 means the server doesn't expose the
	/// catalog endpoint and the user can still type a model slug
	/// directly into the picker field.
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
		drop(snapshot);
		let api_key = self.state.provider_keys.get(provider_id);
		self
			.state
			.inference
			.list_provider_models(&base_url, api_key.as_deref())
			.await
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
			ResolvedProvider::Custom { id, base_url } => {
				if self.state.provider_keys.has_key(id) {
					true
				} else {
					is_local_base_url(base_url)
				}
			}
		};
		// `busy` reflects the **active folder's** turn only — the
		// panel mirrors per-folder UI state, so other folders'
		// running turns don't make this folder's composer disable
		// (they update their own per-folder UI state when the user
		// switches back).
		let busy = match self.state.workspaces.active_folder().await {
			Some(folder) => {
				let path = Utf8PathBuf::from(folder.folder.path.clone());
				let fs = self.state.folder_session_for(&path).await;
				let busy_now = fs.turn.lock().await.cancel.is_some();
				busy_now
			}
			None => false,
		};
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
			signed_in,
			identity,
			busy,
			bash_target,
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
			ChatMessage::User { content: prompt },
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
			ChatMessage::User { content: prompt },
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

	/// Cancel every running turn across every folder. Used by
	/// sign-out (semantic "this auth identity is no longer
	/// driving the agent") and by tests that need a clean slate.
	async fn abort_all(&self) {
		let by = self.state.sessions_by_folder.read().await;
		for fs in by.values() {
			let turn = fs.turn.lock().await;
			if let Some(token) = turn.cancel.as_ref() {
				token.cancel();
			}
		}
	}

	/// Snapshot of the **active folder's** session. `None` when
	/// the session is blank (no user message yet) or no folder is
	/// active — the panel uses this to render the empty /
	/// "send your first message" state.
	pub async fn active_session(&self) -> Option<SessionSummary> {
		let (fs, _) = self.state.active_folder_session().await.ok()?;
		let session = fs.session.lock().await;
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
		let Some(folder) = self.state.workspaces.active_folder().await else {
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

	/// Discard the active folder's in-memory session and start a
	/// blank one. Doesn't touch disk — empty sessions never get a
	/// file in the first place. Returns the new session's metadata
	/// so the panel can reference it before the first send. Other
	/// folders' sessions are untouched.
	pub async fn new_session(&self) -> Result<SessionSummary, CoderError> {
		let (fs, _) = self.state.active_folder_session().await?;
		// Abort the active folder's turn (if any) before swapping
		// out its session. Other folders' running turns keep going.
		{
			let turn = fs.turn.lock().await;
			if let Some(token) = turn.cancel.as_ref() {
				token.cancel();
			}
		}
		let mut session = fs.session.lock().await;
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
		let (fs, folder_path) = self.state.active_folder_session().await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		let LoadedSession { header, records } = sessions::load(&dir, &id).await?;

		// Abort the active folder's turn before swapping its
		// session out. Other folders' turns are untouched.
		{
			let turn = fs.turn.lock().await;
			if let Some(token) = turn.cancel.as_ref() {
				token.cancel();
			}
		}

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
			last_usage: None,
		};
		*fs.session.lock().await = session;

		// Tell the panel to clear + reload, then fan out the
		// records as the same events a live turn would emit.
		// `SessionLoaded` carries the metadata so the sticky
		// header doesn't need a follow-up IPC round trip.
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string());
		sink.send(CoderEvent::SessionLoaded {
			id: summary.id.clone(),
			title: summary.title.clone(),
			created_at_ms: summary.created_at_ms,
			updated_at_ms: summary.updated_at_ms,
		});
		for record in records {
			emit_replay_events(&sink, record);
		}
		// Clear the busy state on the frontend. Replayed `UserMessage`
		// events flip `coder.busy = true` (mirroring the live-turn
		// flow), but no `TurnComplete` is recorded in the session
		// log, so without this final nudge the panel would render
		// the "stop" button after every restore — even for a session
		// whose last turn finished cleanly hours ago. Sending an
		// explicit terminator at end-of-replay is correct in all
		// cases: if the IDE was killed mid-turn we want busy=false
		// anyway, since no real turn is running on the rehydrated
		// session.
		sink.send(CoderEvent::TurnComplete);
		Ok(summary)
	}

	/// Delete a persisted session under the active workspace
	/// folder. Idempotent. If the deleted session is the one
	/// currently mounted in memory for that folder, replace it
	/// with a blank one. Other folders' sessions are untouched.
	pub async fn delete_session(&self, id: String) -> Result<(), CoderError> {
		sessions::validate_session_id(&id)?;
		let (fs, folder_path) = self.state.active_folder_session().await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);
		sessions::delete(&dir, &id).await?;
		{
			let mut session = fs.session.lock().await;
			if session.header.id == id {
				*session = Session::new_blank();
			}
		}
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string());
		sink.send(CoderEvent::SessionListChanged);
		Ok(())
	}

	pub async fn send(&self, text: String) -> Result<(), CoderError> {
		// Bail early if the active route can't authenticate —
		// surface a clean error instead of letting the inference
		// layer fail on the first request. HF needs OAuth; user
		// providers need a configured key (or a localhost
		// `base_url`, where keyless is conventional for Ollama /
		// llama.cpp).
		let route = self.state.models.read().await.resolve_route();
		match &route {
			ResolvedProvider::HuggingFace => {
				if !self.state.auth.has_valid_session().await {
					return Err(CoderError::NotSignedIn);
				}
			}
			ResolvedProvider::Custom { id, base_url } => {
				if !self.state.provider_keys.has_key(id) && !is_local_base_url(base_url) {
					return Err(CoderError::NotSignedIn);
				}
			}
		}
		let (fs, folder_path) = self.state.active_folder_session().await?;
		let dir = sessions_dir(&self.state.coder_sessions_dir, &folder_path);

		// Reject double-sends within this **folder**. Other folders
		// can have their own turns running simultaneously — the
		// per-folder turn lock means switching projects doesn't
		// stall the agent in the one you left behind.
		{
			let turn = fs.turn.lock().await;
			if turn.cancel.is_some() {
				return Err(CoderError::Internal("a turn is already running for this folder".into()));
			}
		}

		let cancel = CancellationToken::new();
		{
			let mut turn = fs.turn.lock().await;
			turn.cancel = Some(cancel.clone());
		}

		// Bind / prep the session: first `send` allocates the
		// title and locks the sessions dir; subsequent sends just
		// append.
		let (auto_rename_after, summary_to_announce) = {
			let mut session = fs.session.lock().await;
			let needs_loaded_event = session.header.title.is_empty() && session.persisted_records == 0;
			if session.session_dir.is_none() {
				session.session_dir = Some(dir.clone());
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
		let sink = FolderEventSink::new(self.state.events.clone(), folder_path.to_string());
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
			let mut session = fs.session.lock().await;
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
				let mut session = fs.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}

		let user_id = new_message_id();
		sink.send(CoderEvent::UserMessage {
			id: user_id,
			text: text.clone(),
		});

		let state = self.state.clone();
		let fs_for_turn = fs.clone();
		let cancel_outer = cancel.clone();
		let sink_for_turn = sink.clone();
		let folder_for_turn = folder_path.clone();
		tokio::spawn(async move {
			let result = run_turn(&state, &fs_for_turn, &folder_for_turn, &sink_for_turn, cancel_outer).await;
			fs_for_turn.turn.lock().await.cancel = None;
			match &result {
				Ok(()) => sink_for_turn.send(CoderEvent::TurnComplete),
				Err(CoderError::Aborted) => sink_for_turn.send(CoderEvent::Aborted),
				Err(err) => {
					tracing::warn!(error = %err, "coder turn failed");
					sink_for_turn.send(CoderEvent::Error {
						message: err.to_string(),
					});
				}
			}
			// Auto-rename fires regardless of how the turn ended.
			// A successful turn gives the fast model the assistant's
			// final answer to summarise into a title; an Esc'd or
			// errored turn falls back to whatever bytes made it
			// into the transcript (the user prompt at minimum,
			// possibly some assistant content + tool results). The
			// real-world failure mode this fixes: long tool-heavy
			// turns the user often Esc's mid-flight — under the
			// previous "Ok(())-only" rule those sessions kept the
			// truncated-prompt fallback title forever.
			if auto_rename_after {
				spawn_auto_rename(state.clone(), fs_for_turn.clone(), sink_for_turn);
			}
		});

		Ok(())
	}

	/// Cancel the **active folder's** turn (if any). Background
	/// turns running in other folders are left alone — switching
	/// to one and hitting stop is a separate action. Just trips
	/// the cancel token; the spawned turn observes it on its
	/// next `select!` and exits.
	pub async fn abort(&self) {
		let Ok((fs, _)) = self.state.active_folder_session().await else {
			return;
		};
		let turn = fs.turn.lock().await;
		if let Some(token) = turn.cancel.as_ref() {
			token.cancel();
		}
	}

	pub fn subscribe(&self) -> broadcast::Receiver<CoderEventEnvelope> {
		self.state.events.subscribe()
	}
}

async fn run_turn(
	state: &Arc<CoderState>,
	fs: &Arc<FolderSession>,
	folder_path: &Utf8Path,
	sink: &FolderEventSink,
	cancel: CancellationToken,
) -> Result<(), CoderError> {
	// Snapshot the user's current model picks once at turn-start.
	// A settings flip mid-turn doesn't retroactively change which
	// model the in-flight requests are talking to; the *next* turn
	// (or sub-agent, or auto-rename) will see the new pick. `bill_to`
	// is read fresh per request via the shared handle inside
	// `InferenceClient` instead.
	let models = state.models.read().await.clone();
	let standard_model = models.standard().to_owned();

	// Parent's tool list = registry's regular tools plus the
	// `spawn_subagent` definition. Sub-agents pick from the
	// registry alone (no spawn_subagent), which is how the
	// depth-1 cap is enforced — a sub-agent literally cannot
	// describe a sub-sub-agent because the model never sees the
	// tool.
	let mut tool_defs = state.tools.definitions();
	tool_defs.push(spawn_subagent_tool_definition());
	// Pin the tool context to the **session's** bound folder
	// (captured at spawn time), not the live `active_folder()`.
	// This is what makes "agent keeps running in folder X while
	// user browses folder Y" actually work: the spawned `run_turn`
	// closes over its `folder_path`, so its tools always operate
	// against folder X regardless of whatever the user has
	// foregrounded in the IDE.
	let folder_entry = state
		.workspaces
		.folder_for_path(folder_path.as_str())
		.await
		.ok_or(CoderError::NoActiveFolder)?;
	let cx = ToolContext::new(folder_entry, CoderMode::Agent);
	// Compose a fresh system prompt and overwrite the session's
	// `messages[0]`: the base prompt plus a "Bound folders"
	// section keyed off whatever summaries are currently cached.
	// Sub-agent dispatch reads the same cache so the model's
	// awareness of bound folders is consistent across parent +
	// sub-agent prompts.
	refresh_system_prompt(state, fs, folder_path).await;
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

		// Token-aware compaction before each round-trip. Reads the
		// session's last-seen usage; if it crossed the threshold,
		// runs a fast-model summary and rewrites `messages` in
		// place. The on-disk JSONL transcript is left untouched —
		// only the in-memory prompt shrinks.
		let last_usage = fs.session.lock().await.last_usage;
		let mut messages = fs.session.lock().await.messages.clone();
		let did_compact = crate::compaction::compact_if_needed(
			&state.inference,
			sink,
			None,
			&models,
			last_usage.as_ref(),
			&mut messages,
			&cancel,
		)
		.await;
		if did_compact {
			let mut session = fs.session.lock().await;
			session.messages = messages.clone();
			// Reset the trigger so we don't re-compact next iteration
			// before the next response's usage lands.
			session.last_usage = None;
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
				}
				// Tool-call deltas are intentionally not surfaced.
				// The runner buffers them inside the inference
				// client and dispatches once the whole call is
				// assembled — partial JSON arguments aren't
				// useful to render.
				StreamEvent::ToolCallDelta { .. } => {}
			})
			.await?;

		{
			let mut session = fs.session.lock().await;
			session.messages.push(response_to_message(&response));
			// Stash whatever usage we have for the next iteration's
			// compaction decision. Provider-supplied is exact; we
			// synthesise a `TokenUsage` from the bytes/4 estimate
			// when missing so the threshold check still has a
			// number to compare against.
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
		persist_assistant_record(fs, &response).await;

		// Per-iteration token usage report. Drives the in-panel
		// usage ring + the auto-compaction trigger. Provider-supplied
		// numbers are exact; falls back to a bytes/4 estimate when
		// the provider didn't emit a streaming usage chunk so the
		// ring still moves on every turn.
		emit_token_usage(sink, &models, &standard_model, &messages, &response);

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
			sink.send(CoderEvent::AssistantMessageEnd {
				id: assistant_id,
				text: response.content.clone().unwrap_or_default(),
				thinking: canonical_thinking,
			});
		}

		if response.tool_calls.is_empty() {
			return Ok(());
		}

		dispatch_tool_calls(state, fs, sink, &cx, &cancel, &response.tool_calls).await?;
	}

	// Iteration cap reached. Rather than just bailing with an
	// error banner — which leaves the user staring at a wall of
	// tool calls and no actual answer — we ask the model for one
	// final, tools-disabled wrap-up turn. It sees the full history
	// it just produced, the tool budget exhausted note, and is
	// instructed to write its best answer with what it has.
	wrap_up_final_answer(state, fs, sink, &cancel, &tool_defs).await
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
	fs: &Arc<FolderSession>,
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

	let sentinel_id = new_message_id();
	let sentinel_text = format!(
		"[Tool-call budget exhausted: you've used all {MAX_TURN_ITERATIONS} tool-call iterations available for this turn. \
Do not call any more tools. Write a final response now using only what you've already gathered: summarise what was \
done, what's still unfinished, and any uncertainty. If the user needs to take a follow-up action, say so explicitly.]"
	);
	{
		let mut session = fs.session.lock().await;
		session.messages.push(ChatMessage::User {
			content: sentinel_text.clone(),
		});
	}
	{
		// Best-effort persist of the sentinel into the JSONL — same
		// shape as a real user turn so re-loading the session shows
		// it inline. Lives entirely inside the lock-then-drop dance
		// the regular user-message path uses, just inlined since
		// we don't need a separate helper for the one-off case.
		let session = fs.session.lock().await;
		let header = session.header.clone();
		let dir = session.session_dir.clone();
		drop(session);
		if let Some(dir) = dir {
			if let Err(err) = sessions::append_record(
				&dir,
				&header,
				&SessionRecord::User {
					text: sentinel_text.clone(),
				},
			)
			.await
			{
				tracing::warn!(error = %err, "failed to persist tool-cap sentinel user message");
			} else {
				let mut session = fs.session.lock().await;
				session.persisted_records = session.persisted_records.saturating_add(1);
			}
		}
	}
	sink.send(CoderEvent::UserMessage {
		id: sentinel_id,
		text: sentinel_text,
	});

	let messages = fs.session.lock().await.messages.clone();
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

	if started.into_inner() {
		let canonical_thinking = if thinking_emitted.into_inner() {
			response.thinking.clone()
		} else {
			None
		};
		sink.send(CoderEvent::AssistantMessageEnd {
			id: assistant_id,
			text: response.content.clone().unwrap_or_default(),
			thinking: canonical_thinking,
		});
	}

	fs.session.lock().await.messages.push(response_to_message(&response));
	persist_assistant_record(fs, &response).await;
	emit_token_usage(sink, &models, &standard_model, &messages, &response);

	Ok(())
}

/// Limit on concurrent sub-agents per parent batch. A
/// `Semaphore`-bound; only meaningful when the model emits a
/// homogeneous `spawn_subagent` batch larger than this. Excess
/// sub-agents queue against the semaphore. Hardcoded for now per
/// AGENTS.md "hardcode first, configure later" — bumps land when
/// a real workload outgrows it.
const SUBAGENT_PARALLELISM_CAP: usize = 4;

/// Run every `tool_call` in `calls`, emitting the `ToolCall` /
/// `ToolResult` event pair for each and pushing the result onto
/// the session's messages. Branches:
///
/// - **Homogeneous `spawn_subagent` batch (N ≥ 2)**: spawn each
///   sub-agent concurrently, bounded by [`SUBAGENT_PARALLELISM_CAP`].
///   Tool-call events fire upfront so the UI inserts every
///   collapsed card before any sub-agent finishes; results land
///   in completion order but are pushed onto `messages` in the
///   model's original tool-call order so context stays
///   deterministic across replays.
/// - **Anything else** (mixed batch, single call, or zero
///   `spawn_subagent` calls): sequential dispatch. Sub-agent
///   intercept still kicks in for individual `spawn_subagent`
///   calls in mixed batches.
async fn dispatch_tool_calls(
	state: &Arc<CoderState>,
	fs: &Arc<FolderSession>,
	sink: &FolderEventSink,
	cx: &ToolContext,
	cancel: &CancellationToken,
	calls: &[crate::inference::ToolCall],
) -> Result<(), CoderError> {
	let homogeneous_subagent = calls.len() >= 2 && calls.iter().all(|c| c.function.name == "spawn_subagent");
	if homogeneous_subagent {
		dispatch_subagent_batch(state, fs, sink, cx, cancel, calls).await
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
			let outcome = if call.function.name == "spawn_subagent" {
				handle_spawn_subagent(state, fs, sink, cx, cancel, &call.id, &args).await
			} else {
				state.tools.dispatch(&call.function.name, &args, cx, cancel).await
			};
			finish_tool_call(fs, sink, &call.id, outcome).await?;
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
	fs: &Arc<FolderSession>,
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
	for (call, args) in calls.iter().cloned().zip(parsed_args.into_iter()) {
		let state_for_task = state.clone();
		let fs_for_task = fs.clone();
		let sink_for_task = sink.clone();
		let cx_for_task = cx.clone();
		let cancel_for_task = cancel.clone();
		let sem_for_task = sem.clone();
		let call_id = call.id.clone();
		let task = tokio::spawn(async move {
			let _permit = sem_for_task.acquire().await.expect("semaphore not closed");
			handle_spawn_subagent(
				&state_for_task,
				&fs_for_task,
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
		finish_tool_call(fs, sink, &call.id, outcome).await?;
	}
	Ok(())
}

/// Build + run a `Subagent` from the JSON args. Validation
/// errors surface back to the model as the tool's `is_error: true`
/// result so a confused call ("folder X not bound", "unknown
/// mode") is a recoverable signal, not a hard turn-failure.
async fn handle_spawn_subagent(
	state: &Arc<CoderState>,
	fs: &Arc<FolderSession>,
	sink: &FolderEventSink,
	cx: &ToolContext,
	cancel: &CancellationToken,
	tool_call_id: &str,
	args: &Value,
) -> Result<Value, CoderError> {
	let parent_session_id = fs.session.lock().await.header.id.clone();
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
	let sub_cancel = cancel.child_token();
	// Sub-agents share their parent's `FolderEventSink` — events
	// arrive in the parent's folder bucket on the frontend, which
	// is exactly the multi-session contract: sub-agents belong to
	// whichever project originated them.
	let models_snapshot = state.models.read().await.clone();
	let report: SubagentReport = run_subagent(
		&state.tools,
		&state.inference,
		sink,
		&state.coder_sessions_dir,
		&models_snapshot,
		spec,
		sub_cancel,
	)
	.await?;
	Ok(json!({
		"result": report.result,
		"sub_session_id": report.sub_session_id,
		"tokens_used_estimate": report.tokens_used_estimate,
		"mode": report.mode.as_wire(),
		"iterations_used": report.iterations_used,
	}))
}

/// Shared "tool finished, push result + emit events + persist"
/// epilogue used by both the sequential and the parallel paths.
async fn finish_tool_call(
	fs: &Arc<FolderSession>,
	sink: &FolderEventSink,
	tool_call_id: &str,
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
			fs.session.lock().await.messages.push(ChatMessage::Tool {
				tool_call_id: tool_call_id.to_string(),
				content: content.clone(),
			});
			persist_tool_record(fs, tool_call_id, &content).await;
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
			fs.session.lock().await.messages.push(ChatMessage::Tool {
				tool_call_id: tool_call_id.to_string(),
				content: content.clone(),
			});
			persist_tool_record(fs, tool_call_id, &content).await;
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
async fn refresh_system_prompt(state: &Arc<CoderState>, fs: &Arc<FolderSession>, folder_path: &Utf8Path) {
	let folders = state.workspaces.folders().await;
	let prompt = compose_system_prompt(&folders, Some(folder_path.as_str()), &state.folder_summaries).await;
	let mut session = fs.session.lock().await;
	if let Some(ChatMessage::System { content }) = session.messages.first_mut() {
		*content = prompt;
	} else {
		session.messages.insert(0, ChatMessage::System { content: prompt });
	}
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
/// 1. Base text from [`PHASE_6_0_SYSTEM_PROMPT`].
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
) -> String {
	let mut out = String::with_capacity(PHASE_6_0_SYSTEM_PROMPT.len() + 1024);
	out.push_str(PHASE_6_0_SYSTEM_PROMPT);
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
	out.push_str(
		"All folders currently bound to this workspace, listed with the synthetic `/workspace/<name>` paths your tools recognise. Your tools resolve paths against the **active** folder only; for any other folder, use `spawn_subagent` with `folder: \"<name>\"`.\n\n",
	);
	for (name, _path, description, is_active) in &entries {
		out.push_str("- `/workspace/");
		out.push_str(name);
		out.push('`');
		if *is_active {
			out.push_str(" **(active — your tools operate here)**");
		} else {
			out.push_str(" — sibling, reach via `spawn_subagent`");
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

/// Append an `Assistant` record to the JSONL of the given
/// folder's session. Best-effort: a write failure logs but
/// doesn't fail the turn.
async fn persist_assistant_record(fs: &Arc<FolderSession>, response: &AssistantResponse) {
	let (dir, header) = {
		let session = fs.session.lock().await;
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
	let mut session = fs.session.lock().await;
	session.persisted_records = session.persisted_records.saturating_add(1);
}

async fn persist_tool_record(fs: &Arc<FolderSession>, tool_call_id: &str, content: &str) {
	let (dir, header) = {
		let session = fs.session.lock().await;
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
	let mut session = fs.session.lock().await;
	session.persisted_records = session.persisted_records.saturating_add(1);
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
fn spawn_auto_rename(state: Arc<CoderState>, fs: Arc<FolderSession>, sink: FolderEventSink) {
	tokio::spawn(async move {
		// Snapshot the chat history without holding the session
		// lock across the LLM call — turns / aborts must be able
		// to grab it freely while we wait on the network. The
		// `auto_rename_pending` flag was already cleared at the
		// caller's send-time critical section so a second send
		// can't double-spawn us.
		let (dir, header_snapshot, transcript) = {
			let session = fs.session.lock().await;
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
			ChatMessage::User { content: transcript },
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
		let mut session = fs.session.lock().await;
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
			Some('-') => {
				if !last_dash && !out.is_empty() {
					out.push('-');
					last_dash = true;
				}
			}
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
fn emit_replay_events(sink: &FolderEventSink, record: SessionRecord) {
	match record {
		SessionRecord::User { text } => {
			sink.send(CoderEvent::UserMessage {
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
				sink.send(CoderEvent::AssistantMessageStart { id: id.clone() });
				sink.send(CoderEvent::AssistantMessageEnd {
					id,
					text: content.unwrap_or_default(),
					thinking: thinking.filter(|t| !t.is_empty()),
				});
			}
			for call in tool_calls {
				let args = parse_tool_args(&call.function);
				sink.send(CoderEvent::ToolCall {
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
			sink.send(CoderEvent::ToolResult {
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

/// Rough byte-count of every chat message — covers system / user /
/// assistant / tool. Includes `tool_calls` argument strings since
/// those land in the prompt verbatim and can be substantial when
/// the model emits long structured arguments.
pub(crate) fn estimate_prompt_tokens(messages: &[ChatMessage]) -> u32 {
	let mut bytes: usize = 0;
	for msg in messages {
		match msg {
			ChatMessage::System { content } | ChatMessage::User { content } => bytes += content.len(),
			ChatMessage::Assistant { content, tool_calls } => {
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
	(bytes / 4) as u32
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
