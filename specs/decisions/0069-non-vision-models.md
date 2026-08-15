# ADR 0069 — Non-vision models: strip at the wire, warn at the source

## Context

Image attachments (composer pastes, `read_file` on an image, MCP
screenshots) ride on every subsequent round-trip. Models without
image input (DeepSeek et al.) make the HF router reject the whole
request with a 400 — one playwright screenshot wedged the session
until the images aged out of history, and switching a working
session to a text-only model wedged it retroactively.

## Decision

- **Detect from catalogs, cache per slug.** The HF router and
  OpenRouter expose `architecture.input_modalities` per model
  (model-level only; per-provider entries carry no modality info).
  Parsed into `RouterModel.supports_image_input` /
  `ProviderModelSummary.supports_image_input` (tri-state: absent =
  unknown) and merged into `CoderModels::vision`, mirroring the
  `context_windows` cache end-to-end (same suffix-stripped lookup,
  same merge-on-fetch sites, same prime). Anthropic is hardcoded
  vision-capable; vLLM/Ollama/LiteLLM stay unknown.
- **Unknown = vision-capable.** Wrongly stripping pixels from a
  capable model is a silent quality bug; sending them to an
  incapable one is a loud, explainable 400.
- **Strip at request-encode time, keep history.** The inference
  client (which already holds the models handle) swaps each image
  block for an `[image omitted: …]` text block when the slug is
  known non-vision. History keeps the attachments, so model swaps
  are non-destructive in both directions. One choke point covers
  the main loop, sub-agents, wrap-up and companion sends.
- **Tools react at dispatch time.** `read_file` on an image errors
  ("the active model does not accept image input"); MCP image
  blocks render as "not attached" notes steering the model to text
  alternatives (accessibility snapshot over screenshot). The
  registry reads the live models handle, so a mid-turn model swap
  applies to the next dispatch.
- **UI.** "no vision" badge in the picker (both catalogs, not
  filtered out); composer refuses image pastes when
  `resolved_standard_supports_images` (a runner-resolved read-only
  field on `CoderModelSettings`, like `resolved_standard_model`)
  is `false`.

## Rejected alternatives

- **Retry-on-400 with images stripped.** Would cover vision models
  whose specific provider route rejects images (the router can't
  tell us) and unknown local endpoints, but adds an error-sniffing
  heuristic and a duplicate request path for a case nobody has hit.
  Revisit if a real route surfaces it.
- **Filtering non-vision models out of the picker.** They're
  legitimate picks (DeepSeek for text-only work); a badge informs
  without deciding.
- **Blocking the model switch while history has images.** Punishes
  the common "this turn doesn't need the screenshots anyway" case;
  the strip note preserves the conversation instead.
