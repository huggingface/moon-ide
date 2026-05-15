//! User-facing model picks for the coder.
//!
//! Knobs persisted in [`AppState.coder`][app]:
//!
//! - **Standard model** — drives the main agent loop and every
//!   sub-agent. Pinned to a tool-capable slug because the loop relies
//!   on `tool_calls`.
//! - **Cheap model** — drives the helper round-trips that don't
//!   need tools and shouldn't burn premium tokens: auto-rename
//!   session titles, branch-name suggester, compaction summary,
//!   folder-summary onboarding.
//! - **`bill_to`** — value sent as `X-HF-Bill-To` on every HF
//!   inference request. `None` (the default) bills the user's
//!   personal account; anything else routes the cost to the named
//!   org. **HF-only** — suppressed off the wire when a user
//!   provider is active.
//! - **`providers` / `active_provider`** — user-added
//!   OpenAI-compatible endpoints (OpenRouter, locally-hosted
//!   vLLM / Ollama / …). When `active_provider` is `Some(id)`, the
//!   runner reads the picks off the matching entry instead of the
//!   HF fields; the inference client routes requests to that
//!   endpoint's `base_url` with a `Bearer <api_key>` header drawn
//!   from the [`ProviderKeyring`].
//!
//! The struct is held behind [`SharedCoderModels`] so the Tauri layer
//! can hot-swap a fresh snapshot when the user touches the settings
//! popover. The runner re-reads at the start of every chat-completions
//! call site, so a flip mid-turn just changes which model takes the
//! *next* round-trip — no abort.
//!
//! [app]: ../../../crates/moon-protocol/src/app_state.rs
//! [`ProviderKeyring`]: crate::providers::ProviderKeyring

use std::collections::HashMap;
use std::sync::Arc;

use moon_protocol::coder_models::{CoderProviderConfig, ProviderModelSummary, RouterModel};
use tokio::sync::RwLock;

use crate::defaults::{context_window_for, DEFAULT_CHEAP_MODEL, DEFAULT_STANDARD_MODEL};

#[derive(Debug, Clone)]
pub struct CoderModels {
	/// HF-tier standard slug (optionally `:provider` /
	/// `:fastest` / `:cheapest` / `:preferred` suffixed). Empty
	/// string → fall back to [`DEFAULT_STANDARD_MODEL`]. Read only
	/// when [`active_provider`](Self::active_provider) is `None`;
	/// user providers carry their own picks in their
	/// [`CoderProviderConfig`].
	pub standard: String,
	/// Same shape, for cheap helper calls. Empty →
	/// [`DEFAULT_CHEAP_MODEL`]. Same HF-only semantics as
	/// [`standard`](Self::standard).
	pub cheap: String,
	/// Organization name for `X-HF-Bill-To`. `None` bills the user's
	/// personal account; `Some(org_name)` routes the cost to the org
	/// (the user must be a paying member with permission, otherwise
	/// the router rejects the request). Suppressed when a user
	/// provider is active.
	pub bill_to: Option<String>,
	/// User-added providers, mirrored from
	/// [`moon_protocol::app_state::CoderAppState::providers`]. Each
	/// entry carries its own `standard` / `cheap` picks; the
	/// [`InferenceClient`] resolves the route off the active one
	/// per request.
	///
	/// [`InferenceClient`]: crate::inference::InferenceClient
	pub providers: Vec<CoderProviderConfig>,
	/// Id of the active provider, or `None` for the implicit HF
	/// route. Falls back to HF if the id doesn't match any entry
	/// in [`providers`](Self::providers) — handles the "user
	/// deleted the entry out of band" race.
	pub active_provider: Option<String>,
	/// Model-id → context-length cache distilled from every
	/// `/v1/models` catalog the picker has fetched in this
	/// process. Populated as a side-effect of
	/// [`crate::runner::CoderHandle::list_models`] (HF) and
	/// [`crate::runner::CoderHandle::list_provider_models`] (user
	/// providers), and primed in the background by
	/// [`crate::runner::CoderHandle::prime_context_windows`] on
	/// startup / active-provider change so the very first turn
	/// after a relaunch already has authoritative numbers.
	/// Read by [`Self::context_window`] on every LLM round-trip
	/// to size the usage ring and arm auto-compaction.
	///
	/// Catalogs from different routes are **merged** rather than
	/// replaced — a fetch from OpenRouter mustn't blow away the
	/// HF entries the user might still flip back to.
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
			providers: Vec::new(),
			active_provider: None,
			context_windows: Arc::new(HashMap::new()),
		}
	}
}

