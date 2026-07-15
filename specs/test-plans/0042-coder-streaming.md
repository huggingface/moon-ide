# Test plan 0042: coder streaming (Phase 6.1)

- **Date**: 2026-05-05
- **Phase**: 6.1 — Streaming

## What shipped

- `InferenceClient::chat_completion_stream` POSTs with
  `stream: true`, parses the OpenAI-shape SSE wire (`\n\n` and
  `\r\n\r\n` event boundaries, `data: …` lines, `[DONE]`
  terminator), and accumulates content + tool-call fragments
  back into an `AssistantResponse`. Five unit tests cover the
  parser end to end.
- New event vocabulary on `coder:event`:
  `assistant_message_start { id }` →
  N × `assistant_thinking_delta { id, delta }` (optional) and
  N × `assistant_message_delta { id, delta }` →
  `assistant_message_end { id, text, thinking? }`. The old
  `assistant_message` event is gone (per "no premature
  migrations" — single writer / single reader, no compat
  shim needed).
- Reasoning trace support: backend accepts `reasoning_content`
  (DeepSeek, Qwen) and `reasoning` (other providers) under
  one `thinking` buffer, fired as `assistant_thinking_delta`.
  UI renders it in a collapsible block above the answer that
  auto-collapses when the message ends.
- Runner streams content live; tool-call fragments are
  buffered inside the inference client and surfaced as a
  single `tool_call` event once fully assembled.
- Esc-abort drops the SSE byte stream on the next chunk via
  the existing `CancellationToken` (same token already
  cancelled in-flight tool calls). The token is also raced
  against route resolution and the 401-retry token refresh, so
  Esc lands even when the turn is parked in an OAuth round trip
  (the original "hangs entirely" failure mode). Both HTTP
  clients carry a connect timeout so a black-holed endpoint
  can't park a turn without Esc either.
- Frontend reconciles deltas by id; `CoderMarkdown` coalesces
  re-renders to one per `requestAnimationFrame` so a 30 Hz
  delta stream doesn't spawn 30 markdown renders per second.

## How to test

Prerequisites: `bun install`, `bun run dev`, signed in to
Hugging Face (per test plan 0039), an active workspace folder
with at least one source file.

### Smoke

1. Open the coder panel from the status bar.
2. Send `say hi in a paragraph or two`.

   Expected:
   - The assistant bubble appears within ~200 ms of pressing
     Enter (not after the full response).
   - Text fills in smoothly. Markdown formatting (paragraphs,
     bold) updates as it streams.
   - When the model finishes, the bubble re-renders once
     (`assistant_message_end` triggers a final markdown
     render).

### Esc-abort mid-stream

3. Send `write me a 500-word essay about typewriters, format as
markdown with H2 sections`.
4. While text is still streaming (not after), press `Esc` (or
   the panel-header stop button).

   Expected:
   - Streaming stops within one chunk.
   - The bubble keeps the partial text — abort never erases
     work the model has already produced.
   - `aborted` row appears below.
   - The composer re-enables; you can send another prompt
     immediately.

### Esc-abort during route resolution / token refresh

4b. Force the OAuth token to be near-expiry so the next request
triggers a refresh: edit the cached token bundle's `expires_at` in
the keyring (or temporarily lower `REFRESH_LEAD_TIME_SECS`), or
point `HF_HUB_BASE` at a local server that accepts the TCP
connection but never responds to `/oauth/token`. Send a prompt.
While the turn is parked in the token-refresh round trip (before any
SSE arrives), press `Esc`.

Expected:

- The turn aborts immediately (within a second), not after a
  30 s timeout. The cancel token is raced against the refresh
  `.await`, so Esc interrupts it.
- `aborted` row appears; `busy` clears; composer re-enables.
- If instead you _don't_ press Esc and the endpoint is truly
  stalled, the turn surfaces a transport error within 30 s
  (the auth client timeout) rather than hanging forever.

### Reasoning models (thinking blocks)

5a. **Thinking renders for reasoning models.** Sign in, then
swap the runner's hardcoded model to a reasoning model
(`deepseek-ai/DeepSeek-R1-Distill-Qwen-32B:nebius` or
`Qwen/QwQ-32B-Preview:hyperbolic`) by editing
`crates/moon-coder/src/defaults.rs:DEFAULT_LARGE_MODEL`.
Send `which sorts faster on average — quicksort or merge
    sort? think before answering`.

    Expected:
    - A grey `THINKING` disclosure appears above the answer
      bubble. While the model is reasoning the disclosure is
      open and text streams in.
    - When the model starts answering, the answer bubble
      appears below the thinking block; thinking continues to
      stream until the answer takes over.
    - On `assistant_message_end`, the thinking disclosure
      auto-collapses. Click to reopen — the trace is preserved.

