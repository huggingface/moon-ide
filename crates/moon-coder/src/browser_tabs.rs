//! Registry of the IDE's browser tabs (ADR 0088).
//!
//! Lives in the coder so `list_browser_tabs` / `browser_tab` can read
//! and drive it directly; the Tauri layer shares the same instance for
//! the user's own actions (URL bar, reload, close) and pumps
//! [`BrowserTabsChanged`] to the frontend, which only mirrors it.
//! Process-lifetime, never persisted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use moon_protocol::browser::{BrowserPageRequest, BrowserTab, BrowserTabsChanged};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

const CHANGE_CHANNEL_CAPACITY: usize = 64;

pub struct BrowserTabRegistry {
	inner: Mutex<Inner>,
	changes: broadcast::Sender<BrowserTabsChanged>,
	page_requests: broadcast::Sender<BrowserPageRequest>,
	pending: Mutex<HashMap<u32, oneshot::Sender<Result<Value, String>>>>,
}

#[derive(Default)]
struct Inner {
	next_id: u32,
	next_request: u32,
	tabs: Vec<BrowserTab>,
}

impl Default for BrowserTabRegistry {
	fn default() -> Self {
		Self::new()
	}
}

impl BrowserTabRegistry {
	pub fn new() -> Self {
		let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
		let (page_requests, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
		Self {
			inner: Mutex::new(Inner {
				next_id: 1,
				next_request: 1,
				tabs: Vec::new(),
			}),
			changes,
			page_requests,
			pending: Mutex::new(HashMap::new()),
		}
	}

	pub fn subscribe_page_requests(&self) -> broadcast::Receiver<BrowserPageRequest> {
		self.page_requests.subscribe()
	}

	/// Ask tab `tab_id`'s page bridge to run `op` and wait for its
	/// answer. Errors (as a message for the model) when the tab is
	/// unknown, no IDE window is listening, the page doesn't answer
	/// within `timeout`, or the bridge reports a failure.
	pub async fn page_request(&self, tab_id: u32, op: &str, args: Value, timeout: Duration) -> Result<Value, String> {
		if self.get(tab_id).is_none() {
			return Err(format!(
				"no browser tab {tab_id}; call `list_browser_tabs` for the open ones"
			));
		}
		let (tx, rx) = oneshot::channel();
		let request_id = {
			let mut inner = self.lock();
			let id = inner.next_request;
			inner.next_request = inner.next_request.wrapping_add(1).max(1);
			id
		};
		self.lock_pending().insert(request_id, tx);
		let sent = self.page_requests.send(BrowserPageRequest {
			request_id,
			tab_id,
			op: op.to_owned(),
			args,
		});
		if sent.is_err() {
			self.lock_pending().remove(&request_id);
			return Err("no IDE window is showing browser tabs".to_owned());
		}
		let answer = tokio::time::timeout(timeout, rx).await;
		self.lock_pending().remove(&request_id);
		match answer {
			Ok(Ok(result)) => result,
			Ok(Err(_)) => Err("the page request was dropped".to_owned()),
			Err(_) => Err(format!(
				"the page didn't answer within {} s — the bridge only runs in http pages served through the IDE (not https), and only once the page has loaded",
				timeout.as_secs()
			)),
		}
	}

	/// Deliver the frontend's answer to a pending `page_request`.
	/// Unknown ids (timed out, duplicate) are ignored.
	pub fn respond(&self, request_id: u32, result: Result<Value, String>) {
		if let Some(tx) = self.lock_pending().remove(&request_id) {
			let _ = tx.send(result);
		}
	}

	/// Record where in-page navigation took tab `id`, without the
	/// focus/reload a `navigate` implies.
	pub fn set_location(&self, id: u32, url: &str) {
		let mut inner = self.lock();
		let Some(tab) = inner.tabs.iter_mut().find(|tab| tab.id == id) else {
			return;
		};
		if tab.url == url {
			return;
		}
		tab.url = url.to_owned();
		self.emit(&inner, None, None);
	}

	fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<u32, oneshot::Sender<Result<Value, String>>>> {
		self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	pub fn subscribe(&self) -> broadcast::Receiver<BrowserTabsChanged> {
		self.changes.subscribe()
	}

	pub fn list(&self) -> Vec<BrowserTab> {
		self.lock().tabs.clone()
	}

	pub fn get(&self, id: u32) -> Option<BrowserTab> {
		self.lock().tabs.iter().find(|tab| tab.id == id).cloned()
	}

	/// Open a tab on `url`, focused. With `reuse`, an existing tab on
	/// the same URL and side is focused and reloaded instead; the bool
	/// reports whether that happened.
	pub fn open(&self, url: &str, in_container: bool, reuse: bool) -> (BrowserTab, bool) {
		let mut inner = self.lock();
		if reuse {
			if let Some(existing) = inner
				.tabs
				.iter()
				.find(|tab| tab.url == url && tab.in_container == in_container)
				.cloned()
			{
				self.emit(&inner, Some(existing.id), Some(existing.id));
				return (existing, true);
			}
		}
		let tab = BrowserTab {
			id: inner.next_id,
			url: url.to_owned(),
			in_container,
		};
		inner.next_id += 1;
		inner.tabs.push(tab.clone());
		self.emit(&inner, Some(tab.id), None);
		(tab, false)
	}

	/// Point tab `id` at `url` (focused, fresh load). `in_container`
	/// of `None` keeps the tab's current side.
	pub fn navigate(&self, id: u32, url: &str, in_container: Option<bool>) -> Option<BrowserTab> {
		let mut inner = self.lock();
		let tab = inner.tabs.iter_mut().find(|tab| tab.id == id)?;
		tab.url = url.to_owned();
		if let Some(in_container) = in_container {
			tab.in_container = in_container;
		}
		let tab = tab.clone();
		self.emit(&inner, Some(id), Some(id));
		Some(tab)
	}

	pub fn reload(&self, id: u32) -> Option<BrowserTab> {
		let inner = self.lock();
		let tab = inner.tabs.iter().find(|tab| tab.id == id).cloned()?;
		self.emit(&inner, None, Some(id));
		Some(tab)
	}

	pub fn focus(&self, id: u32) -> Option<BrowserTab> {
		let inner = self.lock();
		let tab = inner.tabs.iter().find(|tab| tab.id == id).cloned()?;
		self.emit(&inner, Some(id), None);
		Some(tab)
	}

	pub fn close(&self, id: u32) -> Option<BrowserTab> {
		let mut inner = self.lock();
		let idx = inner.tabs.iter().position(|tab| tab.id == id)?;
		let tab = inner.tabs.remove(idx);
		self.emit(&inner, None, None);
		Some(tab)
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
		self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	fn emit(&self, inner: &Inner, focus: Option<u32>, reload: Option<u32>) {
		// No subscriber (headless / tests) is fine: the registry is
		// still the truth `list_browser_tabs` reads.
		let _ = self.changes.send(BrowserTabsChanged {
			tabs: inner.tabs.clone(),
			focus,
			reload,
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn open_reuse_navigate_close() {
		let registry = BrowserTabRegistry::new();
		let mut rx = registry.subscribe();
		let (first, reused) = registry.open("http://localhost:5173/", true, true);
		assert!(!reused);
		assert_eq!(rx.try_recv().unwrap().focus, Some(first.id));

		let (again, reused) = registry.open("http://localhost:5173/", true, true);
		assert!(reused);
		assert_eq!(again.id, first.id);
		assert_eq!(rx.try_recv().unwrap().reload, Some(first.id));

		let (other, _) = registry.open("http://localhost:5173/", false, true);
		assert_ne!(other.id, first.id);

		let moved = registry
			.navigate(first.id, "http://localhost:5173/about", None)
			.unwrap();
		assert!(moved.in_container);
		assert_eq!(registry.get(first.id).unwrap().url, "http://localhost:5173/about");

		registry.set_location(first.id, "http://localhost:5173/after-click");
		assert_eq!(registry.get(first.id).unwrap().url, "http://localhost:5173/after-click");

		assert!(registry.close(first.id).is_some());
		assert!(registry.close(first.id).is_none());
		assert_eq!(registry.list(), vec![other]);
	}

	#[tokio::test]
	async fn page_request_round_trip() {
		let registry = std::sync::Arc::new(BrowserTabRegistry::new());
		let (tab, _) = registry.open("http://localhost:3000/", false, false);
		let err = registry
			.page_request(tab.id, "snapshot", Value::Null, Duration::from_secs(1))
			.await
			.unwrap_err();
		assert!(err.contains("no IDE window"));

		let mut requests = registry.subscribe_page_requests();
		let responder = registry.clone();
		tokio::spawn(async move {
			let req = requests.recv().await.unwrap();
			responder.respond(req.request_id, Ok(serde_json::json!({ "op": req.op })));
		});
		let answer = registry
			.page_request(tab.id, "snapshot", Value::Null, Duration::from_secs(5))
			.await
			.unwrap();
		assert_eq!(answer["op"], "snapshot");
		assert!(registry
			.page_request(99, "snapshot", Value::Null, Duration::from_secs(1))
			.await
			.is_err());
	}
}
