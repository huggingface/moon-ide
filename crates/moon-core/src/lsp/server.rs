//! Per-language LSP server actor.
//!
//! One [`LspServer`] owns one child process (e.g. `tsgo --lsp --stdio`),
//! an [`LspClient`] on top of its stdio, and a map of open documents.
//! Its public surface is narrow: `open` / `update` / `close` /
//! `hover` / `completion`. Nothing here knows about multiple
//! languages — the broker picks the right server for a language id.
//!
//! Lifecycle:
//! 1. `LspServer::spawn` locates the binary, starts the child,
//!    builds the client, sends `initialize` + `initialized`.
//! 2. The broker calls `open` / `update` / `close` as the user
//!    interacts with buffers. We send full-document sync
//!    (`TextDocumentSyncKind::FULL`) for now — correctness over
//!    throughput while the client surface is tiny.
//! 3. `publishDiagnostics` notifications are forwarded out through
//!    the broker's event channel (translated to
//!    `moon_protocol::lsp` shapes).
//! 4. Dropping the server sends `shutdown` + `exit` so the child
//!    can flush, then aborts if it hangs.
//!
//! stderr of the child process is piped to `tracing::debug` — LSP
//! servers are chatty on stderr about things we'd rather not see in
//! the user log at INFO.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use camino::Utf8PathBuf;
use lsp_types as lt;
use moon_protocol::lsp as mp;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{broadcast, Mutex};

use super::client::{LspClient, LspClientError, ServerNotification};
use super::spawn::LspSpawner;
use super::translate;

/// What the broker hears when something interesting happens in a
/// server — feeds through to the Tauri event layer.
#[derive(Debug, Clone)]
pub enum LspServerEvent {
	Diagnostics(mp::LspDiagnosticsEvent),
	StatusChanged(mp::LspStatusEvent),
}

/// Which binary to spawn for a given LSP language. One entry per
/// language id we intend to support.
///
/// `install_hint` is what the status bar's "not available" pill
/// suggests — short and actionable, no prose. On the happy path the
/// hint is never shown; it only surfaces when discovery failed.
pub struct LspBinarySpec {
	pub language_id: &'static str,
	pub bin_name: &'static str,
	pub args: &'static [&'static str],
	pub install_hint: &'static str,
	pub discovery: DiscoveryStrategy,
}

/// Where to look for `bin_name` before falling through to `$PATH`.
///
/// Each strategy picks one ecosystem-idiomatic location: Node users
/// expect `node_modules/.bin/` to win even when there's a global
/// install, Rust users expect `~/.cargo/bin/` to be found even when
/// the Tauri process didn't inherit the shell's `PATH` (which
/// happens on GUI-launched apps on macOS / some Linux DEs).
#[derive(Clone, Copy)]
pub enum DiscoveryStrategy {
	/// Walk ancestors from the workspace root looking for
	/// `node_modules/.bin/<bin>`, then `$PATH`. Matches Node's own
	/// resolution so pnpm-hoisted monorepos work without special
	/// casing.
	NodeModules,
	/// Check `$CARGO_HOME/bin/<bin>` (falling back to
	/// `$HOME/.cargo/bin/<bin>`), then `$PATH`. Covers
	/// `rustup component add rust-analyzer` whose default install
	/// location isn't always on the launched Tauri process's
	/// inherited `PATH`.
	CargoHome,
}

/// TypeScript / JavaScript server.
///
/// We target `tsgo` (Microsoft's native Go port of TypeScript, shipped
/// as `@typescript/native-preview`) rather than the community
/// `typescript-language-server` wrapper. Two reasons:
///
/// 1. It's already in moon-ide's devDependencies (used by the
///    `check:ts` script) — no extra setup cost. Discovery finds it in
///    `node_modules/.bin/` automatically.
/// 2. `typescript-language-server`'s own README says it expects to be
///    superseded by TS 7 / `tsgo`. Adopting the native port now avoids
///    a migration later and gets the ~10× speed-up for free.
///
/// If a project ships `typescript-language-server` instead, flip this
/// spec — the LSP wire format is identical and nothing else has to
/// change. See [`specs/lsp.md`].
pub const TS_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "typescript",
	bin_name: "tsgo",
	args: &["--lsp", "--stdio"],
	install_hint: "bun add -D @typescript/native-preview",
	discovery: DiscoveryStrategy::NodeModules,
};

/// Rust server — `rust-analyzer`, the ecosystem-standard LSP.
///
/// No per-project install exists for Rust LSPs (unlike `tsgo`), so we
/// rely on the system toolchain: `rustup component add rust-analyzer`
/// drops it at `$CARGO_HOME/bin/rust-analyzer`, which is where we
/// look first. A `cargo install rust-analyzer` build lands in the
/// same place. `$PATH` is the last resort for hand-compiled or
/// package-manager-installed copies.
///
/// No args: `rust-analyzer` defaults to stdio + LSP, which is
/// exactly the contract we want. The binary auto-detects the
/// workspace layout from `initialize.workspaceFolders`, which the
/// generic `initialize` below already sends.
pub const RUST_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "rust",
	bin_name: "rust-analyzer",
	args: &[],
	install_hint: "rustup component add rust-analyzer",
	discovery: DiscoveryStrategy::CargoHome,
};

pub struct LspServer {
	language_id: String,
	client: LspClient,
	child: Mutex<Option<Child>>,
	/// Maps between host-relative paths and server-side file URIs.
	/// `Identity` for [`LspSpawner::Local`], `HostMount` when the
	/// server runs inside a container and sees `/workspace/<basename>`
	/// instead of the host absolute path. Replaces the old bare
	/// `root` field; all URI construction + parsing routes through
	/// here so the wire format matches what the server expects.
	translator: PathTranslator,
	// Per-document version counter. LSP requires monotonically
	// increasing versions per didChange to detect out-of-order
	// updates; we start at 1 on open and tick each change.
	docs: Mutex<HashMap<String, DocState>>,
	/// Fan-out sink the server uses to publish its own events
	/// (diagnostics, status changes). Cloned from the broker's
	/// channel so the pull-diagnostics task can shove `LspDiagnostics`
	/// events directly through the same surface as the push pump.
	events: broadcast::Sender<LspServerEvent>,
}

