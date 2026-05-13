//! Coder model picker types.
//!
//! Wire shapes for `coder_list_models` (the catalog the picker
//! consumes) and `coder_set_models` (the picker's write back). The
//! catalog mirrors `https://router.huggingface.co/v1/models` —
//! trimmed to fields the picker actually renders so a router-side
//! addition doesn't flow into our schema by accident.
//!
//! Filtering / sorting happens client-side. The router returns the
//! list sorted "most popular first" (its words), and we just
//! forward that ordering — the picker preserves it unless the user
//! types something into the search box.
//!
//! The picker also exposes user-added OpenAI-compatible providers
//! (OpenRouter, locally-hosted vLLM/Ollama, …) via
//! [`CoderProviderConfig`]. HF is the always-implicit default; user
//! providers stack alongside it. Switching providers swaps the
//! `(standard_model, cheap_model)` pair the runner reads — slugs
//! aren't portable between hosts, so picks are stored per-provider.
//! API keys live in the OS keyring, never in [`CoderModelSettings`].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One model entry, denormalised from the router response into a
/// shape the picker can render directly. `providers` is the
/// authoritative list of routes available for this model id; the
/// picker renders each as an expandable sub-row with context /
/// pricing / throughput / TTFT, and the user clicks the specific
/// `(model, provider)` pair they want — the runner concatenates
/// `model.id` + `:` + `provider.provider` and saves that as the
/// wire slug.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[ts(export)]
pub struct RouterModel {
	/// Canonical model slug — what goes into the `model` field of
	/// the chat-completions request, **without** any `:provider`
	/// suffix.
	pub id: String,
	/// Org or user that owns the model on the Hub. Surfaced as a
	/// secondary label in the picker so a long list with multiple
	/// `Qwen3.5-*` entries can be visually grouped.
	pub owned_by: String,
	/// `true` iff at least one provider in [`providers`] reports
	/// `supports_tools: true`. The picker uses this to filter the
	/// standard-model list — the main agent loop calls `tool_calls`
	/// every iteration, so a non-tool-capable model wouldn't work
	/// at all there. The cheap-model list is unfiltered because the
	/// cheap call sites (auto-rename, branch-name suggester,
	/// compaction summary) don't use tools.
	pub supports_tools_anywhere: bool,
	/// Provider routes. Each entry corresponds to one of the
	/// `:provider` suffixes the router accepts. The router's
	/// "most-popular first" ordering is preserved here too.
	pub providers: Vec<RouterProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[ts(export)]
pub struct RouterProvider {
	/// Provider slug (`novita`, `scaleway`, `together`, …). This
	/// is what gets concatenated after `:` to form the wire model
	/// id — e.g. `Qwen/Qwen3.5-397B-A17B` + `scaleway` →
	/// `Qwen/Qwen3.5-397B-A17B:scaleway`.
	pub provider: String,
	/// Context window in tokens. `None` when the provider didn't
	/// advertise one; the picker shows "—" in that case and the
	/// runner falls back to
	/// [`moon_coder::defaults::context_window_for`] for the usage
	/// ring.
	#[ts(optional, type = "number | null")]
	pub context_length: Option<u32>,
	/// Whether the provider supports `tool_calls`. Picker uses
	/// this per-row to grey out provider chips that the standard
	/// model can't actually route to.
	pub supports_tools: bool,
	/// Optional USD-per-1M-tokens pricing pair. `None` when the
	/// provider didn't expose it — usually a self-hosted route
	/// where pricing is per-deployment rather than per-token.
	#[ts(optional, type = "RouterPricing | null")]
	pub pricing: Option<RouterPricing>,
	/// Mean time-to-first-token in milliseconds, as measured by
	/// the router's internal probes. `None` when the route is too
	/// young to have measurements (e.g. `featherless-ai` entries
	/// on first day a model is exposed) or simply too rarely
	/// hit. Picker surfaces this so the user can compare
	/// latency between providers at a glance.
	#[ts(optional, type = "number | null")]
	pub first_token_latency_ms: Option<f64>,
	/// Mean output throughput in tokens-per-second, again from
	/// the router's probes. Same `None` semantics as
	/// [`first_token_latency_ms`].
	#[ts(optional, type = "number | null")]
	pub throughput: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq)]
#[ts(export)]
pub struct RouterPricing {
	/// USD per 1M input tokens.
	pub input: f64,
	/// USD per 1M output tokens.
	pub output: f64,
}

// `RouterPricing` is `PartialEq` not `Eq` because `f64` isn't `Eq`
// (NaN); that's fine, the picker never hashes it.

/// Read/write payload for `coder_get_model_settings` /
/// `coder_set_model_settings`.
///
/// Mirrors the [`crate::app_state::CoderAppState`] subset the
/// picker cares about. The runner reads `standard_model` /
/// `cheap_model` / `bill_to` straight from
/// [`moon_coder::CoderModels`] (kept in sync on every write); slugs
/// are already in their final `model:provider` form by the time they
/// get here because the picker concatenates on click.
///
/// **Active provider** is `None` for the implicit HF route (the
/// `standard_model` / `cheap_model` / `bill_to` fields apply only
/// in that case), or `Some(id)` matching one of the [`providers`]
/// entries — in which case the runner reads the picks off that
/// provider's record instead. User providers don't have a
/// `bill_to`; the HF-specific header is suppressed off the wire when
/// active.
///
/// [`providers`]: Self::providers
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CoderModelSettings {
	pub standard_model: String,
	pub cheap_model: String,
	pub bill_to: String,
	#[serde(default)]
	#[ts(optional, type = "string | null")]
	pub active_provider: Option<String>,
	#[serde(default)]
	pub providers: Vec<CoderProviderConfig>,
}

