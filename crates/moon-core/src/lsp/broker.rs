//! Multi-language LSP broker.
//!
//! The broker is the single thing the Tauri layer talks to. It:
//!
//! - Owns zero or more [`LspServer`]s keyed by LSP language id.
//! - Routes file-open / update / close / hover / completion calls to
//!   the right one, lazily spawning a server the first time it sees a
//!   language.
//! - Maintains a per-language status (NotAvailable / Starting /
//!   Running / Crashed / Stopped) that the UI pins to the status bar.
//! - Fans out server events (currently diagnostics + status
//!   transitions) through a broadcast channel the Tauri layer listens
//!   on.
//!
//! Workspace scope: one broker per workspace, pointed at the
//! workspace root. Changing the active folder does **not** re-point
//! the broker — different folders can be inside the same
//! `tsconfig.json` project, and TS servers cope with multi-project
//! roots better than they cope with being torn down and re-spawned.
//! Closing the workspace tears everything down.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_protocol::lsp as mp;
use tokio::sync::{broadcast, Mutex};

use super::client::LspClientError;
use super::server::{
	container_binary_path, discover_binary, LspBinarySpec, LspServer, LspServerEvent, PathTranslator, GO_SERVER,
	PYTHON_SERVER, RUST_SERVER, TS_SERVER,
};
use super::spawn::LspSpawner;
use crate::logs::LogSink;

pub struct LspBroker {
	root: Utf8PathBuf,
	/// Where new [`LspServer`]s run by default (host vs.
	/// `docker exec` into the workspace container). Chosen once,
	/// at broker-creation time, by the Tauri layer based on the
	/// current container state — if that state changes, the
	/// broker gets torn down and rebuilt rather than mutated in
	/// place.
	primary: SpawnerPair,
	/// Host-side fallback route. Populated when `primary` is
	/// `DockerExec` so a language whose binary can't be reached
	/// in the container (hoisted `node_modules`, custom image
	/// that dropped the server, etc.) transparently spawns on
	/// the host instead — matching the routing table in
	/// `specs/lsp.md#container-backed-lsp`. `None` when `primary`
	/// is already the host; we don't stack another fallback.
	fallback: Option<SpawnerPair>,
	servers: Mutex<HashMap<String, ServerSlot>>,
	events: broadcast::Sender<LspServerEvent>,
	/// Diagnostic log sink. Receives the same status transitions
	/// the `events` channel does, plus routing decisions
	/// (primary-vs-fallback, discovery hits/misses) the UI can
	/// pull up in the bottom-panel logs view. Every broker gets a
	/// sink — tests construct a standalone one and ignore it.
	/// See [`crate::logs`].
	log_sink: Arc<LogSink>,
}

/// One "how to spawn" target: the spawner plus the translator
/// servers built against it need for URI construction. Stays
/// private to the broker; callers work with the broker's public
/// surface and never see the pair directly.
#[derive(Clone)]
struct SpawnerPair {
	spawner: LspSpawner,
	translator: PathTranslator,
}

enum ServerSlot {
	/// Already spawned and initialised. Taking this variant is a
	/// fast path: no I/O beyond the actual request.
	Ready(Arc<LspServer>),
	/// Earlier spawn attempt found no binary — don't retry within
	/// the same workspace session. A later `rediscover` (manual
	/// palette hook) could flip this back to None on demand; for
	/// now the user restarts moon-ide after installing the server.
	NotAvailable,
}

/// Result of a single spawn attempt on one route. Distinguishes
/// "binary not available here, try another route" from "genuine
/// spawn / init failure, bail" — the first should cascade to the
/// fallback, the second should not.
enum SpawnOutcome {
	Ready(Arc<LspServer>),
	Unavailable,
	Err(LspClientError),
}

/// Subscription handle the Tauri layer uses to listen for server
/// events. One receiver per subscriber; the broker owns the sender.
pub type LspEventRx = broadcast::Receiver<LspServerEvent>;

impl LspBroker {
	/// Host-only broker. Equivalent to
	/// `new_with_spawner(root, LspSpawner::Local, Identity{ root }, LogSink::new())`;
	/// kept as a thin helper so existing call sites and tests that
	/// don't care about container routing or log routing stay
	/// readable. Tests get a fresh standalone sink they can drop.
	pub fn new(root: Utf8PathBuf) -> Arc<Self> {
		let translator = PathTranslator::Identity {
			host_root: root.clone(),
		};
		Self::new_with_spawner(root, LspSpawner::Local, translator, LogSink::new())
	}