/// Resolved request routing for one round-trip. Computed off
/// [`CoderModels`] on the inference side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedProvider {
	/// Implicit HF route. The inference client uses
	/// [`crate::defaults::HF_ROUTER_BASE`] as the base URL, the
	/// `Authenticator`'s OAuth bearer (with refresh on 401), and
	/// sends `X-HF-Bill-To` when set.
	HuggingFace,
	/// User-added OpenAI-compatible endpoint. The client uses
	/// `base_url` verbatim, sends `Authorization: Bearer
	/// <api_key>` when the keyring has an entry, and omits the
	/// bill-to header. The `id` is the keyring lookup key the
	/// inference client uses to fetch the api key per request.
	Custom { id: String, base_url: String },
}

impl CoderModels {
	/// Active slug for the everyday driver. Reads from the
	/// matching [`CoderProviderConfig`] when a user provider is
	/// active, falling through to the HF [`standard`] when there
	/// isn't one (or the active id doesn't resolve, e.g. the
	/// entry was deleted out of band).
	///
	/// [`standard`]: Self::standard
	pub fn standard(&self) -> &str {
		match self.active_provider_entry() {
			Some(p) if !p.standard_model.is_empty() => p.standard_model.as_str(),
			Some(_) => DEFAULT_STANDARD_MODEL,
			None => {
				if self.standard.is_empty() {
					DEFAULT_STANDARD_MODEL
				} else {
					self.standard.as_str()
				}
			}
		}
	}

	/// Active cheap slug. Same fallback rules as
	/// [`standard`](Self::standard).
	pub fn cheap(&self) -> &str {
		match self.active_provider_entry() {
			Some(p) if !p.cheap_model.is_empty() => p.cheap_model.as_str(),
			Some(_) => DEFAULT_CHEAP_MODEL,
			None => {
				if self.cheap.is_empty() {
					DEFAULT_CHEAP_MODEL
				} else {
					self.cheap.as_str()
				}
			}
		}
	}

	/// `X-HF-Bill-To` value or `None`. Treats an empty string the
	/// same as `None` so the frontend can wire a single text field
	/// without a separate "is set" flag. Returns `None`
	/// unconditionally when a user provider is active — the
	/// header is HF-specific and we don't leak it cross-host.
	pub fn bill_to(&self) -> Option<&str> {
		if self.active_provider_entry().is_some() {
			return None;
		}
		match self.bill_to.as_deref() {
			Some(s) if !s.is_empty() => Some(s),
			_ => None,
		}
	}

	/// Resolve which `(base_url, auth scheme, bill_to)` shape the
	/// next request uses. Falls back to HF when
	/// [`active_provider`](Self::active_provider) is `None` *or*
	/// points at a deleted entry.
	pub fn resolve_route(&self) -> ResolvedProvider {
		match self.active_provider_entry() {
			Some(entry) => ResolvedProvider::Custom {
				id: entry.id.clone(),
				base_url: entry.base_url.clone(),
			},
			None => ResolvedProvider::HuggingFace,
		}
	}