/// Bridge between host-relative workspace paths and the
/// `file://`-URIs exchanged with the LSP server.
///
/// - `Identity`: host and server see the same absolute paths.
///   `absolutise(rel)` = `host_root.join(rel)`;
///   `relativise(abs)` = `abs.strip_prefix(host_root)`.
/// - `HostMount`: server runs inside a container where the
///   workspace is bind-mounted under a different absolute root
///   (`/workspace/<basename>`). We keep the host root for
///   tree-side lookups and the server root for URI construction,
///   mapping between them symmetrically.
///
/// Only hosts / container variants live here. Remote / SSH
/// targets get their own variant when that lands; the enum is
/// the extension point.
#[derive(Debug, Clone)]
pub enum PathTranslator {
	Identity {
		host_root: Utf8PathBuf,
	},
	HostMount {
		host_root: Utf8PathBuf,
		server_root: Utf8PathBuf,
	},
}

impl PathTranslator {
	/// Host-relative → server-absolute path. Always joined; no
	/// normalisation. The caller is responsible for passing
	/// forward-slash-separated relatives (which is the moon-ide
	/// tree convention already).
	pub fn absolutise(&self, rel_path: &str) -> Utf8PathBuf {
		match self {
			PathTranslator::Identity { host_root } => host_root.join(rel_path),
			PathTranslator::HostMount { server_root, .. } => server_root.join(rel_path),
		}
	}

	/// Server-absolute → host-relative path. Returns `None` when
	/// the URI points outside the workspace (e.g. `rust-analyzer`
	/// publishing a diagnostic against a dep inside the container's
	/// `~/.cargo/registry/…`): the UI has no buffer for those so we
	/// silently drop them.
	pub fn relativise(&self, abs: &Path) -> Option<String> {
		let server_root = match self {
			PathTranslator::Identity { host_root } => host_root.as_std_path(),
			PathTranslator::HostMount { server_root, .. } => server_root.as_std_path(),
		};
		let rel = abs.strip_prefix(server_root).ok()?;
		Some(rel.to_string_lossy().replace('\\', "/"))
	}

	/// Absolute server root — what `initialize.rootUri` and
	/// `workspaceFolders[0].uri` need to point at so the server
	/// opens the right tree.
	pub fn server_root(&self) -> &Utf8PathBuf {
		match self {
			PathTranslator::Identity { host_root } => host_root,
			PathTranslator::HostMount { server_root, .. } => server_root,
		}
	}

	/// Host-side workspace root. Used by callers that need to
	/// cross-reference tree state (e.g. goto-definition response
	/// translation). Never use this for URI construction.
	pub fn host_root(&self) -> &Utf8PathBuf {
		match self {
			PathTranslator::Identity { host_root } => host_root,
			PathTranslator::HostMount { host_root, .. } => host_root,
		}
	}
}

struct DocState {
	version: i32,
}