	/// Full form. The Tauri layer uses this to plug in
	/// [`LspSpawner::DockerExec`] + a `HostMount` translator when
	/// the workspace container is running and ships the requested
	/// LSP. See `specs/lsp.md#container-backed-lsp` for the
	/// routing table.
	///
	/// `log_sink` collects routing decisions and status changes for
	/// the bottom-panel logs view (see [`crate::logs`]). It's
	/// non-optional on purpose — every production broker has a
	/// sink, and making it `Option` would mean a needless branch
	/// at every emit site. Tests pass a fresh [`LogSink::new()`]
	/// and ignore the broadcast end.
	///
	/// When `spawner` is `DockerExec`, the broker auto-populates a
	/// host fallback from the translator's host root so any server
	/// whose binary can't be resolved inside the container (hoisted
	/// node_modules, custom image) still runs on the host. No
	/// fallback is kept when `spawner` is already `Local` — there
	/// is nowhere lower to drop to.
	pub fn new_with_spawner(
		root: Utf8PathBuf,
		spawner: LspSpawner,
		translator: PathTranslator,
		log_sink: Arc<LogSink>,
	) -> Arc<Self> {
		let fallback = match &spawner {
			LspSpawner::DockerExec { .. } => {
				let host_root = translator.host_root().clone();
				Some(SpawnerPair {
					spawner: LspSpawner::Local,
					translator: PathTranslator::Identity { host_root },
				})
			}
			LspSpawner::Local => None,
		};
		// 256 is more than a human can generate in a frame; the TS
		// server publishes a couple per second per dirty file at
		// worst. Overflow drops oldest which is fine — the
		// frontend caches per-path and the next publish replaces
		// whatever was missed.
		let (events, _) = broadcast::channel::<LspServerEvent>(256);
		Arc::new(Self {
			root,
			primary: SpawnerPair { spawner, translator },
			fallback,
			servers: Mutex::new(HashMap::new()),
			events,
			log_sink,
		})
	}

	/// Source key used by the broker for its own log emissions.
	/// Per-language so the picker's grouping (`lsp.typescript`,
	/// `lsp.rust`, …) matches what the user sees in the status
	/// bar pill — easier to mental-model than a single shared
	/// `lsp` bucket.
	fn log_source_for(language_id: &str) -> String {
		format!("lsp.{language_id}")
	}

	pub fn subscribe(&self) -> LspEventRx {
		self.events.subscribe()
	}