	/// Currently active provider entry, or `None` for HF / orphan
	/// id. Logs at `warn` when [`active_provider`](Self::active_provider)
	/// points at an id that isn't in [`providers`](Self::providers)
	/// — happens when the entry was deleted on a separate launch
	/// and the persisted `AppState` survived; we transparently
	/// fall back to HF in that case.
	fn active_provider_entry(&self) -> Option<&CoderProviderConfig> {
		let id = self.active_provider.as_ref()?;
		let found = self.providers.iter().find(|p| p.id == *id);
		if found.is_none() {
			tracing::warn!(
				active_provider = %id,
				"active provider id has no matching entry; falling back to HF"
			);
		}
		found
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
	/// opened once (HF-only — user providers don't populate the
	/// catalog).
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

/// Same as [`context_windows_from_catalog`] but for a flat user-
/// provider catalog (OpenRouter, LiteLLM, raw vLLM, …). The
/// runner side merges this into [`CoderModels::context_windows`]
/// alongside the HF entries — that way flipping the active
/// provider in the picker doesn't blow the cache away.
pub fn context_windows_from_provider_catalog(catalog: &[ProviderModelSummary]) -> HashMap<String, u32> {
	let mut out = HashMap::new();
	for m in catalog {
		if let Some(w) = m.context_length {
			out.insert(m.id.clone(), w);
		}
	}
	out
}

/// Merge `incoming` slug→window pairs into `base`, returning a
/// fresh `Arc<HashMap>`. New keys win on collision; pre-existing
/// keys not present in `incoming` are preserved. Used by every
/// catalog-fetch site so a route flip in the picker doesn't
/// erase the previous route's windows.
pub fn merge_context_windows(base: &HashMap<String, u32>, incoming: HashMap<String, u32>) -> Arc<HashMap<String, u32>> {
	if base.is_empty() {
		return Arc::new(incoming);
	}
	let mut merged = base.clone();
	for (k, v) in incoming {
		merged.insert(k, v);
	}
	Arc::new(merged)
}

/// Process-wide shared handle. Constructed once at coder startup,
/// updated by the Tauri layer's `coder_set_models` command, read by
/// the runner at every chat-completions call site (snapshot-clone,
/// no awaited read on the hot path beyond the one at turn-start).
pub type SharedCoderModels = Arc<RwLock<CoderModels>>;

pub fn shared(models: CoderModels) -> SharedCoderModels {
	Arc::new(RwLock::new(models))
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::coder_models::ProviderModelSummary;

	fn provider_summary(id: &str, ctx: Option<u32>) -> ProviderModelSummary {
		ProviderModelSummary {
			id: id.to_owned(),
			owned_by: None,
			name: None,
			context_length: ctx,
			pricing_in_per_million: None,
			pricing_out_per_million: None,
			description: None,
		}
	}

	#[test]
	fn provider_catalog_skips_models_without_context_length() {
		let catalog = vec![
			provider_summary("anthropic/claude-opus-4", Some(1_000_000)),
			provider_summary("openai/gpt-4o-mini", None),
		];
		let map = context_windows_from_provider_catalog(&catalog);
		assert_eq!(map.get("anthropic/claude-opus-4"), Some(&1_000_000));
		assert!(!map.contains_key("openai/gpt-4o-mini"));
	}

	#[test]
	fn merge_preserves_old_entries_and_overwrites_collisions() {
		let mut base = HashMap::new();
		base.insert("Qwen/Qwen3.5-397B-A17B".to_owned(), 256_000u32);
		base.insert("anthropic/claude-opus-4".to_owned(), 200_000u32);
		let mut incoming = HashMap::new();
		incoming.insert("anthropic/claude-opus-4".to_owned(), 1_000_000u32);
		incoming.insert("openai/gpt-4.1".to_owned(), 1_000_000u32);

		let merged = merge_context_windows(&base, incoming);

		assert_eq!(merged.get("Qwen/Qwen3.5-397B-A17B"), Some(&256_000));
		assert_eq!(merged.get("anthropic/claude-opus-4"), Some(&1_000_000));
		assert_eq!(merged.get("openai/gpt-4.1"), Some(&1_000_000));
	}

	#[test]
	fn context_window_lookup_consults_cache_then_strips_provider_suffix_then_static_table() {
		let mut models = CoderModels::default();
		let mut cache = HashMap::new();
		cache.insert("anthropic/claude-opus-4".to_owned(), 1_000_000u32);
		models.context_windows = Arc::new(cache);

		assert_eq!(models.context_window("anthropic/claude-opus-4"), 1_000_000);
		assert_eq!(models.context_window("anthropic/claude-opus-4:fastest"), 1_000_000);
		assert_eq!(models.context_window("Qwen/Qwen3.5-397B-A17B"), 256_000);
	}
}
