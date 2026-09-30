//! In-IDE browser tab shapes (ADR 0088).
//!
//! The backend registry is the source of truth for which browser
//! tabs exist, so the coder can list and drive them; the frontend
//! mirrors it from [`BrowserTabsChanged`].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One open browser tab. `url` is what was last asked for (by the
/// user's URL bar or an agent), not wherever in-page navigation has
/// since taken the iframe — that's cross-origin and invisible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BrowserTab {
	pub id: u32,
	pub url: String,
	/// Resolve the URL inside the workspace shell container (through
	/// a loopback tunnel) rather than on the host.
	pub in_container: bool,
}

/// Payload of the `browser:tabs` event: the full tab set after a
/// change, plus which tab (if any) should be brought to front and
/// which should be reloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BrowserTabsChanged {
	pub tabs: Vec<BrowserTab>,
	pub focus: Option<u32>,
	pub reload: Option<u32>,
}

/// Payload of the `browser:page_request` event: the coder wants the
/// page bridge in tab `tab_id` to run `op` (ADR 0088). The frontend
/// relays it into the tab's iframe and answers with
/// `browser_page_respond(request_id, …)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BrowserPageRequest {
	pub request_id: u32,
	pub tab_id: u32,
	pub op: String,
	#[ts(type = "unknown")]
	pub args: serde_json::Value,
}
