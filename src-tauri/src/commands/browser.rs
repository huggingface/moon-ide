//! Tauri commands for the IDE's browser tabs (ADR 0088).
//!
//! The tab set lives in the coder's `BrowserTabRegistry` so agents
//! can list and drive it; these commands are the user's side of the
//! same registry, and the frontend mirrors it from `browser:tabs`.
//! The tab itself is an iframe; `browser_resolve_url` turns a URL
//! that's meaningful *inside* the workspace shell into one the host
//! webview can load, via a loopback tunnel dialing from inside the
//! container.

use moon_coder::CoderHandle;
use moon_container::{dev_container_name, project_name_for_id, PreviewTarget};
use moon_protocol::browser::BrowserTab;
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::broadcast::error::RecvError;
use url::{Host, Url};

use crate::state::AppState;

/// Resolve `url` to something the webview can load: the host/port
/// are replaced with a `127.0.0.1:<ephemeral>` preview proxy that
/// reaches the original host/port as seen from the host
/// (`in_container = false`) or from inside the workspace shell —
/// loopback, compose service names, anything its DNS resolves. The
/// proxy injects the page bridge the coder's `browser_page` tool
/// drives. `https` URLs aren't proxied (the proxy speaks plain HTTP):
/// host ones load directly without the bridge, container ones are
/// refused.
#[tauri::command]
pub async fn browser_resolve_url(
	state: State<'_, AppState>,
	url: String,
	in_container: bool,
) -> Result<String, MoonError> {
	let mut parsed = Url::parse(url.trim()).map_err(|err| MoonError::invalid(format!("invalid url: {err}")))?;
	match (parsed.scheme(), in_container) {
		("http", _) => {}
		("https", false) => return Ok(parsed.into()),
		("https", true) => {
			return Err(MoonError::invalid(
				"https URLs inside the container aren't supported; use the dev server's http port",
			));
		}
		_ => return Err(MoonError::invalid("only http/https URLs can be opened")),
	}
	let dial_host = match parsed.host() {
		Some(Host::Ipv4(ip)) if ip.is_unspecified() => "localhost".to_owned(),
		Some(Host::Ipv6(ip)) if ip.is_unspecified() => "localhost".to_owned(),
		Some(Host::Ipv6(ip)) => ip.to_string(),
		Some(host) => host.to_string(),
		None => return Err(MoonError::invalid("url has no host")),
	};
	let Some(port) = parsed.port_or_known_default() else {
		return Err(MoonError::invalid("url has no port"));
	};
	let target = if in_container {
		let Some(id) = state.workspace_id() else {
			return Err(MoonError::invalid("no workspace bound to this process"));
		};
		let project = project_name_for_id(id).map_err(|err| MoonError::invalid(err.to_string()))?;
		PreviewTarget::Container(dev_container_name(&project))
	} else {
		PreviewTarget::Host
	};
	let local_port = state
		.preview_tunnels
		.ensure(target, &dial_host, port)
		.await
		.map_err(MoonError::io)?;
	parsed
		.set_host(Some("127.0.0.1"))
		.map_err(|err| MoonError::internal(err.to_string()))?;
	parsed
		.set_port(Some(local_port))
		.map_err(|()| MoonError::internal("could not set proxy port"))?;
	Ok(parsed.into())
}

/// Payload: `BrowserTabsChanged`. Mirrored in `src/lib/state.svelte.ts`.
pub const BROWSER_TABS_EVENT: &str = "browser:tabs";

/// Payload: `BrowserPageRequest`. Handled in `src/lib/browserBridge.ts`.
pub const BROWSER_PAGE_REQUEST_EVENT: &str = "browser:page_request";

/// Re-broadcast registry changes (from agents or the commands below)
/// onto the Tauri event bus for the process lifetime.
pub fn spawn_tabs_pump(app: AppHandle, coder: &CoderHandle) {
	let mut requests = coder.browser_tabs().subscribe_page_requests();
	let request_app = app.clone();
	tauri::async_runtime::spawn(async move {
		loop {
			match requests.recv().await {
				Ok(request) => {
					if let Err(err) = request_app.emit(BROWSER_PAGE_REQUEST_EVENT, &request) {
						tracing::warn!(error = %err, "failed to emit browser page request");
					}
				}
				// A dropped request just times out on the coder side.
				Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "browser page request pump lagged"),
				Err(RecvError::Closed) => break,
			}
		}
	});
	let mut rx = coder.browser_tabs().subscribe();
	tauri::async_runtime::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(change) => {
					if let Err(err) = app.emit(BROWSER_TABS_EVENT, &change) {
						tracing::warn!(error = %err, "failed to emit browser tabs event");
					}
				}
				// Each payload is the full tab set, so the next one
				// resyncs whatever was missed.
				Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "browser tabs pump lagged"),
				Err(RecvError::Closed) => break,
			}
		}
	});
}

#[tauri::command]
pub fn browser_tabs_list(state: State<'_, AppState>) -> Vec<BrowserTab> {
	state.coder.browser_tabs().list()
}

/// Open (or, with `reuse`, focus + reload a tab already on) `url`.
#[tauri::command]
pub fn browser_tab_open(state: State<'_, AppState>, url: String, in_container: bool, reuse: bool) -> BrowserTab {
	state.coder.browser_tabs().open(&url, in_container, reuse).0
}

#[tauri::command]
pub fn browser_tab_navigate(state: State<'_, AppState>, id: u32, url: String) -> Result<BrowserTab, MoonError> {
	state
		.coder
		.browser_tabs()
		.navigate(id, &url, None)
		.ok_or_else(|| MoonError::invalid(format!("no browser tab {id}")))
}

#[tauri::command]
pub fn browser_tab_reload(state: State<'_, AppState>, id: u32) {
	state.coder.browser_tabs().reload(id);
}

#[tauri::command]
pub fn browser_tab_focus(state: State<'_, AppState>, id: u32) {
	state.coder.browser_tabs().focus(id);
}

/// Idempotent: closing an already-gone tab is a no-op.
#[tauri::command]
pub fn browser_tab_close(state: State<'_, AppState>, id: u32) {
	state.coder.browser_tabs().close(id);
}

/// In-page navigation report from the tab's bridge (already mapped
/// back to the original origin by the frontend).
#[tauri::command]
pub fn browser_tab_location(state: State<'_, AppState>, id: u32, url: String) {
	state.coder.browser_tabs().set_location(id, &url);
}

/// Answer a `browser:page_request`: `error` set means the page action
/// failed, otherwise `value` is its result.
#[tauri::command]
pub fn browser_page_respond(
	state: State<'_, AppState>,
	request_id: u32,
	value: Option<serde_json::Value>,
	error: Option<String>,
) {
	let result = match error {
		Some(error) => Err(error),
		None => Ok(value.unwrap_or(serde_json::Value::Null)),
	};
	state.coder.browser_tabs().respond(request_id, result);
}