impl LspServer {
	/// Locate and spawn the server binary. Returns `Ok(None)` when
	/// no copy can be found on disk — caller surfaces a
	/// `NotAvailable` status instead of treating this as an error.
	///
	/// Discovery order for [`LspSpawner::Local`]:
	/// 1. The spec's `DiscoveryStrategy` (project-local
	///    `node_modules/.bin`, or `$CARGO_HOME/bin` etc.).
	/// 2. `$PATH` via `which`.
	///
	/// For [`LspSpawner::DockerExec`] we skip host discovery
	/// entirely — the in-container binary is on the container's
	/// `$PATH` (moon-base handles installation) and we hand
	/// `docker exec` the basename so the container resolves it.
	/// The broker will have already run a `--version` probe to
	/// confirm availability before this spawn.
	pub async fn spawn(
		spec: &LspBinarySpec,
		bin_path: &Path,
		spawner: &LspSpawner,
		translator: PathTranslator,
		events: broadcast::Sender<LspServerEvent>,
	) -> Result<Option<Arc<Self>>, LspClientError> {
		let mut child = spawner
			.build_command(bin_path, spec.args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|e| LspClientError::Io(format!("spawn {}: {}", spec.bin_name, e)))?;

		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| LspClientError::Io("child stdin not piped".into()))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| LspClientError::Io("child stdout not piped".into()))?;
		let stderr = child
			.stderr
			.take()
			.ok_or_else(|| LspClientError::Io("child stderr not piped".into()))?;

		// Pipe stderr to tracing so a crash-loop is visible in
		// `RUST_LOG=moon=debug` without spamming info logs.
		let lang = spec.language_id.to_owned();
		tokio::spawn(async move {
			let mut reader = BufReader::new(stderr).lines();
			while let Ok(Some(line)) = reader.next_line().await {
				tracing::debug!(lang = %lang, "lsp stderr: {line}");
			}
		});

		let (notif_tx, mut notif_rx) = broadcast::channel::<ServerNotification>(64);
		let client = LspClient::spawn(stdin, stdout, notif_tx);

		let server = Arc::new(Self {
			language_id: spec.language_id.to_owned(),
			client,
			child: Mutex::new(Some(child)),
			translator,
			docs: Mutex::new(HashMap::new()),
			events: events.clone(),
		});

		// Notification pump: translate the ones we care about,
		// drop the rest. Currently: `textDocument/publishDiagnostics`.
		// Extension slot for `window/showMessage` etc. when we
		// actually surface them.
		let server_ref = server.clone();
		let events_sink = events.clone();
		// Push-diagnostics path. Most LSP servers (rust-analyzer,
		// typescript-language-server) deliver `publishDiagnostics`
		// notifications unsolicited; we forward them straight through.
		// `tsgo` (TypeScript native preview) deliberately does not
		// implement push diagnostics — see
		// <https://github.com/microsoft/typescript-go/issues/2362> —
		// so it relies on the pull path below (`pull_diagnostics`)
		// driven by `update` after every `didOpen` / `didChange`.
		tokio::spawn(async move {
			while let Ok(notif) = notif_rx.recv().await {
				if notif.method.as_str() != "textDocument/publishDiagnostics" {
					continue;
				}
				let params: lt::PublishDiagnosticsParams = match serde_json::from_value(notif.params) {
					Ok(p) => p,
					Err(e) => {
						tracing::warn!(error = %e, "lsp: bad publishDiagnostics payload");
						continue;
					}
				};
				let path = match server_ref.uri_to_relative(&params.uri) {
					Some(p) => p,
					None => continue,
				};
				let diagnostics = params.diagnostics.into_iter().map(translate::diagnostic).collect();
				let _ = events_sink.send(LspServerEvent::Diagnostics(mp::LspDiagnosticsEvent {
					path,
					diagnostics,
				}));
			}
		});

		server.initialize().await?;
		events
			.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
				language_id: spec.language_id.to_owned(),
				status: mp::LspServerStatus::Running,
				detail: Some(bin_path.display().to_string()),
			}))
			.ok();

		Ok(Some(server))
	}

	async fn initialize(&self) -> Result<(), LspClientError> {
		// Minimal client capabilities. We only claim the features
		// we actually wire up today; adding hover / definition /
		// references later just means flipping a flag here and
		// shipping the command that uses it.
		let caps = lt::ClientCapabilities {
			text_document: Some(lt::TextDocumentClientCapabilities {
				synchronization: Some(lt::TextDocumentSyncClientCapabilities {
					dynamic_registration: Some(false),
					will_save: Some(false),
					will_save_wait_until: Some(false),
					did_save: Some(false),
				}),
				publish_diagnostics: Some(lt::PublishDiagnosticsClientCapabilities {
					related_information: Some(false),
					tag_support: None,
					version_support: Some(false),
					code_description_support: Some(false),
					data_support: Some(false),
				}),
				hover: Some(lt::HoverClientCapabilities {
					dynamic_registration: Some(false),
					content_format: Some(vec![lt::MarkupKind::Markdown, lt::MarkupKind::PlainText]),
				}),
				definition: Some(lt::GotoCapability {
					dynamic_registration: Some(false),
					// `LocationLink` response lets servers distinguish
					// the full definition range from the identifier
					// span — our translator uses the identifier span
					// so the caret lands right.
					link_support: Some(true),
				}),
				completion: Some(lt::CompletionClientCapabilities {
					dynamic_registration: Some(false),
					completion_item: Some(lt::CompletionItemCapability {
						snippet_support: Some(false),
						documentation_format: Some(vec![lt::MarkupKind::Markdown, lt::MarkupKind::PlainText]),
						insert_replace_support: Some(false),
						..Default::default()
					}),
					completion_item_kind: None,
					context_support: Some(true),
					..Default::default()
				}),
				// LSP 3.17 pull-diagnostics. We declare support so
				// servers that prefer pull (notably `tsgo`, which
				// deliberately skips `publishDiagnostics`) know we'll
				// call `textDocument/diagnostic` ourselves after
				// every `didOpen` / `didChange`. Servers that only
				// support push (e.g. `rust-analyzer`) ignore this
				// flag and keep delivering notifications.
				diagnostic: Some(lt::DiagnosticClientCapabilities {
					dynamic_registration: Some(false),
					related_document_support: Some(false),
				}),
				..Default::default()
			}),
			..Default::default()
		};

		// Initialize with the **server-side** root — container
		// servers see `/workspace/<basename>`, host servers see
		// the host absolute path. The translator bridges which
		// one we're talking to; the LSP surface is agnostic.
		let server_root = self.translator.server_root();
		let root_uri = path_to_file_uri(server_root.as_std_path());

		#[allow(deprecated)]
		let params = lt::InitializeParams {
			process_id: Some(std::process::id()),
			root_path: None,
			root_uri: Some(root_uri.clone()),
			initialization_options: None,
			capabilities: caps,
			trace: None,
			workspace_folders: Some(vec![lt::WorkspaceFolder {
				uri: root_uri,
				name: server_root.file_name().unwrap_or("workspace").to_owned(),
			}]),
			client_info: Some(lt::ClientInfo {
				name: "moon-ide".into(),
				version: Some(env!("CARGO_PKG_VERSION").into()),
			}),
			locale: None,
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
		};

		let _: lt::InitializeResult = self.client.request("initialize", params).await?;
		self.client.notify("initialized", lt::InitializedParams {}).await?;
		Ok(())
	}

	/// Send `textDocument/didOpen`. Idempotent: a second open for
	/// the same path is routed as a change — editors that reopen
	/// a closed tab expect the server to pick up where they left
	/// off, not crash on a duplicate open.
	pub async fn open(self: &Arc<Self>, rel_path: &str, text: String, language_id: &str) -> Result<(), LspClientError> {
		let mut docs = self.docs.lock().await;
		let uri = self.relative_to_uri(rel_path);
		if let Some(state) = docs.get_mut(rel_path) {
			state.version += 1;
			let version = state.version;
			drop(docs);
			self.apply_change(&uri, version, text).await?;
			self.spawn_pull_diagnostics(rel_path);
			return Ok(());
		}
		let version = 1;
		let params = lt::DidOpenTextDocumentParams {
			text_document: lt::TextDocumentItem {
				uri,
				language_id: language_id.to_owned(),
				version,
				text,
			},
		};
		docs.insert(rel_path.to_owned(), DocState { version });
		drop(docs);
		self.client.notify("textDocument/didOpen", params).await?;
		self.spawn_pull_diagnostics(rel_path);
		Ok(())
	}

	pub async fn update(self: &Arc<Self>, rel_path: &str, text: String) -> Result<(), LspClientError> {
		let mut docs = self.docs.lock().await;
		let state = match docs.get_mut(rel_path) {
			Some(s) => s,
			None => {
				// Frontend is ahead of us (change before open?). Drop silently;
				// the next open call will catch us up.
				return Ok(());
			}
		};
		state.version += 1;
		let version = state.version;
		let uri = self.relative_to_uri(rel_path);
		drop(docs);
		self.apply_change(&uri, version, text).await?;
		self.spawn_pull_diagnostics(rel_path);
		Ok(())
	}

	/// Fire `textDocument/diagnostic` for `rel_path` in a detached
	/// task. Callers don't await — the pull is best-effort and must
	/// not block the originating `didOpen` / `didChange` notification
	/// path. Servers that don't implement pull diagnostics return
	/// `MethodNotFound`, which we silently ignore (push diagnostics
	/// are the fallback for those servers).
	///
	/// The `result_id` round-trip from a previous pull is **not**
	/// threaded through yet; servers that reuse it gain "did anything
	/// change since last pull?" caching, but for our minimal slice
	/// the unconditional full pull keeps the implementation small —
	/// extension slot when latency matters.
	fn spawn_pull_diagnostics(self: &Arc<Self>, rel_path: &str) {
		let server = Arc::clone(self);
		let path = rel_path.to_owned();
		tokio::spawn(async move {
			if let Err(err) = server.pull_diagnostics(&path).await {
				// `MethodNotFound` (-32601) from servers that don't
				// implement pull is the expected fallback path for
				// `rust-analyzer` and any other push-only server;
				// quiet at debug rather than warn-spam every save.
				match &err {
					LspClientError::Rpc(rpc) if rpc.code == -32601 => {
						tracing::debug!(path = %path, "lsp: pull diagnostics unsupported (push-only server)");
					}
					_ => {
						tracing::debug!(path = %path, %err, "lsp: pull diagnostics failed");
					}
				}
			}
		});
	}

	async fn pull_diagnostics(&self, rel_path: &str) -> Result<(), LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		// Hand-shaped params instead of `lt::DocumentDiagnosticParams`:
		// `lsp-types`'s definition serialises `Option::None` as
		// `null`, but tsgo's Go-side protobuf decoder rejects that
		// for the `identifier` and `previousResultId` fields with
		// "null value is not allowed for field". The LSP spec says
		// these are optional and absent ≠ null; we serialise with
		// `skip_serializing_if` to honour that.
		#[derive(serde::Serialize)]
		#[serde(rename_all = "camelCase")]
		struct DiagnosticParams<'a> {
			text_document: &'a lt::TextDocumentIdentifier,
			#[serde(skip_serializing_if = "Option::is_none")]
			identifier: Option<&'a str>,
			#[serde(skip_serializing_if = "Option::is_none")]
			previous_result_id: Option<&'a str>,
		}
		let text_document = lt::TextDocumentIdentifier { uri };
		let params = DiagnosticParams {
			text_document: &text_document,
			identifier: None,
			previous_result_id: None,
		};
		let resp: lt::DocumentDiagnosticReportResult = self.client.request("textDocument/diagnostic", params).await?;
		let items = match resp {
			lt::DocumentDiagnosticReportResult::Report(lt::DocumentDiagnosticReport::Full(full)) => {
				full.full_document_diagnostic_report.items
			}
			// `Unchanged` means the server has nothing new to say
			// since the last pull — we keep whatever the UI is
			// currently showing instead of clobbering it.
			lt::DocumentDiagnosticReportResult::Report(lt::DocumentDiagnosticReport::Unchanged(_)) => {
				return Ok(());
			}
			// Streaming response; we don't wire the partial-result
			// channel today. Servers can fall back to a single
			// `Full` payload, which the branch above handles.
			lt::DocumentDiagnosticReportResult::Partial(_) => {
				return Ok(());
			}
		};
		let diagnostics = items.into_iter().map(translate::diagnostic).collect();
		let _ = self.events.send(LspServerEvent::Diagnostics(mp::LspDiagnosticsEvent {
			path: rel_path.to_owned(),
			diagnostics,
		}));
		Ok(())
	}

	async fn apply_change(&self, uri: &lt::Uri, version: i32, text: String) -> Result<(), LspClientError> {
		// Full-document sync: one content change covering the whole
		// buffer. We don't tell the server about incremental edits
		// because we don't advertise incremental sync in
		// initialize's `TextDocumentSyncClientCapabilities`, so the
		// server expects full bodies regardless of what we'd prefer.
		let params = lt::DidChangeTextDocumentParams {
			text_document: lt::VersionedTextDocumentIdentifier {
				uri: uri.clone(),
				version,
			},
			content_changes: vec![lt::TextDocumentContentChangeEvent {
				range: None,
				range_length: None,
				text,
			}],
		};
		self.client.notify("textDocument/didChange", params).await
	}

	pub async fn close(&self, rel_path: &str) -> Result<(), LspClientError> {
		let removed = self.docs.lock().await.remove(rel_path);
		if removed.is_none() {
			return Ok(());
		}
		let uri = self.relative_to_uri(rel_path);
		let params = lt::DidCloseTextDocumentParams {
			text_document: lt::TextDocumentIdentifier { uri },
		};
		self.client.notify("textDocument/didClose", params).await
	}

	pub async fn hover(&self, rel_path: &str, position: mp::LspPosition) -> Result<Option<mp::LspHover>, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::HoverParams {
			text_document_position_params: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
		};
		let resp: Option<lt::Hover> = self.client.request("textDocument/hover", params).await?;
		Ok(resp.and_then(translate::hover))
	}

	/// Send `textDocument/definition`. Returns `None` if the server
	/// didn't know where the symbol was (common for literals,
	/// whitespace, arbitrary hovers) — not an error, just a
	/// silent-skip signal the UI uses to leave the identifier
	/// un-underlined.
	pub async fn definition(
		&self,
		rel_path: &str,
		position: mp::LspPosition,
	) -> Result<Option<mp::LspLocation>, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::GotoDefinitionParams {
			text_document_position_params: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
			partial_result_params: lt::PartialResultParams::default(),
		};
		let resp: Option<lt::GotoDefinitionResponse> = self.client.request("textDocument/definition", params).await?;
		// The server reports URIs in its own filesystem view
		// (container paths under `HostMount`), so strip against
		// the server root — otherwise every goto-def inside a
		// containerised project would be mis-classified as
		// "external" and surface as a toast.
		Ok(resp.and_then(|r| translate::definition_response(r, self.translator.server_root().as_std_path())))
	}

	pub async fn completion(
		&self,
		rel_path: &str,
		position: mp::LspPosition,
	) -> Result<mp::LspCompletionList, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::CompletionParams {
			text_document_position: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
			partial_result_params: lt::PartialResultParams::default(),
			context: Some(lt::CompletionContext {
				trigger_kind: lt::CompletionTriggerKind::INVOKED,
				trigger_character: None,
			}),
		};
		let resp: Option<lt::CompletionResponse> = self.client.request("textDocument/completion", params).await?;
		match resp {
			Some(r) => Ok(translate::completion_response(r)),
			None => Ok(mp::LspCompletionList {
				is_incomplete: false,
				items: vec![],
			}),
		}
	}

	pub fn language_id(&self) -> &str {
		&self.language_id
	}

	/// Graceful shutdown: send `shutdown` request, then `exit`
	/// notification. Caller should drop the server afterwards; the
	/// child has `kill_on_drop` so even a hung server can't outlive
	/// the broker.
	pub async fn shutdown(&self) {
		// Best-effort; we don't want a hung server to keep the
		// broker from tearing down. A 2s budget is plenty for any
		// sane LSP.
		let shutdown_fut = self
			.client
			.request::<serde_json::Value, serde_json::Value>("shutdown", serde_json::Value::Null);
		let _ = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown_fut).await;
		let _ = self.client.notify("exit", serde_json::Value::Null).await;
		self.client.shutdown().await;
		if let Some(mut child) = self.child.lock().await.take() {
			let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
		}
	}

	fn relative_to_uri(&self, rel_path: &str) -> lt::Uri {
		let abs = self.translator.absolutise(rel_path);
		path_to_file_uri(abs.as_std_path())
	}

	fn uri_to_relative(&self, uri: &lt::Uri) -> Option<String> {
		// `lsp_types::Uri` is a `fluent_uri` newtype and doesn't
		// expose `to_file_path`; `url::Url` does, and the LSP string
		// form is exactly a URL, so parse and delegate. Both crates
		// accept the same file URI syntax.
		let parsed = url::Url::parse(uri.as_str()).ok()?;
		let path = parsed.to_file_path().ok()?;
		self.translator.relativise(&path)
	}
}

