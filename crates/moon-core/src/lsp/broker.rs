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
	container_binary_path, discover_binary, resolve_install_hint, LspBinarySpec, LspServer, LspServerEvent,
	PathTranslator, GO_SERVER, OXLINT_LANGUAGES, OXLINT_LINTER, PYTHON_SERVER, RUST_SERVER, TS_SERVER,
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
	/// Linter co-tenants — keyed by the linter's slot name
	/// (`"oxlint"`), populated lazily on the first `lsp_open` for
	/// a file whose language id appears in
	/// [`super::server::OXLINT_LANGUAGES`]. Each linter speaks
	/// LSP just like the language servers in `servers` and pushes
	/// its diagnostics through the same broadcast channel — they
	/// arrive on the frontend as `producer: "oxlint"` and the UI
	/// merges them with the language server's reports for the
	/// same path.
	///
	/// Lives in a separate map (rather than next to the
	/// language servers) because the lookup is by **linter
	/// name**, not file language id, and routing fans out two
	/// servers per file. Hover / completion / definition / rename
	/// only consult `servers`; only diagnostics-producing ops
	/// (`open` / `update` / `close` / `notify_files_changed` /
	/// `refresh_open_diagnostics`) fan out here.
	lint_servers: Mutex<HashMap<String, ServerSlot>>,
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
			lint_servers: Mutex::new(HashMap::new()),
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

	/// Optional linter co-tenant for `language_id`. Returns the
	/// linter's spec when one applies — today only oxlint for
	/// JS/TS — and `None` when there's no linter wired for that
	/// language. Linters publish their own diagnostics alongside
	/// the language server's; they don't replace it.
	fn lint_spec_for(language_id: &str) -> Option<&'static LspBinarySpec> {
		if OXLINT_LANGUAGES.contains(&language_id) {
			Some(&OXLINT_LINTER)
		} else {
			None
		}
	}

	/// The map of [`ServerSlot`]s a given spec lives in. Language
	/// servers (TS / Rust / Python / Go) live in `servers`;
	/// linter co-tenants (oxlint) live in `lint_servers`. Two
	/// maps so a single language id can have **both** a language
	/// server and a linter slot keyed by their respective
	/// `spec.language_id` ("typescript" vs "oxlint") without
	/// collision.
	fn slot_map_for(&self, spec: &LspBinarySpec) -> &Mutex<HashMap<String, ServerSlot>> {
		if spec.language_id == OXLINT_LINTER.language_id {
			&self.lint_servers
		} else {
			&self.servers
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
	/// cached in the matching slot map so a missing server
	/// doesn't re-probe on every subsequent open.
	async fn ensure_server(&self, spec: &LspBinarySpec) -> Result<Option<Arc<LspServer>>, LspClientError> {
		let log_source = Self::log_source_for(spec.language_id);
		let slot_map = self.slot_map_for(spec);
		{
			let mut servers = slot_map.lock().await;
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
				slot_map
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
				slot_map
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
					slot_map
						.lock()
						.await
						.insert(spec.language_id.to_owned(), ServerSlot::Ready(server.clone()));
					self.log_sink.info(&log_source, "server ready on host fallback route");
					return Ok(Some(server));
				}
				SpawnOutcome::Err(e) => {
					slot_map
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

		slot_map
			.lock()
			.await
			.insert(spec.language_id.to_owned(), ServerSlot::NotAvailable);
		// Resolve once: the helper picks `pnpm -wD add` / `npm i -D` /
		// `bun add -D` based on the lockfile at the workspace root
		// for the package-manager-aware specs (TypeScript, oxlint).
		// Other servers have a single canonical install path and
		// fall through to the static `install_hint`.
		let install_hint = resolve_install_hint(spec, &self.root);
		let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
			language_id: spec.language_id.to_owned(),
			status: mp::LspServerStatus::NotAvailable,
			// Surface the install command directly in the pill
			// tooltip so the user has copy-pasteable next steps
			// without leaving the IDE.
			detail: Some(install_hint.clone()),
		}));
		self
			.log_sink
			.warn(&log_source, format!("not available; install hint: {install_hint}"));
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
		// Fan out to the language server (if any) AND the linter
		// co-tenant (if any). Either being missing is fine; only
		// truly hard errors propagate. Errors are independent: a
		// failed linter open shouldn't stop the language server's
		// open from going through.
		if let Some(spec) = Self::spec_for(language_id) {
			if let Some(server) = self.ensure_server(spec).await? {
				server.open(path, text.clone(), language_id).await?;
			}
		}
		if let Some(spec) = Self::lint_spec_for(language_id) {
			if let Some(server) = self.ensure_server(spec).await? {
				server.open(path, text, language_id).await?;
			}
		}
		Ok(())
	}

	/// Forward a buffer's latest text to every server that covers
	/// `language_id`. Routed through [`LspServer::open`] (not
	/// `update`) so a respawned server — fresh slot, empty `docs`
	/// map — auto-attaches the buffer on the next keystroke instead
	/// of silently dropping the change. Concretely: oxlint died (or
	/// was restarted via the diag-logs panel), `ensure_server`
	/// minted a clean slot, and the frontend's `lspScheduleUpdate`
	/// debounce is the first thing that reaches the new process. We
	/// want that to be `didOpen` if the new process has never seen
	/// the file, and `didChange` if it has — `LspServer::open`
	/// already keys on its own `docs` to pick the right one. Without
	/// this, the linter pill flips green again but its diagnostics
	/// would freeze on the pre-crash snapshot until the user
	/// switched tabs and back.
	pub async fn update(&self, path: &str, text: String, language_id: &str) -> Result<(), LspClientError> {
		if let Some(spec) = Self::spec_for(language_id) {
			if let Some(server) = self.ensure_server(spec).await? {
				server.open(path, text.clone(), language_id).await?;
			}
		}
		if let Some(spec) = Self::lint_spec_for(language_id) {
			if let Some(server) = self.ensure_server(spec).await? {
				server.open(path, text, language_id).await?;
			}
		}
		Ok(())
	}

	pub async fn close(&self, path: &str, language_id: &str) -> Result<(), LspClientError> {
		// Don't spawn either server just to close; if neither is up
		// there's nothing to do.
		if let Some(spec) = Self::spec_for(language_id) {
			let server = {
				let servers = self.servers.lock().await;
				match servers.get(spec.language_id) {
					Some(ServerSlot::Ready(s)) => Some(s.clone()),
					_ => None,
				}
			};
			if let Some(s) = server {
				s.close(path).await?;
			}
		}
		if let Some(spec) = Self::lint_spec_for(language_id) {
			let server = {
				let lint_servers = self.lint_servers.lock().await;
				match lint_servers.get(spec.language_id) {
					Some(ServerSlot::Ready(s)) => Some(s.clone()),
					_ => None,
				}
			};
			if let Some(s) = server {
				s.close(path).await?;
			}
		}
		Ok(())
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

	pub async fn prepare_rename(
		&self,
		path: &str,
		language_id: &str,
		position: mp::LspPosition,
		fallback_word: &str,
	) -> Result<Option<mp::LspPrepareRename>, LspClientError> {
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(None);
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(None);
		};
		server.prepare_rename(path, position, fallback_word).await
	}

	pub async fn rename(
		&self,
		path: &str,
		language_id: &str,
		position: mp::LspPosition,
		new_name: &str,
	) -> Result<mp::LspWorkspaceEdit, LspClientError> {
		let empty = mp::LspWorkspaceEdit::default();
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(empty);
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(empty);
		};
		server.rename(path, position, new_name).await
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

	/// Resolve one completion item against the matching language
	/// server. The frontend ships back the opaque token we issued
	/// in [`Self::completion`]; we hand it to the right
	/// [`LspServer`] which round-trips through
	/// `completionItem/resolve` to fetch lazy-resolved fields
	/// (auto-import edits, full documentation, etc.).
	///
	/// Returns the empty completion item when the language has no
	/// server registered or its server isn't running — the
	/// frontend treats that as "fall back to whatever
	/// `additionalTextEdits` we already had", which is the right
	/// behaviour both for unsupported languages and for the
	/// lifecycle window between server crash and broker restart.
	pub async fn completion_resolve(
		&self,
		language_id: &str,
		resolve_token: &str,
	) -> Result<mp::LspCompletionItem, LspClientError> {
		let empty = mp::LspCompletionItem {
			label: String::new(),
			kind: None,
			detail: None,
			documentation: None,
			insert_text: None,
			sort_text: None,
			filter_text: None,
			text_edit: None,
			additional_text_edits: Vec::new(),
			resolve_token: None,
		};
		let Some(spec) = Self::spec_for(language_id) else {
			return Ok(empty);
		};
		let Some(server) = self.ensure_server(spec).await? else {
			return Ok(empty);
		};
		server.completion_resolve(resolve_token).await
	}

	/// Forward a host fs-watcher batch to every running server,
	/// scoped per-server through the globs that server registered
	/// for `workspace/didChangeWatchedFiles`. Servers that didn't
	/// register watchers (rust-analyzer post-init, push-only
	/// servers that ignore the capability, …) silently no-op
	/// inside `LspServer::notify_files_changed`, so the broad
	/// fan-out is cheap.
	///
	/// This is the canonical LSP signal for off-disk file changes
	/// (a `git checkout`, an external editor save, a coder tool
	/// rewriting an unopened file). On reception, well-behaved
	/// servers invalidate their per-file caches and — if we
	/// advertised `workspace.diagnostics.refreshSupport` (we do)
	/// — request a workspace-wide diagnostic refresh, which loops
	/// back through the notification pump into
	/// `refresh_open_diagnostics` for that server.
	///
	/// Errors are logged at warn level rather than propagated:
	/// notify is fire-and-forget by design, and a single server
	/// failing to receive the batch shouldn't block the others.
	pub async fn notify_files_changed(&self, paths: &[String]) {
		let mut servers: Vec<Arc<LspServer>> = collect_alive(&self.servers).await;
		servers.extend(collect_alive(&self.lint_servers).await);
		for server in servers {
			if let Err(err) = server.notify_files_changed(paths).await {
				tracing::warn!(error = %err, "lsp: notify_files_changed failed");
			}
		}
	}

	/// Re-pull diagnostics for every open document on every
	/// running server. The IDE calls this when an out-of-band file
	/// change lands (fs-watcher detected a `git checkout` /
	/// external editor save / coder-tool-driven rewrite of an
	/// unopened file) or when the window regains focus, so stale
	/// diagnostics computed against a previous filesystem state
	/// repaint without the user having to retype.
	///
	/// `language_filter` lets the caller scope the fan-out to only
	/// the servers whose language matches at least one path in the
	/// triggering event — a `.toml` change shouldn't poke
	/// `tsserver`. Pass `None` to refresh every running server
	/// (the focus-event path).
	///
	/// Slots in `Pending`, `NotAvailable`, or `Failed` are skipped
	/// silently: a server that hasn't even started can't have
	/// stale diagnostics yet, and one we know we can't spawn isn't
	/// going to start now. Push-only servers (rust-analyzer) noop
	/// the pull internally at debug-log level, so the broad
	/// fan-out stays cheap.
	pub async fn refresh_open_diagnostics(&self, language_filter: Option<&[String]>) {
		// Language servers: filter by their slot key (which equals
		// the file's language id for tsgo / rust-analyzer / ty /
		// gopls).
		let mut servers: Vec<Arc<LspServer>> = {
			let guard = self.servers.lock().await;
			guard
				.iter()
				.filter_map(|(lang, slot)| {
					let ServerSlot::Ready(server) = slot else {
						return None;
					};
					if let Some(filter) = language_filter {
						if !filter.iter().any(|l| l == lang) {
							return None;
						}
					}
					if !server.is_alive() {
						return None;
					}
					Some(server.clone())
				})
				.collect()
		};
		// Linter co-tenants: filter by *file* language id, since
		// the linter's slot key ("oxlint") is the producer name,
		// not a file language. Treat the linter as relevant when
		// the filter contains any language it covers.
		let lint_servers: Vec<Arc<LspServer>> = {
			let guard = self.lint_servers.lock().await;
			guard
				.iter()
				.filter_map(|(lint_name, slot)| {
					let ServerSlot::Ready(server) = slot else {
						return None;
					};
					if let Some(filter) = language_filter {
						let covers = if lint_name == OXLINT_LINTER.language_id {
							filter.iter().any(|l| OXLINT_LANGUAGES.contains(&l.as_str()))
						} else {
							false
						};
						if !covers {
							return None;
						}
					}
					if !server.is_alive() {
						return None;
					}
					Some(server.clone())
				})
				.collect()
		};
		servers.extend(lint_servers);
		for server in servers {
			server.refresh_open_diagnostics().await;
		}
	}

	/// Shut every spawned server down and drop them. Called on
	/// workspace close. Best-effort — any hung server will SIGKILL
	/// on its `Child` drop regardless.
	pub async fn shutdown_all(&self) {
		let mut all: Vec<(String, ServerSlot)> = {
			let mut servers = self.servers.lock().await;
			servers.drain().collect()
		};
		{
			let mut lint_servers = self.lint_servers.lock().await;
			all.extend(lint_servers.drain());
		}
		for (lang, slot) in all {
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

	/// Tear down the server slot for `language_id`. Same end state
	/// as the crash-recovery path: the broker forgets the slot and
	/// the next request for this language lazily re-spawns. Emits
	/// `Stopped` so the status pill flips while the next spawn is
	/// in flight. No-op when no server slot exists for the
	/// language — restarting an LSP that never ran is a sensible
	/// idempotent (e.g. the user clicked "Restart" on a freshly
	/// opened diag-logs tab whose server hadn't actually spun up
	/// yet). Anything in `Failed` / `Pending` is dropped without
	/// a shutdown call.
	pub async fn shutdown_language(&self, language_id: &str) {
		// Language id here is also the slot key, so it works for
		// both maps. A "shutdown_language('typescript')" only kills
		// `tsgo`, leaving `oxlint` alone — and vice versa for
		// "shutdown_language('oxlint')". The Restart pill in the
		// status bar is per-pill, so the per-slot semantics match
		// what the user clicked on.
		let slot = {
			let mut servers = self.servers.lock().await;
			servers.remove(language_id)
		};
		let slot = match slot {
			Some(s) => Some(s),
			None => {
				let mut lint_servers = self.lint_servers.lock().await;
				lint_servers.remove(language_id)
			}
		};
		let Some(slot) = slot else {
			return;
		};
		let log_source = Self::log_source_for(language_id);
		self
			.log_sink
			.info(&log_source, "restart requested; tearing down server slot");
		if let ServerSlot::Ready(server) = slot {
			server.shutdown().await;
		}
		let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
			language_id: language_id.to_string(),
			status: mp::LspServerStatus::Stopped,
			detail: None,
		}));
	}
}

/// Snapshot of every alive [`LspServer`] in a slot map. Returned as
/// owned `Arc`s so the caller can drop the lock before doing I/O on
/// any of them — the broker's broadcast operations (notify-files-
/// changed, refresh-diagnostics) call into the servers, which take
/// their own locks; holding the slot map while doing that risks
/// deadlock with `ensure_server` paths.
async fn collect_alive(map: &Mutex<HashMap<String, ServerSlot>>) -> Vec<Arc<LspServer>> {
	let guard = map.lock().await;
	guard
		.iter()
		.filter_map(|(_, slot)| {
			let ServerSlot::Ready(server) = slot else {
				return None;
			};
			if !server.is_alive() {
				return None;
			}
			Some(server.clone())
		})
		.collect()
}
