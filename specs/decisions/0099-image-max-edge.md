# ADR 0099 — Image attachments are capped at 2000 px on the longest edge

Date: 2026-10-29
Status: accepted. Supersedes the "Downscaling" rejected alternative in
ADR 0049.

## Context

A long moon-landing session (46 screenshots, mostly 2x-DPR captures at
2160x1500 and 2560x1800, plus a 3062x1461 paste and a 1920x4313
full-page capture) started failing every request on Anthropic with
`messages.N.content.M.image.source.base64.data: At least one of the
image dimensions exceed max allowed size for many-image requests`.
Anthropic allows 8000 px per image, but once a request carries more
than 20 images _every_ image must fit a much smaller box (documented as
2000 px; the error text has also said 3000). The session is stuck from
then on: the oversized image stays in the history and is resent on
every turn.

ADR 0049 rejected downscaling on the premise that playwright frames are
1440x900. HiDPI pastes and full-page screenshots break that premise.

## Decision

- **Capture:** `read_file` on an image, MCP `image` blocks and composer
  pastes are downscaled (Lanczos3, aspect preserved) to fit 2000 px
  before the WebP re-encode. JPEG stays JPEG (q90); everything else
  goes through PNG and on to lossless WebP as before.
- **Reload:** images replayed from the JSONL are downscaled the same
  way, so sessions captured before the cap recover by reopening them.
  The JSONL keeps the original pixels. A header-only probe skips the
  full decode for images already within the cap.
- One cap for every provider. Vision models resample to ~1568 px
  anyway, so 2000 px loses nothing the model would have seen.

## Rejected alternatives

- **Cap only on the Anthropic route, or only past 20 images.** The
  history is provider-agnostic and the model can be switched
  mid-session; a per-route rule would rewrite the prompt prefix (and
  miss the cache) at the moment the 21st image arrives.
- **Elide oversized images like the byte budget does.** Drops an image
  the user may still be asking about, when a resize keeps it legible.
- **Hand-rolled resize on `png` + `webp`.** `image` is already in the
  tree via arboard and covers JPEG pastes too.