/// Locate `bin_name` on disk, preferring an ecosystem-idiomatic
/// location over a bare `$PATH` lookup.
///
/// Returns `None` when nothing is found — caller treats that as
/// `NotAvailable` and surfaces the spec's `install_hint`.
///
/// Platform note: Node's `.bin` entry is a symlink to the real
/// script on *nix (shebang resolved by the kernel) and a `.cmd`
/// wrapper on Windows. We pick the right suffix so
/// `tokio::process::Command` gets a spawn-able path on both. Cargo's
/// `bin/` has no such dance — both targets install a native executable
/// (with `.exe` on Windows, which `which`-style resolution and the
/// explicit check below both handle).
pub fn discover_binary(bin_name: &str, strategy: DiscoveryStrategy, start: &Path) -> Option<PathBuf> {
	match strategy {
		DiscoveryStrategy::NodeModules => {
			let filename = if cfg!(windows) {
				format!("{bin_name}.cmd")
			} else {
				bin_name.to_owned()
			};
			for ancestor in start.ancestors() {
				let candidate = ancestor.join("node_modules").join(".bin").join(&filename);
				if candidate.exists() {
					tracing::debug!(
						bin = bin_name,
						path = %candidate.display(),
						"lsp: resolved via project-local node_modules"
					);
					return Some(candidate);
				}
			}
			match which::which(bin_name) {
				Ok(path) => {
					tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via PATH");
					Some(path)
				}
				Err(_) => None,
			}
		}
		DiscoveryStrategy::CargoHome => {
			// 1. Ask rustup directly. When the tool is a rustup
			//    component that's installed, this returns the real
			//    toolchain binary path (bypassing the shim in
			//    `~/.cargo/bin/`). When the component isn't installed,
			//    `rustup which` exits non-zero and we fall through —
			//    critically, we also want to avoid spawning the raw
			//    shim here because it'd die at startup with
			//    `Unknown binary '<tool>' in official toolchain`,
			//    which the broker would report as "Crashed" instead
			//    of the more useful "install this component" hint.
			if let Some(path) = rustup_which(bin_name) {
				tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via rustup which");
				return Some(path);
			}
			// 2. `cargo install` builds land in `~/.cargo/bin/` as
			//    real executables (not symlinks to rustup). Accept
			//    those; reject anything that looks like a rustup
			//    shim for the reason above.
			if let Some(candidate) = cargo_bin_candidate(bin_name) {
				if candidate.exists() && !is_rustup_shim(&candidate) {
					tracing::debug!(
						bin = bin_name,
						path = %candidate.display(),
						"lsp: resolved via cargo home"
					);
					return Some(candidate);
				}
			}
			// 3. `$PATH` — last resort for package-manager installs
			//    or hand-compiled binaries living outside cargo-home.
			//    Still filter out rustup shims here: `~/.cargo/bin/`
			//    is typically on `$PATH`, so a naive `which` will
			//    happily hand us the same broken shim.
			match which::which(bin_name) {
				Ok(path) if !is_rustup_shim(&path) => {
					tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via PATH");
					Some(path)
				}
				_ => None,
			}
		}
	}
}

