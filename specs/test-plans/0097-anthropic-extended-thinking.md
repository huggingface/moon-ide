# Test plan 0097: Anthropic native extended / adaptive thinking

- **Date**: 2026-06-11
- **Phase**: post-Phase 6 coder polish

## What shipped

- The native Anthropic path (`kind=anthropic`) requests `thinking: {type: "adaptive", display: "summarized"}` for the modern adaptive models (Fable 5, Mythos 5, Opus 4.6/4.7/4.8), so reasoning actually streams back (they omit it by default). Every other model — notably Haiku in its cheap-summarisation / auto-rename role — sends no `thinking` object. The older manual `enabled` shape is deliberately unsupported; nobody routes work through pre-adaptive models.
- Signed/redacted thinking blocks are now round-tripped verbatim. A new opaque `thinking_blocks` list threads through `AssistantResponse` → `ChatMessage::Assistant` → `SessionRecord::Assistant`; the translator replays them, in order, before the `tool_use` blocks. This fixes the "Fable 5 emits one batch of tool calls and then nothing" failure (the second tool round-trip was 400-ing because the required thinking block was dropped).
- The signed blocks persist into the session JSONL (as moon-specific fields on the pi `thinking` / `redacted_thinking` content blocks), so a session reopened mid-tool-loop replays correctly instead of 400-ing on the first send.
- `max_tokens` is model-aware (`max_tokens_for`): the adaptive thinking models get a 32 K ceiling so reasoning + a full answer both fit; Haiku / anything unrecognised keeps the conservative 8 K. (`max_tokens` is required by the API; it can't simply be omitted.)
- Non-Anthropic providers are untouched: `thinking_blocks` is empty for them and the OpenAI-compat wire body is byte-for-byte identical to before.

## How to test

Prerequisites: an Anthropic API key with access to a current thinking model (ideally one adaptive-only model such as `claude-opus-4-8` or `claude-fable-5`, and one `enabled`-style model such as `claude-sonnet-4-5`). A working multi-provider setup (test plan 0072 covers adding a provider).

1. `bun dev`. Open the coder model-settings popover, add/select an **Anthropic** provider, set the standard model to an adaptive model (e.g. `claude-opus-4-8` or `claude-fable-5`) and the cheap model to a Haiku (e.g. `claude-haiku-4-5-20251001`). Close the popover.
2. Send a prompt that forces a multi-step tool loop, e.g. _"Read `AGENTS.md`, then read `specs/coder.md`, then summarise how the coder routes Anthropic requests."_ Expected:
   - A grey `THINKING` disclosure appears with **non-empty** summarized reasoning (this is the Opus-4.8 "no reasoning" fix — previously empty).
   - The agent runs **multiple** tool calls across **multiple** round-trips and produces a final answer. It does **not** stop after the first batch of tool calls (this is the Fable-5 fix). No 400 in the logs about "thinking blocks ... cannot be modified".
3. From a `tracing::debug!` or a proxy, inspect one request body on the adaptive model. Expected: `"thinking": {"type": "adaptive", "display": "summarized"}` with **no** `budget_tokens` key, and `"max_tokens": 32000`.
4. Trigger the cheap model (let a long session auto-compact, or let the auto-rename fire). Inspect that request body. Expected: **no** `"thinking"` key at all, and `"max_tokens": 8192`.
5. Inspect the **second** request of a tool loop (after the first tool result). Expected: the most recent assistant message's `content` array begins with a `{"type":"thinking", ..., "signature":"..."}` (or `redacted_thinking`) block, **then** the `tool_use` block(s).
6. **Reload mid-loop.** Start a tool-heavy turn, let the model emit a thinking block + tool calls, then close and reopen the session before the turn fully completes (or reopen after it completes and send a follow-up). Expected: the next send succeeds — no 400. Inspect the JSONL: the assistant line's `content` has a `thinking` block carrying a `signature` field (and a `redacted_thinking` block with `data` if one was produced).

## What must keep working

- **Non-Anthropic providers unchanged.** Send a turn on the HF router, a custom OpenAI-compat endpoint, and OpenRouter (Anthropic and non-Anthropic slugs). Expected: byte-for-byte the same wire body as before — no `thinking` object, no `thinking_blocks`, no extra content blocks. The `wire_messages_*` and OpenRouter-cache tests pin this.
- **Prompt caching still fires** on the native Anthropic path (system + last user/tool marker) and via OpenRouter (test plan 0073). The thinking blocks ride ahead of the cache marker on assistant turns, which are never the marked message.
- **Legacy / non-thinking sessions** (assistant turns with no thinking block) still load and send: Anthropic's graceful-degradation silently disables thinking for an incompatible request rather than erroring.
- **Empty-shell assistant turns** are still dropped on both persist and load (no regression to the `text content blocks must contain non-whitespace text` guard).
- `cargo test -p moon-coder` green (232 tests), `cargo clippy -p moon-coder --all-targets -- -D warnings` clean.

## Known limitations

- **Model classification is by id substring** (`is_adaptive_thinking_model`). Only the known adaptive families (Fable 5, Mythos 5, Opus 4.6/4.7/4.8) get thinking + the 32 K ceiling; everything else — Haiku, and any unrecognised slug — sends no `thinking` and gets the conservative 8 K. A brand-new adaptive model with an unrecognised slug would silently get no thinking until its family token is added here. We deliberately don't probe capabilities at runtime.
- **No support for the older manual `enabled` thinking shape.** Nobody routes work through pre-adaptive models; Haiku (the one non-adaptive model we use) wants no thinking anyway. If that changes, add an `enabled` branch back to `thinking_config_for`.
- **Summarized, not full, thinking.** We request `display: "summarized"`; raw thinking tokens are never surfaced (Anthropic only returns them to allow-listed orgs). The summary is what the `THINKING` disclosure shows.
- **OpenRouter Anthropic path doesn't round-trip signed blocks.** Caching there still works; signed-thinking replay is a native-`kind=anthropic`-only concern because OpenRouter normalises the shape on the way through. If interleaved thinking over OpenRouter ever 400s, that's the place to look.

## Related

- Specs: [`specs/coder.md`](../coder.md) § "Extended / adaptive thinking (native Anthropic)".
- Prior test plans: [0072](0072-coder-multi-provider.md) (multi-provider), [0073](0073-anthropic-prompt-caching.md) (prompt caching), [0042](0042-coder-streaming.md) (streaming / thinking deltas).
- Anthropic docs: extended thinking <https://platform.claude.com/docs/en/build-with-claude/extended-thinking>, adaptive thinking <https://platform.claude.com/docs/en/build-with-claude/adaptive-thinking>.
