# ADR 0049 — Image payloads are budgeted in bytes, not tokens

Date: 2026-07-30
Status: accepted

## Context

A long UI-review session (21 playwright screenshots, `read_file` on
each) started failing every request with `HTTP 413 {"error":"request
entity too large"}` from `https://router.huggingface.co/v1/chat/completions`.

The numbers, from the session's own JSONL and the backend's request
log:

- Request body 5,280,162 B against moon-landing's
  `express.json({ limit: "5MB" })` — 5 MiB = 5,242,880 B — on
  `POST /v1/chat/completions`. The last successful request was
  5,162,010 B; the next was ~5,259,762 B.
- **5,025,588 B of that (95%) was base64 image data.** Text, tool
  results and source files were 254 kB.
- Each 1440x900 screenshot cost ~1,847 provider tokens but 200-520 kB
  of wire bytes. Text ran ~4 B/token; images ~250 B/token, i.e. **~60x
  heavier per token**.
- The provider reported 101,178 input tokens — nowhere near any
  context limit. Compaction triggers on `prompt_tokens /
context_window ≥ 0.80`, so it never fired, and could not have: 21
  screenshots move that ratio by ~36k tokens while adding 5 MB.

So the failure mode was invisible by construction. Every accounting
surface we had was denominated in tokens, and images are the one input
where tokens tell you nothing about bytes. The 72 kB PNG the user read
when it finally broke contributed 95,880 B — the last straw on a body
that had been over-full for hours, which is why the blame landed on it.

Two smaller bugs fell out of the same investigation:

- The context ring read "125k tokens" because `estimate_prompt_tokens`
  counted an image's base64 length at bytes/4: `101,178 + 60 +
95,880/4`. That overstates one screenshot by >10x and sent us
  looking for a context-accounting bug that did not exist.
- A 413 surfaced as the provider's bare `request entity too large`,
  which names neither the body size nor the images that filled it.

## Decision

### Re-encode captured images to lossless WebP, once, at capture

`crates/moon-coder/src/images.rs` decodes PNG attachments and
re-encodes them as lossless WebP (libwebp, `method = 4`), keeping the
result only when it is smaller. Applies to `read_file` on an image,
MCP `image` blocks and composer pastes. The re-encoded form is what
enters `session.messages` and the JSONL, so the cost is paid once per
screenshot rather than once per round-trip.

Measured on the 21 screenshots above: 5.03 MB → 2.85 MB of base64
(−43%), 0.22 s/frame, pixel-identical. The session that could not send
a single further message came back at 3.01 MiB.

Capture-time is the only shrink that is free with respect to prompt
caching: the bytes settle before they enter the history, so no prefix
is ever rewritten.

### Budget image bytes on the wire, stickily

Per-route byte budget (`InferenceClient::image_wire_budget`), set only
for the HF router — Anthropic accepts 32 MB and a user's own
`base_url` is their business. Once image payload crosses `ceiling`
(3.5 MB), the oldest attachments are dropped from the _wire copy_ of
the history until it is back under `floor` (1.5 MB).

Three properties matter:

- **Sticky.** The dropped set lives on the session and only grows, so
  the prompt prefix is stable between events. A stateless "keep the
  newest N bytes" rule would rewrite the prefix on _every_ new
  screenshot, invalidating the provider's prompt cache each time. The
  ceiling/floor gap buys one cache miss per ~2 MB of new screenshots
  instead.
- **Non-destructive.** `session.messages` and the JSONL keep every
  image; only the outgoing copy is trimmed. The panel still renders
  them, reload still replays them, and raising the budget restores
  them with no migration.
- **Keyed by payload, not position.** Survives compaction deleting
  messages, and incidentally dedupes: two byte-identical screenshots
  share a key. (The measured session contained one such pair.)

### Estimate images at a flat per-image token cost

`IMAGE_TOKENS = 1_700` per attachment instead of base64 length / 4,
matching the ~1,730 measured. 413s additionally report the body size
and the image share.

## Rejected alternatives

- **JPEG.** Measured _larger_ than the source PNG at q95 (6.11 MB vs
  5.03 MB of base64): flat UI colour is JPEG's worst case. q75 saves
  34% but blurs the small text and 1px borders a UI review exists to
  judge.
- **256-colour palette PNG.** Smallest of all (1.72 MB, −66%) and
  visually fine on flat screenshots, but it bands gradients and
  shadows — 6.2% of pixels changed, max channel delta 89, on the
  gradient-heavy frames. Degrading exactly the signal the agent is
  asked to critique is not a saving.
- **Pure-Rust `image-webp` encoder.** Avoids the C dependency but its
  lossless encoder only reached −11% (4.48 MB), against libwebp's
  −43%.
- **libwebp `method = 6`.** −52% instead of −43%, at 1.6 s/frame
  instead of 0.22 s. Not worth 7x the latency while the user waits.
- **Downscaling.** Playwright frames are already 1440x900 at 1x DPR,
  under the ~1568 px that vision models downscale to anyway. Cutting
  further would cost legibility.
- **Provider-explicit router path.**
  `router.huggingface.co/{provider}/v1/chat/completions` has no body
  limit at all (moon-landing reads it via `stream/consumers` rather
  than `express.json`). Rejected: it demands the provider's own model
  id rather than the HF repo id or the `:provider` suffixed form,
  per-provider path suffixes are inconsistent (`/groq/openai/v1/…`,
  `/cohere/compatibility/v1/…`, `/novita/v3/openai/…`), it forfeits
  OpenAI-shaped errors, capability checks and the
  `:fastest`/`:cheapest` policies, and the missing limit is an
  oversight rather than a contract. Depending on it would be building
  on something that can close without notice.
- **Eviction alone, no re-encode.** Would have worked, but it pays for
  every screenshot with a prompt-cache miss. WebP is what keeps the
  budget from ever being reached in an ordinary session; eviction is
  the backstop for the pathological ones.
- **Compaction handling it.** Compaction is token-triggered and image
  bytes barely move the token count. Extending it to trigger on bytes
  would spend an LLM summarisation call and discard text history
  because screenshots piled up.

## Notes

The 5 MiB cap is low for vision workloads — it was itself raised from
1 MB for this reason (moon-landing `debf05004`) — and raising it
further is being tracked on that side. `HF_IMAGE_WIRE_BUDGET` and
`REENCODE_TO` are the two knobs to revisit when it moves; neither
change needs a migration.