/// One user-added OpenAI-compatible provider — OpenRouter, a local
/// llama.cpp / vLLM / Ollama, a hosted Anthropic-via-proxy, …
/// Persisted into [`crate::app_state::CoderAppState::providers`];
/// the API key lives in the OS keyring under
/// `service=moon-ide`, `account=coder-provider:<id>` and is never
/// surfaced in this struct — only the [`has_api_key`] flag is.
///
/// `id` is opaque and assigned by the backend on
/// `coder_add_provider`. The picker addresses providers by id
/// across read / write / delete; `label` is a free-form human name
/// the user picked.
///
/// [`has_api_key`]: Self::has_api_key
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[ts(export)]
pub struct CoderProviderConfig {
	pub id: String,
	pub label: String,
	/// OpenAI-compat `/v1` root, e.g.
	/// `https://openrouter.ai/api/v1` or
	/// `http://localhost:8080/v1`. Stored verbatim; the runner
	/// builds request URLs by appending `/chat/completions` etc.
	pub base_url: String,
	/// Per-provider standard model slug. Same wire shape as the
	/// HF `standard_model` — empty string falls back to the
	/// hardcoded default at request time.
	#[serde(default)]
	pub standard_model: String,
	/// Per-provider cheap model slug. Same semantics as
	/// [`standard_model`](Self::standard_model).
	#[serde(default)]
	pub cheap_model: String,
	/// `true` iff the keyring currently holds an entry for this
	/// provider. Server-set, read-only on the picker's side —
	/// editing the key goes through `coder_set_provider_api_key`.
	/// Deserialised as `false` when the field is missing so
	/// inbound shapes from the frontend don't have to set it.
	#[serde(default)]
	pub has_api_key: bool,
}

/// Result of a `coder_probe_provider` call. Returned on the
/// `Add provider` modal's verify gesture so the user can confirm
/// the URL + key combination actually reaches a sane endpoint
/// before saving. Failures land as `MoonError`; this success
/// shape only carries advisory info (model count, optional
/// `id`-shaped names) the picker shows inline.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProviderProbeResult {
	/// Number of model entries the probe returned. `0` is
	/// possible (well-formed `/v1/models` with an empty list,
	/// or a 1-token completion fallback that gives no catalog
	/// info). Picker uses this just to render a confirmation
	/// blurb.
	pub model_count: u32,
	/// First few model ids the probe surfaced, capped at a
	/// handful for UI breathability. Empty when the probe
	/// fell back to the chat-completion path.
	pub sample_model_ids: Vec<String>,
}

/// One row of a user-added provider's `/v1/models` catalog.
///
/// Minimum required: `id`. Everything else is best-effort and
/// server-dependent — the OpenAI-compat spec only promises `id`
/// and `owned_by`. Servers that emit a richer shape (OpenRouter
/// publishes `name` / `context_length` / per-token `pricing` /
/// `description` at the top level; LiteLLM does similar for routes
/// it knows about) get the picker to render the extra chips
/// without our needing a server-specific catalog endpoint.
///
/// Pricing is normalised to **US dollars per million tokens** at
/// the parse boundary. The wire shape varies — OpenRouter emits
/// strings of dollars-per-token (`"0.000003"`) which we multiply
/// by `1_000_000`; LiteLLM emits per-million floats directly.
/// `None` means the server didn't advertise pricing, not "free".
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[ts(export)]
pub struct ProviderModelSummary {
	/// Canonical model slug — what the picker writes into
	/// `standard_model` / `cheap_model` on click and what the
	/// runner feeds to `chat/completions` verbatim.
	pub id: String,
	/// Owner/org slug when the server exposes one (OpenRouter,
	/// LiteLLM). `None` for minimal servers that don't bother.
	#[ts(optional, type = "string | null")]
	pub owned_by: Option<String>,
	/// Long human-readable name when the server provides one
	/// (OpenRouter: `"Anthropic: Claude 3.5 Sonnet"`). Picker
	/// shows it under the slug; `None` falls back to the slug
	/// alone.
	#[serde(default)]
	#[ts(optional, type = "string | null")]
	pub name: Option<String>,
	/// Max prompt context window for this model on this server,
	/// in tokens. Picker renders as a `200k ctx` chip.
	#[serde(default)]
	#[ts(optional, type = "number | null")]
	pub context_length: Option<u32>,
	/// Input price, normalised to $/M tokens. See struct-level
	/// docs for the wire-shape normalisation.
	#[serde(default)]
	#[ts(optional, type = "number | null")]
	pub pricing_in_per_million: Option<f64>,
	/// Output price, normalised to $/M tokens. Same caveat as
	/// [`pricing_in_per_million`](Self::pricing_in_per_million).
	#[serde(default)]
	#[ts(optional, type = "number | null")]
	pub pricing_out_per_million: Option<f64>,
	/// Short description when the server provides one. Capped to
	/// the first ~200 chars at the parse boundary so a server
	/// that ships a multi-paragraph README per model doesn't blow
	/// the picker UI. `None` when absent.
	#[serde(default)]
	#[ts(optional, type = "string | null")]
	pub description: Option<String>,
}
