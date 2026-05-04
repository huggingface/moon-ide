//! Per-language LSP server actor.
//!
//! One [`LspServer`] owns one child process (e.g.
//! `typescript-language-server --stdio`), an [`LspClient`] on top of
//! its stdio, and a map of open documents. Its public surface is
//! narrow: `open` / `update` / `close` / `hover` / `completion`.
//! Nothing here knows about multiple languages — the broker picks the
//! right server for a language id.
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
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use camino::Utf8PathBuf;
use lsp_types as lt;
use moon_protocol::lsp as mp;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex};

use super::client::{LspClient, LspClientError, ServerNotification};
use super::translate;

/// What the broker hears when something interesting happens in a
/// server — feeds through to the Tauri event layer.
#[derive(Debug, Clone)]
pub enum LspServerEvent {
	Diagnostics(mp::LspDiagnosticsEvent),
	StatusChanged(mp::LspStatusEvent),
}

/// Which binary to spawn for a given LSP language. One entry per
/// language id we intend to support. For stage 1 only `typescript`
/// is populated.
pub struct LspBinarySpec {
	pub language_id: &'static str,
	pub bin_name: &'static str,
	pub args: &'static [&'static str],
}

pub const TS_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "typescript",
	bin_name: "typescript-language-server",
	args: &["--stdio"],
};

pub struct LspServer {
	language_id: String,
	client: LspClient,
	child: Mutex<Option<Child>>,
	// Workspace-relative to file://URI mapping.
	root: Utf8PathBuf,
	// Per-document version counter. LSP requires monotonically
	// increasing versions per didChange to detect out-of-order
	// updates; we start at 1 on open and tick each change.
	docs: Mutex<HashMap<String, DocState>>,
}

struct DocState {
	version: i32,
}

impl LspServer {
	/// Locate and spawn the server binary. Returns `Ok(None)` when
	/// the binary is not on PATH — caller surfaces a
	/// `NotAvailable` status instead of treating this as an error.
	pub async fn spawn(
		spec: &LspBinarySpec,
		root: Utf8PathBuf,
		events: broadcast::Sender<LspServerEvent>,
	) -> Result<Option<Arc<Self>>, LspClientError> {
		let Some(resolved) = which::which(spec.bin_name).ok() else {
			tracing::info!(
				bin = spec.bin_name,
				lang = spec.language_id,
				"lsp: binary not on PATH, server unavailable"
			);
			return Ok(None);
		};

		let mut child = Command::new(&resolved)
			.args(spec.args)
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
			root,
			docs: Mutex::new(HashMap::new()),
		});

		// Notification pump: translate the ones we care about,
		// drop the rest. Currently: `textDocument/publishDiagnostics`.
		// Extension slot for `window/showMessage` etc. when we
		// actually surface them.
		let server_ref = server.clone();
		let events_sink = events.clone();
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
				detail: Some(resolved.display().to_string()),
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
				..Default::default()
			}),
			..Default::default()
		};

		let root_uri = path_to_file_uri(self.root.as_std_path());

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
				name: self.root.file_name().unwrap_or("workspace").to_owned(),
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
	pub async fn open(&self, rel_path: &str, text: String, language_id: &str) -> Result<(), LspClientError> {
		let mut docs = self.docs.lock().await;
		let uri = self.relative_to_uri(rel_path);
		if let Some(state) = docs.get_mut(rel_path) {
			state.version += 1;
			let version = state.version;
			drop(docs);
			return self.apply_change(&uri, version, text).await;
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
		self.client.notify("textDocument/didOpen", params).await
	}

	pub async fn update(&self, rel_path: &str, text: String) -> Result<(), LspClientError> {
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
		self.apply_change(&uri, version, text).await
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
		let abs = self.root.join(rel_path);
		path_to_file_uri(abs.as_std_path())
	}

	fn uri_to_relative(&self, uri: &lt::Uri) -> Option<String> {
		// `lsp_types::Uri` is a `fluent_uri` newtype and doesn't
		// expose `to_file_path`; `url::Url` does, and the LSP string
		// form is exactly a URL, so parse and delegate. Both crates
		// accept the same file URI syntax.
		let parsed = url::Url::parse(uri.as_str()).ok()?;
		let path = parsed.to_file_path().ok()?;
		let rel = path.strip_prefix(self.root.as_std_path()).ok()?;
		// Normalise to forward slashes: moon-ide's workspace
		// paths use `/` on every OS so the frontend doesn't have
		// to branch on `std::path::MAIN_SEPARATOR`.
		let s = rel.to_string_lossy().replace('\\', "/");
		Some(s)
	}
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
