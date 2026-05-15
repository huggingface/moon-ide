# Test plan 0073: Anthropic prompt caching via OpenRouter

- **Date**: 2026-05-14
- **Phase**: post-Phase 6 polish (multi-provider follow-up)

## What shipped

- The inference layer now emits Anthropic `cache_control: ephemeral` markers on chat-completion requests automatically when the active provider routes through OpenRouter (`base_url` contains `openrouter.ai`) and the model id starts with `anthropic/`. Every other route — HF, any non-OpenRouter custom provider, OpenRouter-but-non-Anthropic model — gets the original string-content wire shape byte-for-byte unchanged.
- Two breakpoints per request: end of system prompt, and end of the last non-assistant message (the most recent user prompt or tool result). The strategy is documented in `cache_breakpoint_indexes` and in `specs/coder.md` under "Prompt caching (Anthropic via OpenRouter)".
- `TokenUsage` gained `cache_read_input_tokens` + `cache_creation_input_tokens`, parsed off the streaming `usage` chunk. `CoderEvent::TokenUsage` forwards them as `cache_read_tokens` / `cache_creation_tokens`; the panel's `ContextRing` tooltip surfaces them on a new `cache: …` line that only renders when at least one side is non-zero.
- The compaction trigger keeps keying off `prompt_tokens` (full input regardless of how it was billed) — caching is a billing concern, not a context-window concern.
- Unit tests pin: route gating (`cache_breakpoints_empty_for_non_openrouter_route`), breakpoint placement (`cache_breakpoints_marks_system_and_last_non_assistant`), wire serialisation in both modes (`wire_messages_no_cache_serialises_as_string_content`, `wire_messages_with_cache_emits_blocks_only_on_marked_indexes`), and usage-chunk parsing (`token_usage_accepts_anthropic_cache_fields`).

## How to test

Prerequisites: an OpenRouter API key with budget for one or two Claude calls; a working multi-provider setup (test plan 0072 covers initial OpenRouter setup).

1. Run `bun run check` and `cargo test -p moon-coder --lib`. Expected: clean, all 107+ tests pass.
2. Launch `bun dev`. Open the coder model-settings popover, switch the active provider to **OpenRouter**, set the standard model to e.g. `anthropic/claude-sonnet-4.5` (any `anthropic/...` slug). Close the popover.
3. Start a fresh session in any non-trivial repo (anything with a system prompt that lands above ~6 K tokens after the bound-folders / folder-summary expansion — the moon-ide repo itself is fine). Send a first user message that triggers at least one tool call, e.g. `read src-tauri/src/lib.rs and summarise it`.
4. After the first round-trip completes, hover the context ring at the top of the coder panel. Expected: the tooltip shows the normal `prompt / context` line **and** a `cache: …k written (+25%)` line — first call always pays the write surcharge on whatever it caches, no reads yet.
5. Send a follow-up prompt within 5 minutes ("now also look at `crates/moon-coder/src/inference.rs`"). After the round-trip, hover the ring again. Expected: the tooltip's `cache:` line now reads `…k read (XX%, -90%) · …k written (+25%)`, where the percent is "what share of the new prompt tokens came off cache". For a 7 K system prompt + a small new user turn, that ratio should be ~70–90 %.
6. Open the OpenRouter activity panel in the browser. For the calls made in steps 3–5, expand the usage rows. Expected: the `cache_read_input_tokens` and `cache_creation_input_tokens` figures shown by OpenRouter match what the IDE's tooltip showed (off-by-one rounding allowed). The "input cost" column on the cached calls should be visibly lower than on the first call.
7. With the same session still active, switch the active provider back to **Hugging Face** in the model-settings popover and send one more message. Expected: the next call's tooltip has **no** `cache:` line (HF route doesn't enable caching; the cache fields stay at zero).
8. Switch back to OpenRouter, change the standard model to a non-Anthropic slug like `openai/gpt-4o`, send another message. Expected: still no `cache:` line in the tooltip — the gating is `is OpenRouter` AND `anthropic/...`, both must hold.
9. From DevTools or a `tracing::debug!` (whatever's convenient), inspect the request body the IDE sends on an OpenRouter+Anthropic call. Expected: the system message and the most recent user/tool message both serialise as `{"role": "...", "content": [{"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}]}`; every other message stays as `{"role": "...", "content": "..."}`. On an HF or non-Anthropic OpenRouter call, every message uses the string-content shape (no `cache_control` anywhere in the body).

## What must keep working

Regression checks. If any of these break, the commit needs a follow-up.

- Non-Anthropic providers (HF router, custom non-OpenRouter, OpenRouter with non-`anthropic/*` models) keep getting byte-for-byte the same JSON they did before. The `wire_messages_no_cache_serialises_as_string_content` test pins this — if a provider that was working before starts 400-ing on extra `cache_control` keys, this test is the alarm.
- The compaction trigger still fires at ~80 % of the context window based on `prompt_tokens`. Caching only changes the billing, not the context-window math — that's intentional, because the model still has to actually process the cached tokens (it just doesn't bill them as new input).
- `bytes/4` estimate fallback still works when the provider doesn't emit a usage chunk: `emit_token_usage` constructs a synthetic `TokenUsage` with zero cache fields, so the tooltip just shows no `cache:` line.
- The auto-rename / cheap-model / folder-summary / sub-agent paths all go through the same `InferenceClient::chat_completion[_stream]`, so they inherit caching automatically when run against an OpenRouter Anthropic model. Worth a sanity check on at least the sub-agent path: spawn a sub-agent, hover its ring, expected: same caching tooltip shape.
- Sessions opened from disk (the JSONL replay path) don't crash on the extended `TokenUsage` — we never persist usage to the JSONL today, but a future test plan or code change could; the defaults on `cache_read_input_tokens` / `cache_creation_input_tokens` cover the read side either way.

