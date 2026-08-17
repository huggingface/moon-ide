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

use moon_protocol::coder_models::{CoderProviderConfig, ProviderKind, ProviderModelSummary, RouterModel};
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
	/// Model-id → image-input-support cache, populated from the
	/// same catalog fetches as [`context_windows`](Self::context_windows)
	/// (HF: `architecture.input_modalities`; OpenRouter: same
	/// field; Anthropic: hardcoded `true`). Absent slug = unknown
	/// — the wire encoder treats that as "supports images" because
	/// wrongly stripping pixels from a capable model is worse than
	/// an explainable provider error.
	pub vision: Arc<HashMap<String, bool>>,
	/// User-set per-slug context-window caps. Mirrors
	/// [`moon_protocol::app_state::CoderAppState::context_window_overrides`]
	/// at runtime. [`Self::context_window`] applies the cap with
	/// `min(catalog, override)` so the usage ring and the
	/// auto-compaction threshold both respect "this model is
	/// better at 250k even though it advertises 1M".
	pub context_window_overrides: Arc<HashMap<String, u32>>,
	/// User-authored rate-limit fallback chain (full wire slugs, any
	/// models/flavors, tried in order). Deliberately explicit
	/// rather than auto-derived from the router catalog's sibling
	/// flavors: the user knows which providers they trust with a
	/// given task (context window, caching, quality all differ).
	/// Empty = no rotation.
	pub rotation: Arc<Vec<String>>,
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
			vision: Arc::new(HashMap::new()),
			context_window_overrides: Arc::new(HashMap::new()),
			rotation: Arc::new(Vec::new()),
		}
	}
}

/// Resolved request routing for one round-trip. Computed off
/// [`CoderModels`] on the inference side.
///
/// `Custom` and `OpenRouter` share a wire path (OpenAI-compat
/// `/chat/completions`); they're separate variants so the runner
/// can attach the right `cache_control` markers without sniffing
/// the base URL. `Anthropic` is a different beast — see
/// [`crate::anthropic`] for the translator the inference client
/// branches into when this variant is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedProvider {
	/// Implicit HF route. The inference client uses
	/// [`crate::defaults::HF_ROUTER_BASE`] as the base URL, the
	/// `Authenticator`'s OAuth bearer (with refresh on 401), and
	/// sends `X-HF-Bill-To` when set.
	HuggingFace,
	/// Free-form OpenAI-compatible endpoint. The client uses
	/// `base_url` verbatim, sends `Authorization: Bearer
	/// <api_key>` when the keyring has an entry, and omits the
	/// bill-to header. The `id` is the keyring lookup key the
	/// inference client uses to fetch the api key per request.
	Custom { id: String, base_url: String },
	/// OpenRouter built-in. Wire path is identical to
	/// [`Self::Custom`]; the variant exists so prompt-cache
	/// markers fire on `anthropic/*` slugs without consulting
	/// the base URL.
	OpenRouter { id: String, base_url: String },
	/// Anthropic native (`/v1/messages`). Auth via `x-api-key`,
	/// system prompt as a top-level field, native `cache_control`
	/// blocks, separate streaming SSE event grammar.
	Anthropic { id: String, base_url: String },
}

impl ResolvedProvider {
	/// Keyring lookup id for this route, or `None` for the
	/// implicit HF route (which has no keyring entry — the
	/// `Authenticator` owns the bearer there).
	pub fn provider_id(&self) -> Option<&str> {
		match self {
			Self::HuggingFace => None,
			Self::Custom { id, .. } | Self::OpenRouter { id, .. } | Self::Anthropic { id, .. } => Some(id.as_str()),
		}
	}

	/// Endpoint root for this route, or `None` for HF (the
	/// inference client hardcodes `HF_ROUTER_BASE` there).
	pub fn base_url(&self) -> Option<&str> {
		match self {
			Self::HuggingFace => None,
			Self::Custom { base_url, .. } | Self::OpenRouter { base_url, .. } | Self::Anthropic { base_url, .. } => {
				Some(base_url.as_str())
			}
		}
	}

	/// Human-readable provider name we stamp on persisted
	/// pi-mono assistant messages — `huggingface`, `anthropic`,
	/// `openrouter`, or the user-provider's own id for the
	/// generic [`Self::Custom`] case (e.g. a self-hosted vLLM
	/// pinned under id `local-vllm`).
	pub fn pi_provider_name(&self) -> &str {
		match self {
			Self::HuggingFace => "huggingface",
			Self::Anthropic { .. } => "anthropic",
			Self::OpenRouter { .. } => "openrouter",
			Self::Custom { id, .. } => id.as_str(),
		}
	}

