//! The workspace process's command surface, independent of how it is
//! reached (ADR 0095). The desktop serves it in-process through one
//! Tauri command; a remote workspace serves the same dispatcher over
//! SSH. Desktop-shell actions (windows, dialogs, opener, clipboard)
//! are deliberately not here: they always run on the user's desktop.

use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_core::WorkspaceRegistry;
use moon_protocol::MoonError;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub mod attach;
pub mod boot;
pub mod browser;
mod catalog;
pub mod coder;
pub mod compose_logs;
pub mod container;
pub mod container_events;
mod editorconfig;
mod events;
mod fs;
pub mod fs_watcher;
mod lifecycle;
pub mod logs;
pub mod lsp;
pub mod ports;
pub mod project_compose;
mod search;
mod session;
pub mod settings;
mod shell_resolver;
pub mod terminal;
mod workspace;

pub use catalog::bump_last_active;
pub use events::{emit, EventSink};
pub use fs::refuse_switching_owned_worktree;

/// State every dispatched command runs against. Grows as command
/// groups move out of `src-tauri` (Phase 16.2).
pub struct AppCore {
	pub workspaces: Arc<WorkspaceRegistry>,
	/// Machine-local app state (`state.json`: catalog, providers).
	pub config_dir: Utf8PathBuf,
	/// Root of the per-workspace state directories.
	pub workspaces_dir: Utf8PathBuf,
	/// Workspace this process owns; `None` in preboot mode.
	pub workspace_id: Option<String>,
	pub coder: moon_coder::CoderHandle,
	pub events: Arc<dyn EventSink>,
	/// Loopback tunnels backing browser tabs (ADR 0088).
	pub preview_tunnels: moon_container::PreviewTunnels,
	/// Diagnostic log sink behind the bottom-panel logs view.
	pub logs: Arc<moon_core::LogSink>,
	/// What's open in the terminal tabs plus each one's retained
	/// output; shared with the coder's terminal tools (ADR 0048).
	pub terminals: Arc<moon_terminal::TerminalRegistry>,
	pub terminal_streams: terminal::TerminalStreams,
	/// LSP broker for the active folder plus its event pump; built
	/// lazily on the first `lsp_*` call.
	pub lsp: tokio::sync::Mutex<Option<lsp::LspHandle>>,
	/// Open `docker compose logs -f` tails behind the log tabs.
	pub log_streams: compose_logs::LogStreams,
	/// Watches the active folder and emits `fs:changed`; retargeted
	/// by the folder commands.
	pub fs_watcher: fs_watcher::FsWatcherHandle,
}

impl AppCore {
	pub fn workspace_id(&self) -> Option<&str> {
		self.workspace_id.as_deref()
	}

	/// Let agents open, restart and close terminals through the
	/// registry they share with the terminal commands (ADR 0090).
	pub fn install_agent_terminals(self: &Arc<Self>) {
		self.terminals.set_spawner(Arc::new(terminal::AgentTerminalSpawner {
			app: Arc::downgrade(self),
		}));
	}

	/// Start the tasks that forward in-process broadcasts (coder loop,
	/// browser tabs, logs) to the event sink. Must run on the tokio
	/// runtime.
	pub fn spawn_pumps(&self) {
		logs::spawn_event_pump(self.events.clone(), self.logs.clone());
		coder::spawn_event_pump(self.events.clone(), self.coder.clone());
		browser::spawn_tabs_pump(self.events.clone(), &self.coder);
	}

	/// Run `method` with its JSON `params` (an object keyed by the
	/// camelCase argument names, same shape Tauri's `invoke` takes).
	pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, MoonError> {
		macro_rules! route {
			($($group:ident),*) => {
				$(if $group::handles(method) {
					return $group::dispatch(self, method, params).await;
				})*
			};
		}
		route!(
			fs,
			search,
			editorconfig,
			session,
			coder,
			browser,
			logs,
			terminal,
			lsp,
			container,
			project_compose,
			compose_logs,
			ports,
			workspace
		);
		Err(MoonError::invalid(format!("unknown method `{method}`")))
	}
}

fn parse_params<T: DeserializeOwned>(method: &str, params: Value) -> Result<T, MoonError> {
	let params = if params.is_null() {
		Value::Object(serde_json::Map::new())
	} else {
		params
	};
	serde_json::from_value(params).map_err(|e| MoonError::invalid(format!("{method}: bad params: {e}")))
}

/// What a command may return: a serialisable `Result`, or nothing.
trait IntoRpc {
	fn into_rpc(self) -> Result<Value, MoonError>;
}