5b. **No empty disclosure for non-reasoning models.** Restore
the default model and send the same prompt. Expected: no
thinking disclosure renders (`thinking` was empty
server-side, so it's `None` on the wire and the UI just
doesn't draw the block).

### Tool-call surfacing

5. Send `read the first 30 lines of AGENTS.md and summarise the
tools section`.

   Expected:
   - The assistant bubble streams a "thinking" preamble (if the
     model emits one).
   - **One** `tool_call` block appears (not partial). The
     args object is fully formed JSON
     (`{"path":"AGENTS.md","start_line":1,"end_line":30}`),
     not a half-typed string.
   - After the tool result lands, a _second_ assistant bubble
     starts streaming with the summary.

### Multi-iteration turn

6. Send `find every place we call write_file in moon-coder and
show me the lines around the third hit`. (Same prompt as
   test plan 0041.)

   Expected:
   - Two distinct streaming bubbles, separated by the
     intermediate `grep` and `read_file` tool calls.
   - Each bubble's id is different (open the panel inspector
     and check the `data-` attributes on the row, or watch the
     events stream in `bun run dev`'s console with
     `localStorage.setItem('debug', '*')` if you want).

### Wire-shape sanity

7. With the dev console open, run:

   ```js
   window.__moonDebugEvents = [];
   ```

   then add a one-line listener (or temporarily uncomment the
   debug log in `coder.svelte.ts:#applyEvent`). Send a short
   prompt. Expected event order on a 1-tool turn:

   ```text
   user_message
   assistant_message_start  (a)
   assistant_message_delta  (a, "I")
   assistant_message_delta  (a, "'ll read")
   …
   assistant_message_end    (a, full text)
   tool_call                (read_file)
   tool_result              (read_file)
   assistant_message_start  (b)
   assistant_message_delta  (b, …)
   …
   assistant_message_end    (b, full text)
   turn_complete
   ```

### Provider keep-alive resilience

8. Some providers send `: ping\n\n` keep-alive frames. The
   parser ignores any line starting with `:` (see
   `extract_data_lines`). Confirm by reading the
   `extract_data_skips_comments_and_keepalives` unit test
   (`cargo test -p moon-coder inference`); a 14-test pass means
   keep-alives can't surface as a parse error mid-stream.

### Long-output performance

9. Send `produce a 2000-word essay on how a Rust borrow
checker works`. Watch CPU usage (Activity Monitor / `top`)
   while it streams.

   Expected:
   - The webview's CPU stays well under 100 % of one core. The
     rAF coalescer on `CoderMarkdown` is the load-bearing
     piece here — without it a 30 Hz delta stream would chase
     ~30 markdown renders per second per panel.
   - No visible jank in the rest of the IDE (file tree
     scrolling, editor typing both stay smooth).

## What must keep working

- Non-streaming `chat_completion` (still used for sub-agents
  in 6.x and any future test fixtures) — its only behavioural
  diff vs. before is the explicit `stream: false` field on the
  wire.
- `coder_abort` Tauri command still cancels the SSE read,
  any in-flight tool dispatch, _and_ the route-resolution /
  401-retry token-refresh awaits.
- A single `tool_call` event still pairs with a single
  `tool_result` event, keyed by id (same contract as Phase
  6.0).
- The `MAX_TURN_ITERATIONS` cap still fires if a model loops
  through tool calls forever — the streaming path doesn't
  bypass it.

## Known limitations

- Reasoning / "thinking" deltas (some HF providers emit
  `reasoning_content` or `thinking` separately from `content`)
  are not surfaced. The parser silently drops the field; add
  it as a separate event variant when a real prompt benefits
  from it. Out of scope for 6.1 per the roadmap update.
- ~~`CoderMarkdown` re-renders the _full_ assistant text every
  rAF tick, not just the new tail.~~ Resolved by [ADR 0032](../decisions/0032-block-level-markdown-streaming.md):
  `CoderMarkdown` now splits the parsed token stream into
  top-level blocks and renders each independently. Frozen blocks
  (everything except the still-growing tail) hit the per-block
  cache and Svelte's keyed `{@html}` skips the `innerHTML` write,
  so their DOM nodes survive across deltas — no flicker. Only the
  live tail block is re-rendered.
- Tool calls dispatch sequentially within one assistant turn.
  pi-mono's parallel dispatch is on the roadmap if a workload
  needs it.

## Related

- Specs:
  [`specs/coder.md`](../coder.md#loop-shape) (event vocabulary
  table updated in this commit).
- Roadmap:
  [`specs/roadmaps/phase-06-coder.md` § 6.1](../roadmaps/phase-06-coder.md#61--streaming--done).
- Prior test plans:
  [0039-coder-skeleton.md](./0039-coder-skeleton.md),
  [0040-coder-write-tools.md](./0040-coder-write-tools.md),
  [0041-right-panel-single-slot.md](./0041-right-panel-single-slot.md).