## Known limitations

Things we deliberately did not do, with one-line justification.

- **DeepSeek prompt caching is not surfaced.** DeepSeek emits `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` under different field names and auto-caches without `cache_control` markers, so the IDE doesn't have to _enable_ anything — but the panel's tooltip will show 0 / 0 until we plumb the alternate field names through `TokenUsage`. Revisit when somebody starts using DeepSeek as their primary; the wire-side work is already done.
- **No direct Anthropic API path.** OpenRouter is the only Anthropic surface we wire today. A custom provider pointed at `api.anthropic.com` would need a translation layer for the native `/v1/messages` shape (system, tools, messages all live at the top level instead of inside `messages[]`) — out of scope until somebody has a Claude Pro/Max subscription and asks for OAuth-style billing (see `specs/coder.md` § "Later: Anthropic OAuth").
- **No 1-hour TTL option.** 5-minute ephemeral is the default; the 1-hour TTL costs 5× more per cache write and buys nothing for an interactive agent loop where turns land seconds apart. Hardcoded per the AGENTS.md "hardcode first, configure later" rule.
- **No cache-marker placement on the `tools` array.** Anthropic's native format supports `cache_control` on the last tool definition; the OpenAI-compatible `tools` field doesn't carry through the marker reliably across OpenRouter's translation layer. The system-prompt marker covers the same prefix in practice (tools are included in the cache lookup naturally as part of the prefix before any message).
- **No cache-hit indication on the per-message rows.** The tooltip on the context ring is the only surface — adding a "cached" badge on individual transcript rows would be UI clutter for a billing-side concern. Re-evaluate if a user actually asks.

## Related

- Specs: [`specs/coder.md`](../coder.md) § "Prompt caching (Anthropic via OpenRouter)".
- Prior test plans: [0071](0071-coder-model-picker.md) (model picker), [0072](0072-coder-multi-provider.md) (multi-provider — OpenRouter / local LLM bring-up), [0054](0054-token-usage-and-auto-compaction.md) (token usage event shape and compaction trigger).
- Anthropic prompt caching: <https://docs.claude.com/en/docs/build-with-claude/prompt-caching>.
- OpenRouter prompt caching: <https://openrouter.ai/docs/features/prompt-caching>.