	/// Map a frontend language id (what the user's file types into)
	/// to an LSP server spec. `None` when no server is wired up for
	/// that language yet; callers should no-op in that case rather
	/// than surface "unknown language" errors — plenty of file
	/// types have no LSP (e.g. Markdown in stage 1).
	fn spec_for(language_id: &str) -> Option<&'static LspBinarySpec> {
		// One entry per broker server, not per file-extension: `tsgo`
		// handles all four JS/TS flavours in a single process, and
		// `rust-analyzer` handles everything a `.rs` file asks for.
		match language_id {
			"typescript" | "typescriptreact" | "javascript" | "javascriptreact" => Some(&TS_SERVER),
			"rust" => Some(&RUST_SERVER),
			"python" => Some(&PYTHON_SERVER),
			"go" => Some(&GO_SERVER),
			_ => None,
		}
	}

	/// Ensure a server for `spec` is running and return a cloned
	/// handle. Spawns on first call. On spawn failure (binary
	/// missing, bad args, child exits immediately) caches a
	/// `NotAvailable` slot and returns `Ok(None)` — the caller
	/// treats missing LSP as a feature that just isn't on, not an
	/// error that stops the editor.
	///
	/// Routing: tries `primary` first (container when one is up,
	/// host otherwise). On any miss (binary not found, probe
	/// fails, spawn rejects) transparently retries on `fallback`
	/// if one is configured — concretely, a container-backed
	/// broker falls back to host LSP for servers whose binary
	/// isn't in the container. The per-language outcome is
	/// cached in the `servers` map so a missing server doesn't
	/// re-probe on every subsequent open.
	async fn ensure_server(&self, spec: &LspBinarySpec) -> Result<Option<Arc<LspServer>>, LspClientError> {
		let log_source = Self::log_source_for(spec.language_id);
		{
			let mut servers = self.servers.lock().await;
			if let Some(slot) = servers.get(spec.language_id) {
				match slot {
					ServerSlot::Ready(s) if s.is_alive() => return Ok(Some(s.clone())),
					ServerSlot::Ready(_) => {
						// Cached server died (the LspServer's
						// death-watcher already logged + flipped
						// the status pill). Evict so the spawn
						// path below mints a fresh one. The doc
						// state from the old server is lost; the
						// frontend's `lsp:status` Crashed
						// listener re-opens the active file, so
						// the next request after this one
						// resolves against a primed server.
						servers.remove(spec.language_id);
						self
							.log_sink
							.info(&log_source, "re-spawning after detected death of previous server");
					}
					ServerSlot::NotAvailable => return Ok(None),
				}
			}
		}

		// Announce `Starting` before the spawn so the UI shows a
		// transient pill even if the spawn takes a while (first-run
		// tsserver + tsconfig parsing).
		let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
			language_id: spec.language_id.to_owned(),
			status: mp::LspServerStatus::Starting,
			detail: Some(spec.bin_name.to_owned()),
		}));
		self
			.log_sink
			.info(&log_source, format!("starting server (bin = {})", spec.bin_name));

		match self.try_spawn_on(spec, &self.primary).await {
			SpawnOutcome::Ready(server) => {
				self
					.servers
					.lock()
					.await
					.insert(spec.language_id.to_owned(), ServerSlot::Ready(server.clone()));
				self.log_sink.info(&log_source, "server ready on primary route");
				return Ok(Some(server));
			}
			SpawnOutcome::Err(e) => {
				// A genuine spawn error (child started then died
				// inside LSP init, framing fault, etc.) is fatal
				// for this language. Falling back to host on an
				// init-level crash would mask bugs in the server
				// or the image; surface it instead.
				self
					.servers
					.lock()
					.await
					.insert(spec.language_id.to_owned(), ServerSlot::NotAvailable);
				let detail = e.to_string();
				let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
					language_id: spec.language_id.to_owned(),
					status: mp::LspServerStatus::Crashed,
					detail: Some(detail.clone()),
				}));
				self
					.log_sink
					.error(&log_source, format!("spawn failed on primary route: {detail}"));
				return Err(e);
			}
			SpawnOutcome::Unavailable => {
				// Binary wasn't found or `--version` probe
				// exited non-zero. Fall through to the host
				// fallback if one is configured.
				self
					.log_sink
					.warn(&log_source, "binary not found on primary route (container)");
			}
		}

		if let Some(fallback) = &self.fallback {
			tracing::info!(
				bin = spec.bin_name,
				lang = spec.language_id,
				"lsp: primary (container) unavailable, retrying on host fallback"
			);
			self.log_sink.info(&log_source, "retrying on host fallback route");
			match self.try_spawn_on(spec, fallback).await {
				SpawnOutcome::Ready(server) => {
					self
						.servers
						.lock()
						.await
						.insert(spec.language_id.to_owned(), ServerSlot::Ready(server.clone()));
					self.log_sink.info(&log_source, "server ready on host fallback route");
					return Ok(Some(server));
				}
				SpawnOutcome::Err(e) => {
					self
						.servers
						.lock()
						.await
						.insert(spec.language_id.to_owned(), ServerSlot::NotAvailable);
					let detail = e.to_string();
					let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
						language_id: spec.language_id.to_owned(),
						status: mp::LspServerStatus::Crashed,
						detail: Some(detail.clone()),
					}));
					self
						.log_sink
						.error(&log_source, format!("spawn failed on host fallback: {detail}"));
					return Err(e);
				}
				SpawnOutcome::Unavailable => {
					// Host fallback also missing the binary.
					// Fall through to the NotAvailable pill.
					self
						.log_sink
						.warn(&log_source, "binary not found on host fallback either");
				}
			}
		}

		self
			.servers
			.lock()
			.await
			.insert(spec.language_id.to_owned(), ServerSlot::NotAvailable);
		let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
			language_id: spec.language_id.to_owned(),
			status: mp::LspServerStatus::NotAvailable,
			// Surface the install command directly in the pill
			// tooltip so the user has copy-pasteable next steps
			// without leaving the IDE.
			detail: Some(spec.install_hint.to_owned()),
		}));
		self.log_sink.warn(
			&log_source,
			format!("not available; install hint: {}", spec.install_hint),
		);
		Ok(None)
	}

	/// One attempt at bringing up a server on a given route.
	///
	/// `Unavailable` means "this route has no copy of the
	/// binary" — a non-error signal that the caller should try
	/// another route (fallback) or surface `NotAvailable`.
	/// `Err` is a real spawn / init failure that should bubble
	/// all the way out.
	async fn try_spawn_on(&self, spec: &LspBinarySpec, route: &SpawnerPair) -> SpawnOutcome {
		let log_source = Self::log_source_for(spec.language_id);
		let bin_path: PathBuf = match &route.spawner {
			LspSpawner::Local => match discover_binary(spec.bin_name, spec.discovery, self.root.as_std_path()) {
				Some(p) => {
					self
						.log_sink
						.debug(&log_source, format!("host discovery → {}", p.display()));
					p
				}
				None => {
					tracing::info!(
						bin = spec.bin_name,
						lang = spec.language_id,
						"lsp: host binary discovery found nothing"
					);
					self.log_sink.debug(
						&log_source,
						format!(
							"host discovery: no `{}` found on PATH or ecosystem paths",
							spec.bin_name
						),
					);
					return SpawnOutcome::Unavailable;
				}
			},
			LspSpawner::DockerExec { .. } => match container_binary_path(spec, &route.translator) {
				Some(p) => {
					self
						.log_sink
						.debug(&log_source, format!("container discovery → {}", p.display()));
					p
				}
				None => {
					tracing::info!(
						bin = spec.bin_name,
						lang = spec.language_id,
						"lsp: container binary path unresolved (likely node_modules above mount or missing)"
					);
					self.log_sink.debug(
						&log_source,
						format!(
							"container discovery: no `{}` found below the bind mount (hoisted node_modules?)",
							spec.bin_name
						),
					);
					return SpawnOutcome::Unavailable;
				}
			},
		};

		// `<bin> <probe_args…>` keeps us honest: resolving a
		// path doesn't prove the file is executable on the
		// target (Linux-only binary in a node_modules installed
		// from a macOS host, or a rustup shim for a component
		// that isn't actually installed). The probe uses the
		// same build-command pipeline the real spawn will, so
		// if it clears we know framing + stdio wiring work.
		// Argv is per-spec — most servers accept `--version`,
		// gopls is the odd one out with subcommand syntax.
		let bin_str = bin_path.to_string_lossy();
		if !route.spawner.probe(&bin_str, spec.probe_args).await {
			tracing::info!(
				bin = spec.bin_name,
				lang = spec.language_id,
				path = %bin_str,
				probe_args = ?spec.probe_args,
				"lsp: probe failed on this route"
			);
			self
				.log_sink
				.warn(&log_source, format!("probe `{} {:?}` failed", bin_str, spec.probe_args));
			return SpawnOutcome::Unavailable;
		}

		match LspServer::spawn(
			spec,
			&bin_path,
			&route.spawner,
			route.translator.clone(),
			self.events.clone(),
			self.log_sink.clone(),
		)
		.await
		{
			Ok(Some(server)) => SpawnOutcome::Ready(server),
			Ok(None) => SpawnOutcome::Unavailable,
			Err(e) => {
				tracing::warn!(error = %e, lang = spec.language_id, "lsp: spawn failed");
				SpawnOutcome::Err(e)
			}
		}
	}

	pub async fn open(&self, path: &str, text: String, language_id: &str) -> Result<(), LspClientError> {
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(());
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(());
		};
		server.open(path, text, language_id).await
	}

	pub async fn update(&self, path: &str, text: String, language_id: &str) -> Result<(), LspClientError> {
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(());
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(());
		};
		server.update(path, text).await
	}

	pub async fn close(&self, path: &str, language_id: &str) -> Result<(), LspClientError> {
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(());
		};
		// Don't spawn just to close; if the server isn't up there's
		// nothing to do.
		let server = {
			let servers = self.servers.lock().await;
			match servers.get(spec.language_id) {
				Some(ServerSlot::Ready(s)) => s.clone(),
				_ => return Ok(()),
			}
		};
		server.close(path).await
	}

	pub async fn hover(
		&self,
		path: &str,
		language_id: &str,
		position: mp::LspPosition,
	) -> Result<Option<mp::LspHover>, LspClientError> {
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(None);
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(None);
		};
		server.hover(path, position).await
	}

	pub async fn definition(
		&self,
		path: &str,
		language_id: &str,
		position: mp::LspPosition,
	) -> Result<Option<mp::LspLocation>, LspClientError> {
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(None);
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(None);
		};
		server.definition(path, position).await
	}

	pub async fn completion(
		&self,
		path: &str,
		language_id: &str,
		position: mp::LspPosition,
	) -> Result<mp::LspCompletionList, LspClientError> {
		let empty = mp::LspCompletionList {
			is_incomplete: false,
			items: vec![],
		};
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(empty);
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(empty);
		};
		server.completion(path, position).await
	}

	/// Shut every spawned server down and drop them. Called on
	/// workspace close. Best-effort — any hung server will SIGKILL
	/// on its `Child` drop regardless.
	pub async fn shutdown_all(&self) {
		let mut servers = self.servers.lock().await;
		let slots: Vec<_> = servers.drain().collect();
		for (lang, slot) in slots {
			if let ServerSlot::Ready(server) = slot {
				server.shutdown().await;
				let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
					language_id: lang,
					status: mp::LspServerStatus::Stopped,
					detail: None,
				}));
			}
		}
	}
}
