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
use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_protocol::lsp as mp;
use tokio::sync::{broadcast, Mutex};

use super::client::LspClientError;
use super::server::{LspBinarySpec, LspServer, LspServerEvent, TS_SERVER};

pub struct LspBroker {
	root: Utf8PathBuf,
	servers: Mutex<HashMap<String, ServerSlot>>,
	events: broadcast::Sender<LspServerEvent>,
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

/// Subscription handle the Tauri layer uses to listen for server
/// events. One receiver per subscriber; the broker owns the sender.
pub type LspEventRx = broadcast::Receiver<LspServerEvent>;

impl LspBroker {
	pub fn new(root: Utf8PathBuf) -> Arc<Self> {
		// 256 is more than a human can generate in a frame; the TS
		// server publishes a couple per second per dirty file at
		// worst. Overflow drops oldest which is fine — the
		// frontend caches per-path and the next publish replaces
		// whatever was missed.
		let (events, _) = broadcast::channel::<LspServerEvent>(256);
		Arc::new(Self {
			root,
			servers: Mutex::new(HashMap::new()),
			events,
		})
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
		// typescript-language-server handles all four JS/TS flavours
		// through one process; internally tsserver will spawn a
		// second project for JS if needed.
		match language_id {
			"typescript" | "typescriptreact" | "javascript" | "javascriptreact" => Some(&TS_SERVER),
			_ => None,
		}
	}

	/// Ensure a server for `spec` is running and return a cloned
	/// handle. Spawns on first call. On spawn failure (binary
	/// missing, bad args, child exits immediately) caches a
	/// `NotAvailable` slot and returns `Ok(None)` — the caller
	/// treats missing LSP as a feature that just isn't on, not an
	/// error that stops the editor.
	async fn ensure_server(&self, spec: &LspBinarySpec) -> Result<Option<Arc<LspServer>>, LspClientError> {
		{
			let servers = self.servers.lock().await;
			if let Some(slot) = servers.get(spec.language_id) {
				return Ok(match slot {
					ServerSlot::Ready(s) => Some(s.clone()),
					ServerSlot::NotAvailable => None,
				});
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

		match LspServer::spawn(spec, self.root.clone(), self.events.clone()).await {
			Ok(Some(server)) => {
				self
					.servers
					.lock()
					.await
					.insert(spec.language_id.to_owned(), ServerSlot::Ready(server.clone()));
				Ok(Some(server))
			}
			Ok(None) => {
				self
					.servers
					.lock()
					.await
					.insert(spec.language_id.to_owned(), ServerSlot::NotAvailable);
				let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
					language_id: spec.language_id.to_owned(),
					status: mp::LspServerStatus::NotAvailable,
					// Surface the install command directly in the pill tooltip
					// so the user has copy-pasteable next steps without
					// leaving the IDE.
					detail: Some(spec.install_hint.to_owned()),
				}));
				Ok(None)
			}
			Err(e) => {
				tracing::warn!(error = %e, lang = spec.language_id, "lsp: spawn failed");
				self
					.servers
					.lock()
					.await
					.insert(spec.language_id.to_owned(), ServerSlot::NotAvailable);
				let _ = self.events.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
					language_id: spec.language_id.to_owned(),
					status: mp::LspServerStatus::Crashed,
					detail: Some(e.to_string()),
				}));
				Err(e)
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
