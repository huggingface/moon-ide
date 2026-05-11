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
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CoderModelSettings {
	pub standard_model: String,
	pub cheap_model: String,
	pub bill_to: String,
}