/// Resolve the in-container absolute path of `spec`'s binary for a
/// DockerExec broker. Container terminals / LSPs don't inherit a
/// login shell, so `$PATH` only covers what the image itself puts
/// there (moon-base's rustup + fnm + bun prefixes) — a Node-ecosystem
/// binary installed via `bun install` into
/// `node_modules/.bin/<bin>` won't be found by a plain basename
/// invocation.
///
/// For `NodeModules` we mirror the host-side ancestor walk against
/// the filesystem, then translate the resulting host path into the
/// container's view. The walk has to happen against host paths
/// because the actual `exists()` check is done on the host (the
/// bind-mount is the same bytes, so existence is equivalent —
/// we just can't `stat()` container paths directly from here).
///
/// Only the active folder's own `node_modules` + its descendants
/// within the mount are reachable from the container: a hoisted
/// monorepo that stores `node_modules` **above** the active folder
/// lives outside the bind-mount and returns `None`, in which case
/// the broker falls back to the host spawner for that server.
///
/// For `CargoHome` we return the basename — `moon-base` installs
/// `rust-analyzer` via `rustup component add` at
/// `$CARGO_HOME/bin/rust-analyzer`, which the image's `PATH` env
/// covers — and let the container's own resolution do the rest.
pub fn container_binary_path(spec: &LspBinarySpec, translator: &PathTranslator) -> Option<PathBuf> {
	let (host_root, server_root) = match translator {
		PathTranslator::HostMount { host_root, server_root } => (host_root.as_std_path(), server_root.as_std_path()),
		// Callers should only hit this for a DockerExec broker,
		// which always carries a HostMount translator. Keep the
		// guard so a wire-up bug surfaces as None (→ NotAvailable
		// pill) rather than a silent host-path leak.
		PathTranslator::Identity { .. } => return None,
	};

	match spec.discovery {
		DiscoveryStrategy::NodeModules => {
			// Container is always Linux (moon-base is debian) —
			// no `.cmd` suffix dance the host path needs.
			let filename = spec.bin_name;
			for ancestor in host_root.ancestors() {
				let candidate = ancestor.join("node_modules").join(".bin").join(filename);
				if !candidate.exists() {
					continue;
				}
				// Only accept matches that sit inside the bind
				// mount. Hoisted monorepos with `node_modules`
				// at a parent of the active folder aren't
				// reachable from inside the container; the
				// caller then falls back to the host spawner.
				let Ok(rel) = candidate.strip_prefix(host_root) else {
					tracing::debug!(
						bin = spec.bin_name,
						host_path = %candidate.display(),
						host_root = %host_root.display(),
						"lsp: node_modules match sits outside the bind mount, \
						 container can't reach it — falling back to host"
					);
					return None;
				};
				let path = server_root.join(rel);
				tracing::debug!(
					bin = spec.bin_name,
					path = %path.display(),
					"lsp: resolved via container-side node_modules"
				);
				return Some(path);
			}
			tracing::debug!(
				bin = spec.bin_name,
				host_root = %host_root.display(),
				"lsp: no node_modules/.bin/<bin> found below the mount root"
			);
			None
		}
		DiscoveryStrategy::CargoHome => Some(PathBuf::from(spec.bin_name)),
	}
}