impl<T: serde::Serialize> IntoRpc for Result<T, MoonError> {
	fn into_rpc(self) -> Result<Value, MoonError> {
		serde_json::to_value(self?).map_err(|e| MoonError::internal(e.to_string()))
	}
}

impl IntoRpc for () {
	fn into_rpc(self) -> Result<Value, MoonError> {
		Ok(Value::Null)
	}
}

/// Generates a module's `handles` + `dispatch`: one arm per command, deserialising
/// the camelCase params object into the listed arguments. `[app]`
/// before the arguments passes the [`AppCore`] through.
macro_rules! commands {
	($( $name:ident ( $([$app:ident])? $( $arg:ident : $ty:ty ),* ) ),* $(,)?) => {
		pub(crate) fn handles(method: &str) -> bool {
			matches!(method, $( stringify!($name) )|*)
		}

		pub(crate) async fn dispatch(
			app: &crate::AppCore,
			method: &str,
			params: serde_json::Value,
		) -> Result<serde_json::Value, moon_protocol::MoonError> {
			match method {
				$( stringify!($name) => {
					#[derive(serde::Deserialize)]
					#[serde(rename_all = "camelCase")]
					#[allow(dead_code)]
					struct Args { $( $arg: $ty ),* }
					#[allow(unused_variables)]
					let Args { $( $arg ),* } = crate::parse_params::<Args>(method, params)?;
					crate::IntoRpc::into_rpc($name($(crate::pass_app!($app, app),)? $( $arg ),*).await)
				} )*
				_ => Err(moon_protocol::MoonError::invalid(format!("unknown method `{method}`"))),
			}
		}
	};
}
pub(crate) use commands;

/// Emits the dispatcher's own `app` binding for a command marked
/// `[app]` (macro hygiene keeps the caller's spelling from naming it).
macro_rules! pass_app {
	($marker:ident, $app:expr) => {
		$app
	};
}
pub(crate) use pass_app;

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use serde_json::json;

	struct NoEvents;

	impl EventSink for NoEvents {
		fn emit(&self, _event: &str, _payload: Value) {}
	}

	fn app() -> (AppCore, tempfile::TempDir) {
		app_with_events(Arc::new(NoEvents))
	}

	pub(crate) fn app_with_events(events: Arc<dyn EventSink>) -> (AppCore, tempfile::TempDir) {
		// `CoderHandle::new` warms the OS keyring; CI has no Secret Service.
		keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
		let data = tempfile::tempdir().unwrap();
		let dir = |name: &str| Utf8PathBuf::from_path_buf(data.path().join(name)).unwrap();
		let workspaces = Arc::new(WorkspaceRegistry::new("test-workspace".into()));
		let terminals = Arc::new(moon_terminal::TerminalRegistry::default());
		let coder = moon_coder::CoderHandle::new(
			workspaces.clone(),
			dir("workspaces"),
			dir("coder-sessions"),
			dir("folder-summaries"),
			moon_coder::CoderModels::default(),
			terminals.clone(),
		)
		.unwrap();
		let app = AppCore {
			workspaces,
			config_dir: dir("config"),
			workspaces_dir: dir("workspaces"),
			workspace_id: Some("test-workspace".into()),
			coder,
			fs_watcher: fs_watcher::spawn(events.clone()),
			events,
			preview_tunnels: moon_container::PreviewTunnels::new(),
			logs: moon_core::LogSink::new(),
			terminals,
			terminal_streams: Default::default(),
			lsp: Default::default(),
			log_streams: Default::default(),
		};
		(app, data)
	}

	#[tokio::test]
	async fn dispatches_camel_case_params_to_the_command() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a.txt");
		let path = path.to_str().unwrap();
		let (app, _data) = app();
		app
			.dispatch("fs_write_file_host", json!({ "path": path, "text": "hi" }))
			.await
			.unwrap();
		let read = app
			.dispatch("fs_read_file_host", json!({ "path": path }))
			.await
			.unwrap();
		assert_eq!(read["text"], "hi");
	}

	#[tokio::test]
	async fn rejects_unknown_methods_and_bad_params() {
		let (app, _data) = app();
		let unknown = app.dispatch("nope", Value::Null).await.unwrap_err();
		assert!(unknown.to_string().contains("unknown method `nope`"), "{unknown}");
		let bad = app.dispatch("fs_read_file_host", json!({})).await.unwrap_err();
		assert!(bad.to_string().contains("fs_read_file_host: bad params"), "{bad}");
	}
}
