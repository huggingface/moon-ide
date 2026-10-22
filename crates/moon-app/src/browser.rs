//! Commands for the IDE's browser tabs (ADR 0088).
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
use tokio::sync::broadcast::error::RecvError;
use url::{Host, Url};

use std::sync::Arc;

use crate::events::EventSink;
use crate::AppCore;

/// Resolve `url` to something the webview can load: the host/port
/// are replaced with a `127.0.0.1:<ephemeral>` preview proxy that
/// reaches the original host/port as seen from the host
/// (`in_container = false`) or from inside the workspace shell —
/// loopback, compose service names, anything its DNS resolves.
/// Loopback URLs fall back to the other side when nothing answers on
/// the first. The proxy injects the page bridge the coder's `browser_page` tool
/// drives. `https` URLs aren't proxied (the proxy speaks plain HTTP):
/// host ones load directly without the bridge, container ones are
/// refused.
pub async fn browser_resolve_url(app: &AppCore, url: String, in_container: bool) -> Result<String, MoonError> {
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
	let container = match app.workspace_id().map(project_name_for_id) {
		Some(Ok(project)) => Some(PreviewTarget::Container(dev_container_name(&project))),
		Some(Err(err)) if in_container => return Err(MoonError::invalid(err.to_string())),
		Some(Err(_)) => None,
		None if in_container => return Err(MoonError::invalid("no workspace bound to this process")),
		None => None,
	};
	// A loopback URL is ambiguous across the container boundary (an
	// agent's `bash` side needn't be where the server runs), so the
	// other side is the fallback. Other hosts only make sense on the
	// side that named them.
	let loopback = is_loopback(&dial_host);
	let sides: Vec<PreviewTarget> = if in_container {
		container
			.into_iter()
			.chain(loopback.then_some(PreviewTarget::Host))
			.collect()
	} else {
		std::iter::once(PreviewTarget::Host)
			.chain(container.filter(|_| loopback))
			.collect()
	};
	let local_port = app
		.preview_tunnels
		.ensure(sides, &dial_host, port)
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

fn is_loopback(host: &str) -> bool {
	host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Payload: `BrowserTabsChanged`. Mirrored in `src/lib/app.svelte.ts`.
pub const BROWSER_TABS_EVENT: &str = "browser:tabs";

/// Payload: `BrowserPageRequest`. Handled in `src/lib/browserBridge.ts`.
pub const BROWSER_PAGE_REQUEST_EVENT: &str = "browser:page_request";

/// Re-broadcast registry changes (from agents or the commands below)
/// onto the event sink for the process lifetime.
pub fn spawn_tabs_pump(events: Arc<dyn EventSink>, coder: &CoderHandle) {
	let mut requests = coder.browser_tabs().subscribe_page_requests();
	let request_events = events.clone();
	tokio::spawn(async move {
		loop {
			match requests.recv().await {
				Ok(request) => {
					crate::events::emit(request_events.as_ref(), BROWSER_PAGE_REQUEST_EVENT, &request);
				}
				// A dropped request just times out on the coder side.
				Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "browser page request pump lagged"),
				Err(RecvError::Closed) => break,
			}
		}
	});
	let mut rx = coder.browser_tabs().subscribe();
	tokio::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(change) => {
					crate::events::emit(events.as_ref(), BROWSER_TABS_EVENT, &change);
				}
				// Each payload is the full tab set, so the next one
				// resyncs whatever was missed.
				Err(RecvError::Lagged(n)) => tracing::warn!(missed = n, "browser tabs pump lagged"),
				Err(RecvError::Closed) => break,
			}
		}
	});
}

pub async fn browser_tabs_list(app: &AppCore) -> Result<Vec<BrowserTab>, MoonError> {
	Ok(app.coder.browser_tabs().list())
}

/// Open (or, with `reuse`, focus + reload a tab already on) `url`.
pub async fn browser_tab_open(
	app: &AppCore,
	url: String,
	in_container: bool,
	reuse: bool,
) -> Result<BrowserTab, MoonError> {
	Ok(app.coder.browser_tabs().open(&url, in_container, None, reuse).0)
}

pub async fn browser_tab_navigate(app: &AppCore, id: u32, url: String) -> Result<BrowserTab, MoonError> {
	app
		.coder
		.browser_tabs()
		.navigate(id, &url, None)
		.ok_or_else(|| MoonError::invalid(format!("no browser tab {id}")))
}

pub async fn browser_tab_reload(app: &AppCore, id: u32) {
	app.coder.browser_tabs().reload(id);
}

pub async fn browser_tab_focus(app: &AppCore, id: u32) {
	app.coder.browser_tabs().focus(id);
}

/// Idempotent: closing an already-gone tab is a no-op.
pub async fn browser_tab_close(app: &AppCore, id: u32) {
	app.coder.browser_tabs().close(id);
}

/// In-page navigation report from the tab's bridge (already mapped
/// back to the original origin by the frontend).
pub async fn browser_tab_location(app: &AppCore, id: u32, url: String) {
	app.coder.browser_tabs().set_location(id, &url);
}

/// Answer a `browser:page_request`: `error` set means the page action
/// failed, otherwise `value` is its result.
pub async fn browser_page_respond(
	app: &AppCore,
	request_id: u32,
	value: Option<serde_json::Value>,
	error: Option<String>,
) {
	let result = match error {
		Some(error) => Err(error),
		None => Ok(value.unwrap_or(serde_json::Value::Null)),
	};
	app.coder.browser_tabs().respond(request_id, result);
}

crate::commands! {
	browser_resolve_url([app] url: String, in_container: bool),
	browser_tabs_list([app]),
	browser_tab_open([app] url: String, in_container: bool, reuse: bool),
	browser_tab_navigate([app] id: u32, url: String),
	browser_tab_reload([app] id: u32),
	browser_tab_focus([app] id: u32),
	browser_tab_close([app] id: u32),
	browser_tab_location([app] id: u32, url: String),
	browser_page_respond([app] request_id: u32, value: Option<serde_json::Value>, error: Option<String>),
}