/// Probe `rustup which <tool>` and return its reported path. Returns
/// `None` when rustup isn't on PATH, when the requested tool isn't
/// installed in the active toolchain, or any other failure mode —
/// all of which should gracefully surface as "server unavailable"
/// rather than treated as an error.
fn rustup_which(tool: &str) -> Option<PathBuf> {
	let out = std::process::Command::new("rustup")
		.args(["which", tool])
		.output()
		.ok()?;
	if !out.status.success() {
		return None;
	}
	let raw = String::from_utf8(out.stdout).ok()?;
	let trimmed = raw.trim();
	if trimmed.is_empty() {
		return None;
	}
	Some(PathBuf::from(trimmed))
}

/// Is `path` a rustup proxy shim (i.e. `~/.cargo/bin/<tool>` that's
/// really a symlink to the `rustup` binary)? A shim spawns rustup
/// with `argv[0] == <tool>`, which demands that the tool is a known
/// component in the active toolchain — not a contract we can
/// satisfy just because the file exists.
///
/// Only symlinks can be detected cheaply this way. On Windows
/// rustup installs per-tool `.exe` shims that are separate binaries
/// and harder to identify without running them. The `rustup_which`
/// probe above handles the Windows case by giving us the real path
/// before we look in `cargo_bin_candidate` at all, so missing
/// Windows-shim detection here is a small residual risk at most.
fn is_rustup_shim(path: &Path) -> bool {
	match std::fs::read_link(path) {
		Ok(target) => target.file_name() == Some(std::ffi::OsStr::new("rustup")),
		Err(_) => false,
	}
}

/// Resolve `$CARGO_HOME/bin/<bin_name>` with the `$HOME/.cargo/bin/`
/// fallback that rustup uses when `$CARGO_HOME` isn't set. Returns
/// `None` only when we can't build any candidate at all (no
/// `$CARGO_HOME`, no `$HOME` / `$USERPROFILE`) — caller still has
/// the `$PATH` escape hatch after this.
fn cargo_bin_candidate(bin_name: &str) -> Option<PathBuf> {
	let filename = if cfg!(windows) {
		format!("{bin_name}.exe")
	} else {
		bin_name.to_owned()
	};
	let base = std::env::var_os("CARGO_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
		.or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))?;
	Some(base.join("bin").join(filename))
}