	/// `provider/model`-shaped slug suitable for
	/// [`crate::sessions::SessionRecord::Assistant::model`].
	/// Combines [`Self::pi_provider_name`] with `model_slug`
	/// using a `/` separator. The pi-mono trace viewer splits
	/// on that boundary to render `provider · model` in its
	/// per-message header — see [`crate::sessions::pi_assistant_message`].
	pub fn pi_provider_model(&self, model_slug: &str) -> String {
		format!("{}/{}", self.pi_provider_name(), model_slug)
	}
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

	/// Active cheap slug.
	///
	/// On a user provider with no `cheap_model` set, fall back to
	/// the same provider's `standard_model`. The previous fallback
	/// (the HF `DEFAULT_CHEAP_MODEL` slug) was wrong for any
	/// non-HF route — non-HF hosts simply don't carry that slug —
	/// so a session running on OpenRouter / Anthropic with the
	/// cheap field left blank would 404 on the first auto-rename.
	/// Falling through to standard keeps the cheap call sites
	/// (auto-rename, branch-name suggester, compaction summary)
	/// working without forcing the user to pick two slugs in the
	/// modal.
	pub fn cheap(&self) -> &str {
		match self.active_provider_entry() {
			Some(p) if !p.cheap_model.is_empty() => p.cheap_model.as_str(),
			Some(p) if !p.standard_model.is_empty() => p.standard_model.as_str(),
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
			Some(entry) => match entry.kind {
				ProviderKind::Anthropic => ResolvedProvider::Anthropic {
					id: entry.id.clone(),
					base_url: entry.base_url.clone(),
				},
				ProviderKind::OpenRouter => ResolvedProvider::OpenRouter {
					id: entry.id.clone(),
					base_url: entry.base_url.clone(),
				},
				ProviderKind::Custom => ResolvedProvider::Custom {
					id: entry.id.clone(),
					base_url: entry.base_url.clone(),
				},
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
	///
	/// A user-set override from
	/// [`Self::context_window_overrides`] is **authoritative** — it
	/// can raise past the catalog as well as cap below it. Routers
	/// routinely under-advertise (128k listed for models that ship
	/// 1M), and the old `min(catalog, override)` made a wrong
	/// catalog value impossible to correct from the UI. Lookup
	/// tries the full slug then the suffix-stripped base — same
	/// precedence as the catalog lookup, so an override entered
	/// against `Qwen/...:scaleway` applies to that slug only while
	/// one on the bare id applies to every `:provider` flavour.
	/// Values strictly below `1` collapse to no override
	/// (defensive against the frontend persisting a `0` from a
	/// cleared input).
	/// Fallback slugs to try when `wire_model`'s rate-limit backoff
	/// is exhausted: the user's rotation list, minus the model that
	/// just failed. When the failed model is itself in the list the
	/// order starts *after* it and wraps — chain semantics, so a
	/// list of `[a, b, c]` under pressure on `b` tries `c` then
	/// `a`. Empty list = no rotation.
	pub fn rotation_candidates(&self, wire_model: &str) -> Vec<String> {
		let list = &self.rotation;
		if list.is_empty() {
			return Vec::new();
		}
		let start = list.iter().position(|m| m == wire_model).map_or(0, |i| i + 1);
		(0..list.len())
			.map(|k| &list[(start + k) % list.len()])
			.filter(|m| m.as_str() != wire_model)
			.cloned()
			.collect()
	}

	pub fn context_window(&self, slug: &str) -> u32 {
		match self.cap_for(slug).filter(|c| *c > 0) {
			Some(c) => c,
			None => self.catalog_context_window(slug),
		}
	}

	fn catalog_context_window(&self, slug: &str) -> u32 {
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

	/// User-set context-window cap for `slug`, or `None` when
	/// none is set. Same lookup discipline as the catalog: full
	/// slug first, then `:provider` suffix stripped, so
	/// model-wide caps apply to every routed flavour.
	pub fn cap_for(&self, slug: &str) -> Option<u32> {
		if let Some(&c) = self.context_window_overrides.get(slug) {
			return Some(c);
		}
		let base = strip_provider_suffix(slug);
		if base != slug {
			if let Some(&c) = self.context_window_overrides.get(base) {
				return Some(c);
			}
		}
		None
	}

	/// Whether `slug` accepts image input, per the catalog-derived
	/// [`vision`](Self::vision) map. `None` = unknown (catalog not
	/// primed, or the provider doesn't advertise modalities);
	/// callers that have to decide treat unknown as "yes". Same
	/// full-slug-then-suffix-stripped lookup as
	/// [`context_window`](Self::context_window).
	pub fn supports_images(&self, slug: &str) -> Option<bool> {
		if let Some(&v) = self.vision.get(slug) {
			return Some(v);
		}
		let base = strip_provider_suffix(slug);
		if base != slug {
			if let Some(&v) = self.vision.get(base) {
				return Some(v);
			}
		}
		None
	}

	/// [`supports_images`](Self::supports_images) for the active
	/// standard model, short-circuiting the Anthropic route to
	/// `Some(true)` — every current Claude is vision-capable and
	/// Anthropic's catalog doesn't advertise modalities, so the
	/// map alone would report unknown there.
	pub fn standard_supports_images(&self) -> Option<bool> {
		if matches!(self.resolve_route(), ResolvedProvider::Anthropic { .. }) {
			return Some(true);
		}
		self.supports_images(self.standard())
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
	merge_slug_map(base, incoming)
}

/// Distill a catalog's image-input flags into the slug→vision map
/// [`CoderModels::vision`] holds. Slugs whose catalog entry didn't
/// advertise modalities are left out — absent means unknown, and
/// unknown must stay distinguishable from `false`.
pub fn vision_from_catalog(catalog: &[RouterModel]) -> HashMap<String, bool> {
	let mut out = HashMap::with_capacity(catalog.len());
	for m in catalog {
		if let Some(v) = m.supports_image_input {
			out.insert(m.id.clone(), v);
		}
	}
	out
}

/// Same as [`vision_from_catalog`] but for a flat user-provider
/// catalog (OpenRouter, Anthropic, …).
pub fn vision_from_provider_catalog(catalog: &[ProviderModelSummary]) -> HashMap<String, bool> {
	let mut out = HashMap::new();
	for m in catalog {
		if let Some(v) = m.supports_image_input {
			out.insert(m.id.clone(), v);
		}
	}
	out
}

/// Merge `incoming` slug→vision pairs into `base` — same semantics
/// as [`merge_context_windows`].
pub fn merge_vision(base: &HashMap<String, bool>, incoming: HashMap<String, bool>) -> Arc<HashMap<String, bool>> {
	merge_slug_map(base, incoming)
}

fn merge_slug_map<V: Clone>(base: &HashMap<String, V>, incoming: HashMap<String, V>) -> Arc<HashMap<String, V>> {
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
			supports_image_input: None,
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
	fn supports_images_lookup_strips_provider_suffix_and_reports_unknown() {
		let mut models = CoderModels::default();
		let mut cache = HashMap::new();
		cache.insert("deepseek-ai/DeepSeek-V3".to_owned(), false);
		cache.insert("Qwen/Qwen3-VL".to_owned(), true);
		models.vision = Arc::new(cache);

		assert_eq!(models.supports_images("deepseek-ai/DeepSeek-V3"), Some(false));
		// `:provider`-suffixed slug falls back to the bare id —
		// vision is model-level on the router.
		assert_eq!(models.supports_images("deepseek-ai/DeepSeek-V3:novita"), Some(false));
		assert_eq!(models.supports_images("Qwen/Qwen3-VL:fastest"), Some(true));
		// Not in the catalog → unknown, not false.
		assert_eq!(models.supports_images("mystery/model"), None);
	}

	#[test]
	fn vision_from_provider_catalog_keeps_unknown_out_of_the_map() {
		let mut vision_model = provider_summary("qwen2.5-vl", None);
		vision_model.supports_image_input = Some(true);
		let mut text_model = provider_summary("deepseek-chat", None);
		text_model.supports_image_input = Some(false);
		let unknown = provider_summary("llama3.2", None);

		let map = vision_from_provider_catalog(&[vision_model, text_model, unknown]);
		assert_eq!(map.get("qwen2.5-vl"), Some(&true));
		assert_eq!(map.get("deepseek-chat"), Some(&false));
		assert!(!map.contains_key("llama3.2"));
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

	#[test]
	fn user_cap_clamps_catalog_window_and_falls_back_when_no_cap() {
		let mut models = CoderModels::default();
		let mut cache = HashMap::new();
		cache.insert("anthropic/claude-opus-4".to_owned(), 1_000_000u32);
		models.context_windows = Arc::new(cache);
		let mut caps = HashMap::new();
		caps.insert("anthropic/claude-opus-4".to_owned(), 250_000u32);
		models.context_window_overrides = Arc::new(caps);

		assert_eq!(models.context_window("anthropic/claude-opus-4"), 250_000);
		// Provider-suffixed slug: bare-id cap applies through the
		// suffix-strip fallback.
		assert_eq!(models.context_window("anthropic/claude-opus-4:fastest"), 250_000);
		// Different model: no cap, no clamp.
		assert_eq!(models.context_window("Qwen/Qwen3.5-397B-A17B"), 256_000);
	}

	#[test]
	fn rotation_candidates_follow_the_user_list() {
		let models = CoderModels {
			rotation: Arc::new(vec![
				"moonshotai/Kimi-K3:baseten".to_owned(),
				"moonshotai/Kimi-K3:together".to_owned(),
				"deepseek-ai/DeepSeek-V4-Pro-0813:fireworks-ai".to_owned(),
			]),
			..CoderModels::default()
		};

		// Failed model in the list: start after it, wrap, exclude it.
		assert_eq!(
			models.rotation_candidates("moonshotai/Kimi-K3:together"),
			vec![
				"deepseek-ai/DeepSeek-V4-Pro-0813:fireworks-ai",
				"moonshotai/Kimi-K3:baseten"
			]
		);
		// Failed model not in the list: full list in order.
		assert_eq!(
			models.rotation_candidates("zai-org/GLM-5"),
			vec![
				"moonshotai/Kimi-K3:baseten",
				"moonshotai/Kimi-K3:together",
				"deepseek-ai/DeepSeek-V4-Pro-0813:fireworks-ai"
			]
		);
		// No list: no rotation.
		assert!(CoderModels::default()
			.rotation_candidates("moonshotai/Kimi-K3")
			.is_empty());
	}

	#[test]
	fn user_override_is_authoritative_in_both_directions() {
		let mut models = CoderModels::default();
		let mut cache = HashMap::new();
		cache.insert("openai/gpt-4o-mini".to_owned(), 128_000u32);
		models.context_windows = Arc::new(cache);
		let mut caps = HashMap::new();
		caps.insert("openai/gpt-4o-mini".to_owned(), 1_000_000u32);
		models.context_window_overrides = Arc::new(caps);

		// An override *raises* past a stale/under-advertised catalog
		// value (routers list 128k for models that ship 1M) — the
		// old `min()` semantics made that impossible to correct.
		assert_eq!(models.context_window("openai/gpt-4o-mini"), 1_000_000);

		// …and still caps below the catalog.
		let mut caps = HashMap::new();
		caps.insert("openai/gpt-4o-mini".to_owned(), 64_000u32);
		models.context_window_overrides = Arc::new(caps);
		assert_eq!(models.context_window("openai/gpt-4o-mini"), 64_000);
	}

	#[test]
	fn cap_of_zero_is_treated_as_no_cap() {
		// Defensive against the frontend persisting a cleared
		// input as `0` instead of removing the entry: a literal
		// 0-token cap would lock the runner out of every call.
		let mut models = CoderModels::default();
		let mut cache = HashMap::new();
		cache.insert("anthropic/claude-opus-4".to_owned(), 1_000_000u32);
		models.context_windows = Arc::new(cache);
		let mut caps = HashMap::new();
		caps.insert("anthropic/claude-opus-4".to_owned(), 0u32);
		models.context_window_overrides = Arc::new(caps);

		assert_eq!(models.context_window("anthropic/claude-opus-4"), 1_000_000);
	}

	#[test]
	fn provider_specific_cap_does_not_apply_to_other_routes() {
		// Cap pinned with the `:provider` suffix should clamp
		// only the matching wire slug; the bare id stays
		// uncapped.
		let mut models = CoderModels::default();
		let mut cache = HashMap::new();
		cache.insert("anthropic/claude-opus-4".to_owned(), 1_000_000u32);
		models.context_windows = Arc::new(cache);
		let mut caps = HashMap::new();
		caps.insert("anthropic/claude-opus-4:scaleway".to_owned(), 200_000u32);
		models.context_window_overrides = Arc::new(caps);

		assert_eq!(models.context_window("anthropic/claude-opus-4:scaleway"), 200_000);
		assert_eq!(models.context_window("anthropic/claude-opus-4"), 1_000_000);
		assert_eq!(models.context_window("anthropic/claude-opus-4:fastest"), 1_000_000);
	}
}
