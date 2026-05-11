//! User-facing model picks for the coder.
//!
//! Three knobs, all per-user, persisted in [`AppState.coder`][app]:
//!
//! - **Standard model** — drives the main agent loop and every
//!   sub-agent. Pinned to a tool-capable slug because the loop relies
//!   on `tool_calls`.
//! - **Cheap model** — drives the helper round-trips that don't
//!   need tools and shouldn't burn premium tokens: auto-rename
//!   session titles, branch-name suggester, compaction summary,
//!   folder-summary onboarding.
//! - **`bill_to`** — value sent as `X-HF-Bill-To` on every inference
//!   request. `None` (the default) bills the user's personal account;
//!   anything else routes the cost to the named org.
//!
//! The struct is held behind [`SharedCoderModels`] so the Tauri layer
//! can hot-swap a fresh snapshot when the user touches the settings
//! popover. The runner re-reads at the start of every chat-completions
//! call site, so a flip mid-turn just changes which model takes the
//! *next* round-trip — no abort.
//!
//! [app]: ../../../crates/moon-protocol/src/app_state.rs

use std::collections::HashMap;
use std::sync::Arc;

use moon_protocol::coder_models::RouterModel;
use tokio::sync::RwLock;

use crate::defaults::{context_window_for, DEFAULT_CHEAP_MODEL, DEFAULT_STANDARD_MODEL};

#[derive(Debug, Clone)]
pub struct CoderModels {
	/// Slug (optionally `:provider` / `:fastest` / `:cheapest` /
	/// `:preferred` suffixed) for the everyday driver. Empty string
	/// → fall back to [`DEFAULT_STANDARD_MODEL`].
	pub standard: String,
	/// Same shape, for cheap helper calls. Empty → [`DEFAULT_CHEAP_MODEL`].
	pub cheap: String,
	/// Organization name for `X-HF-Bill-To`. `None` bills the user's
	/// personal account; `Some(org_name)` routes the cost to the org
	/// (the user must be a paying member with permission, otherwise
	/// the router rejects the request).
	pub bill_to: Option<String>,
	/// Model-id → context-length cache distilled from the router's
	/// `/v1/models` response. Populated as a side-effect of
	/// [`crate::runner::CoderHandle::list_models`] (which the picker
	/// hits on open), read by [`Self::context_window`] on every
	/// LLM round-trip to size the usage ring and arm auto-compaction.
	///
	/// Value is the **max** over `providers[].context_length` for
	/// the model — most providers serve the same window, but a few
	/// truncate; the runner gives the model the benefit of the doubt
	/// since the router is what enforces the cap. `Arc` keeps cloning
	/// the whole [`CoderModels`] for snapshot reads a pointer copy
	/// regardless of catalog size (~1k entries).
	pub context_windows: Arc<HashMap<String, u32>>,
}

impl Default for CoderModels {
	fn default() -> Self {
		Self {
			standard: DEFAULT_STANDARD_MODEL.to_string(),
			cheap: DEFAULT_CHEAP_MODEL.to_string(),
			bill_to: None,
			context_windows: Arc::new(HashMap::new()),
		}
	}
}

impl CoderModels {
	/// Standard slug with empty-string fallback. The caller treats
	/// the return as `&str` and feeds it straight into the
	/// inference request.
	pub fn standard(&self) -> &str {
		if self.standard.is_empty() {
			DEFAULT_STANDARD_MODEL
		} else {
			self.standard.as_str()
		}
	}

	/// Cheap slug with empty-string fallback.
	pub fn cheap(&self) -> &str {
		if self.cheap.is_empty() {
			DEFAULT_CHEAP_MODEL
		} else {
			self.cheap.as_str()
		}
	}

	/// `X-HF-Bill-To` value or `None`. Treats an empty string the same
	/// as `None` so the frontend can wire a single text field
	/// without a separate "is set" flag.
	pub fn bill_to(&self) -> Option<&str> {
		match self.bill_to.as_deref() {
			Some(s) if !s.is_empty() => Some(s),
			_ => None,
		}
	}

	/// Best-effort context-window lookup for `slug`. Tries the
	/// router-derived cache first (with and without the
	/// `:provider` suffix the user may have pinned), then falls
	/// back to the static table in
	/// [`crate::defaults::context_window_for`].
	///
	/// Always returns a non-zero number: the static fallback's
	/// 128k default makes the usage ring and the compaction
	/// threshold render sensibly even for slugs we've never seen
	/// — at the cost of being wrong if the slug is a 1M-window
	/// model. The cache fills in as soon as the picker has been
	/// opened once.
	pub fn context_window(&self, slug: &str) -> u32 {
		if let Some(&w) = self.context_windows.get(slug) {
			return w;
		}
		let base = strip_provider_suffix(slug);
		if base != slug {
			if let Some(&w) = self.context_windows.get(base) {
				return w;
			}
		}
		context_window_for(slug)
	}
}

/// Drop the `:provider` / `:fastest` / `:cheapest` / `:preferred`
/// tail from a model slug, returning the bare `owner/name` form
/// the router catalog keys are stored under.
fn strip_provider_suffix(slug: &str) -> &str {
	match slug.find(':') {
		Some(idx) => &slug[..idx],
		None => slug,
	}
}

/// Distill a `Vec<RouterModel>` from `/v1/models` into the
/// slug→context-length map [`CoderModels::context_windows`] holds.
/// Provider variants of the same model are collapsed by taking
/// the max — see [`CoderModels::context_window`] for the rationale.
pub fn context_windows_from_catalog(catalog: &[RouterModel]) -> HashMap<String, u32> {
	let mut out = HashMap::with_capacity(catalog.len());
	for m in catalog {
		let max = m.providers.iter().filter_map(|p| p.context_length).max();
		if let Some(w) = max {
			out.insert(m.id.clone(), w);
		}
	}
	out
}

/// Process-wide shared handle. Constructed once at coder startup,
/// updated by the Tauri layer's `coder_set_models` command, read by
/// the runner at every chat-completions call site (snapshot-clone,
/// no awaited read on the hot path beyond the one at turn-start).
pub type SharedCoderModels = Arc<RwLock<CoderModels>>;

pub fn shared(models: CoderModels) -> SharedCoderModels {
	Arc::new(RwLock::new(models))
}