fn path_to_file_uri(path: &Path) -> lt::Uri {
	// `url::Url::from_file_path` handles the OS-specific cases
	// (Windows drive letters, percent-escaping) correctly; we then
	// parse the result back into `lsp_types::Uri` which is a newtype
	// around `fluent_uri`. Both libraries accept the same string
	// form, so the round-trip is lossless.
	let url = url::Url::from_file_path(path).unwrap_or_else(|_| {
		tracing::warn!(path = %path.display(), "lsp: failed to build file:// URL, using empty");
		url::Url::parse("file:///").expect("static parse")
	});
	use std::str::FromStr;
	lt::Uri::from_str(url.as_str()).unwrap_or_else(|e| {
		tracing::warn!(path = %path.display(), error = %e, "lsp: failed to parse file URL as URI");
		lt::Uri::from_str("file:///").expect("static parse")
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use std::sync::Mutex;

	/// Tests that mutate `$CARGO_HOME` serialise on this mutex.
	/// `cargo test` runs tests in parallel by default and process
	/// env is global, so without the lock the two
	/// `CARGO_HOME`-mutating tests below race and one sporadically
	/// sees the other's value.
	static CARGO_HOME_LOCK: Mutex<()> = Mutex::new(());

	#[cfg(unix)]
	fn make_executable(path: &Path) {
		use std::os::unix::fs::PermissionsExt;
		let mut perms = fs::metadata(path).unwrap().permissions();
		perms.set_mode(0o755);
		fs::set_permissions(path, perms).unwrap();
	}

	#[cfg(not(unix))]
	fn make_executable(_: &Path) {}

	/// Binary nestled in the start directory itself resolves.
	#[test]
	fn discover_finds_binary_in_same_dir() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "my-lsp.cmd" } else { "my-lsp" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let found = discover_binary("my-lsp", DiscoveryStrategy::NodeModules, tmp.path());
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// Binary in an ancestor directory resolves — mimics pnpm's
	/// hoisted monorepo layout where `node_modules` lives at the
	/// repo root, not the active subdirectory.
	#[test]
	fn discover_walks_up_to_ancestor_node_modules() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "my-lsp.cmd" } else { "my-lsp" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		// Start from a nested subfolder; discovery should walk up
		// to tmp and find the bin there.
		let nested = tmp.path().join("apps").join("web");
		fs::create_dir_all(&nested).unwrap();

		let found = discover_binary("my-lsp", DiscoveryStrategy::NodeModules, &nested);
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// A project-local copy beats whatever happens to be on PATH.
	/// Regression guard: if someone ever "optimises" discovery by
	/// checking PATH first, this test flips red.
	#[test]
	fn discover_prefers_project_local_over_path() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		// Pick a binary every CI box has on PATH (sh/cmd.exe) so the
		// test's "beats PATH" assertion is actually observable.
		let (probe_name, probe_file) = if cfg!(windows) {
			("cmd", "cmd.cmd")
		} else {
			("sh", "sh")
		};
		let local = bin_dir.join(probe_file);
		fs::write(&local, b"#!/bin/sh\n").unwrap();
		make_executable(&local);

		let found =
			discover_binary(probe_name, DiscoveryStrategy::NodeModules, tmp.path()).expect("project-local should resolve");
		assert_eq!(found, local, "project-local copy must win over PATH");
	}

	/// Missing binary returns None rather than erroring — that's the
	/// contract the broker relies on to surface `NotAvailable`.
	#[test]
	fn discover_returns_none_when_missing_everywhere() {
		let tmp = tempfile::tempdir().unwrap();
		let found = discover_binary(
			"definitely-not-a-real-lsp-server-xyzzy",
			DiscoveryStrategy::NodeModules,
			tmp.path(),
		);
		assert!(found.is_none());
	}

	/// Rustup pre-creates a symlink at `$CARGO_HOME/bin/<tool>` for
	/// every known component, regardless of whether the component is
	/// actually installed. Running the shim when the component is
	/// missing fails with `Unknown binary`. Discovery must see
	/// through that and refuse the shim so the broker surfaces the
	/// right install hint.
	///
	/// Unix-only: on Windows rustup uses per-tool `.exe` shims that
	/// aren't cheaply distinguishable without running them. The
	/// `rustup_which` probe handles that case before we get here.
	#[cfg(unix)]
	#[test]
	fn discover_rejects_rustup_shim_in_cargo_home() {
		use std::os::unix::fs::symlink;

		let _guard = CARGO_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("bin");
		fs::create_dir_all(&bin_dir).unwrap();
		// Create a fake `rustup` binary first — it's the symlink
		// target we check for. Contents don't matter for the shim
		// test; we never spawn it here.
		let rustup_path = bin_dir.join("rustup");
		fs::write(&rustup_path, b"#!/bin/sh\nexit 101\n").unwrap();
		make_executable(&rustup_path);
		// Now drop a shim that points at it — exactly what rustup
		// does for known-but-not-installed components.
		let shim = bin_dir.join("phantom-tool");
		symlink("rustup", &shim).unwrap();

		let prev = std::env::var_os("CARGO_HOME");
		// SAFETY: single-threaded env mutation for this test; previous
		// value restored below. See `discover_uses_cargo_home_when_set`
		// for the rationale.
		unsafe {
			std::env::set_var("CARGO_HOME", tmp.path());
		}
		let found = discover_binary("phantom-tool", DiscoveryStrategy::CargoHome, tmp.path());
		// SAFETY: see above.
		unsafe {
			match prev {
				Some(v) => std::env::set_var("CARGO_HOME", v),
				None => std::env::remove_var("CARGO_HOME"),
			}
		}
		assert!(
			found.is_none(),
			"a rustup shim must not be reported as a usable LSP binary"
		);
	}

	/// CargoHome strategy resolves `$CARGO_HOME/bin/<bin>` when set.
	/// Test pattern: mutating process env is a global concern, so we
	/// set the var for just this test's duration (and pick a binary
	/// name nothing else would ever find, so leaked state never
	/// masks a real failure elsewhere).
	#[test]
	fn discover_uses_cargo_home_when_set() {
		let _guard = CARGO_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "fake-ra.exe" } else { "fake-ra" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		// `CARGO_HOME` points at the temp dir; the file under
		// `$CARGO_HOME/bin/` must win over any PATH lookup that
		// might find a real rust-analyzer on the dev's machine.
		//
		// SAFETY: `std::env::set_var` was marked unsafe in Rust
		// 1.90 because multi-threaded writes to the env table can
		// race with libc readers. This test binary is
		// single-threaded for this read/write, and we restore the
		// original value on exit; no other test in this module
		// touches `CARGO_HOME`. Accepted trade-off for the
		// coverage.
		let prev = std::env::var_os("CARGO_HOME");
		// SAFETY: see above.
		unsafe {
			std::env::set_var("CARGO_HOME", tmp.path());
		}
		let found = discover_binary("fake-ra", DiscoveryStrategy::CargoHome, tmp.path());
		// SAFETY: see above.
		unsafe {
			match prev {
				Some(v) => std::env::set_var("CARGO_HOME", v),
				None => std::env::remove_var("CARGO_HOME"),
			}
		}
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// `Identity` translator is a plain join / strip. Round-tripping
	/// a workspace-relative path through absolutise → relativise
	/// must come back identical or the LSP layer silently loses
	/// track of which buffer a diagnostic belongs to.
	#[test]
	fn translator_identity_round_trip() {
		let t = PathTranslator::Identity {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
		};
		let abs = t.absolutise("src/lib/state.svelte.ts");
		assert_eq!(
			abs,
			Utf8PathBuf::from("/home/dev/code/moon-ide/src/lib/state.svelte.ts")
		);
		let rel = t.relativise(abs.as_std_path()).expect("round-trip");
		assert_eq!(rel, "src/lib/state.svelte.ts");
		assert_eq!(t.host_root(), t.server_root());
	}

	/// `HostMount` round-trip: what the server sees is
	/// `/workspace/<basename>/...`, what the tree and the frontend
	/// see is the host absolute path, and the forward-slash
	/// workspace-relative string must be identical across both
	/// views.
	#[test]
	fn translator_host_mount_round_trip() {
		let t = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let abs = t.absolutise("crates/moon-core/src/lsp/server.rs");
		assert_eq!(
			abs,
			Utf8PathBuf::from("/workspace/moon-ide/crates/moon-core/src/lsp/server.rs")
		);
		let rel = t.relativise(abs.as_std_path()).expect("round-trip");
		assert_eq!(rel, "crates/moon-core/src/lsp/server.rs");
		// Callers that need tree state use host_root, not the
		// containerised server_root.
		assert_eq!(t.host_root(), &Utf8PathBuf::from("/home/dev/code/moon-ide"));
		assert_eq!(t.server_root(), &Utf8PathBuf::from("/workspace/moon-ide"));
	}

	/// Diagnostics against files outside the server's view (e.g.
	/// rust-analyzer publishing against `/usr/local/.../core.rs`
	/// inside the container) are dropped silently — the UI has no
	/// buffer for them. Regression guard for the `strip_prefix`
	/// guard in `relativise`.
	#[test]
	fn translator_relativise_rejects_paths_outside_root() {
		let t = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let outside = Path::new("/usr/local/lib/rustlib/src/rust/library/core/src/lib.rs");
		assert!(t.relativise(outside).is_none());
	}

	/// Regression for the original user bug: a container-backed
	/// broker was handing `docker exec` a bare `tsgo` basename,
	/// which isn't on the container's `$PATH`. The fix resolves
	/// the binary to the in-container `node_modules/.bin/<bin>`
	/// path built from the host's ancestor walk.
	#[test]
	fn container_binary_path_resolves_node_modules_inside_mount() {
		let tmp = tempfile::tempdir().unwrap();
		let host_root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let bin_dir = host_root.join("node_modules").join(".bin");
		fs::create_dir_all(bin_dir.as_std_path()).unwrap();
		let bin_path = bin_dir.join("tsgo");
		fs::write(bin_path.as_std_path(), b"#!/usr/bin/env node\n").unwrap();
		make_executable(bin_path.as_std_path());

		let translator = PathTranslator::HostMount {
			host_root: host_root.clone(),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let resolved = container_binary_path(&TS_SERVER, &translator).expect("resolves when node_modules is in the mount");
		assert_eq!(resolved, Path::new("/workspace/moon-ide/node_modules/.bin/tsgo"));
	}

	/// pnpm-hoisted monorepos keep `node_modules` at a parent of
	/// the active folder. That parent isn't bind-mounted, so the
	/// container can't see the binary. `container_binary_path`
	/// must say so cleanly — `None` — rather than handing back a
	/// mount-escaping server path. The broker then cascades to
	/// the host fallback.
	#[test]
	fn container_binary_path_rejects_node_modules_above_mount() {
		let tmp = tempfile::tempdir().unwrap();
		let monorepo = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let bin_dir = monorepo.join("node_modules").join(".bin");
		fs::create_dir_all(bin_dir.as_std_path()).unwrap();
		let bin_path = bin_dir.join("tsgo");
		fs::write(bin_path.as_std_path(), b"#!/usr/bin/env node\n").unwrap();
		make_executable(bin_path.as_std_path());

		let active = monorepo.join("packages").join("app");
		fs::create_dir_all(active.as_std_path()).unwrap();

		let translator = PathTranslator::HostMount {
			host_root: active,
			server_root: Utf8PathBuf::from("/workspace/app"),
		};
		assert!(
			container_binary_path(&TS_SERVER, &translator).is_none(),
			"hoisted node_modules sits above the bind mount — container can't reach it"
		);
	}

	/// `CargoHome`-strategy specs return the basename: `moon-base`
	/// installs `rust-analyzer` on the container's `$PATH` via
	/// rustup, so `docker exec` can resolve it without an
	/// absolute path.
	#[test]
	fn container_binary_path_cargo_home_returns_basename() {
		let translator = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let resolved =
			container_binary_path(&RUST_SERVER, &translator).expect("rust-analyzer always returns Some for CargoHome");
		assert_eq!(resolved, Path::new("rust-analyzer"));
	}
}
