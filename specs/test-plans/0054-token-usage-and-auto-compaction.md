# Test plan 0054: token usage ring + auto-compaction

- **Date**: 2026-05-06
- **Phase**: Phase 6.x agent polish

## What shipped

- Per-turn token-usage report driven by the OpenAI-compatible streaming `usage` chunk (with a `bytes / 4` fallback). Surfaced as a new `token_usage` event the panel reads to draw a context-window ring in the coder header.
- Hardcoded per-model context windows in `defaults::context_window_for` (256 K for the two HF Qwen slugs the team uses; conservative 128 K fallback for unknown slugs).
- Auto-compaction when the next prompt would cross 80 % of the context window: a fast-model summary call replaces the older middle of the message history with a synthetic system message, keeping the most recent 6 user turns intact. On-disk JSONL stays untouched. New `compaction_started` / `compaction_complete` events render an inline disclosure in the transcript and a pulse on the ring.
- `MAX_TURN_ITERATIONS` raised from 32 → 200; `SUBAGENT_MAX_ITERATIONS` raised from 6 → 50; the sub-agent byte cap (`SUBAGENT_MAX_BYTES`) and `byte_budget_exceeded` field are gone — auto-compaction now backstops sub-agents the same way.
- Iteration-cap failure mode rewritten: instead of bailing with an error banner, the parent (and sub-agents) run one final tools-disabled round-trip with a `[Tool-call budget exhausted: …]` sentinel user message asking for a wrap-up. The model now writes a real answer with what it has instead of leaving the user staring at a wall of tool calls.

## How to test

Prerequisites: `bun install`, signed in to Hugging Face in the panel, on a workspace folder with at least a few thousand lines of code so a few `read_file` calls move the ring.

1. Open the coder panel. Expected: a small empty ring next to the `stop` / panel-switch buttons. Hover: tooltip says `No turns yet.`.
2. Send a small prompt (`tell me what's in this folder`). Expected: after the response lands the ring fills slightly (~1–3 %), tooltip shows `prompt_tokens / 256.0k` with a real percentage. If the provider didn't emit a `usage` chunk you'll see a leading `≈` in the tooltip.
3. Run `tail -F` on the dev server log (or watch the panel) and send a long-context prompt that triggers many `read_file` / `grep` calls. Expected: the ring gradually fills as iterations land. Above 60 % it tints warning yellow, above 80 % it tints danger red.
4. Keep going until the ring crosses ~80 %. Expected: at the **next** iteration's start, a "compacting…" pulse appears (full-width row at the bottom of the transcript, italic; ring also pulses). Within a few seconds it flips to a `<details>` titled `Compacted N earlier messages into a summary`. Click it: the synthetic summary is visible, covering user intent, decisions, and current state.
5. After compaction completes the ring's fill drops sharply (the next prompt is now `messages[0]` + summary + last 6 user turns). Tooltip's prompt-token count reflects the new size. Send another prompt: the agent continues coherently — try `what was I working on?` and verify the answer references the right file/topic from the summary, not just the most recent turn.
6. Force a sub-agent run (`spawn_subagent` via a "research the X folder" prompt). Expected: the sub-agent card under the parent's tool row fills its iteration counter; the parent's ring still updates per parent round-trip. Open the sub-agent pop-out: it streams normally. Hard cap remains 50 iterations.
7. Open `target/debug` build with `RUST_LOG=info` (or a dev build) and send a deliberately huge prompt (paste a 30 K-line file via Ctrl+L). Expected: `tracing::info!("auto-compaction applied", …)` lands once the threshold is crossed; `tracing::warn!` fires only on summary failure.
8. Edit `crates/moon-coder/src/defaults.rs` `context_window_for` to return `1` for the active model, restart, send a prompt. Expected: ring goes red immediately, compaction triggers on the very next iteration. Revert.
9. Iteration-cap wrap-up: temporarily lower `MAX_TURN_ITERATIONS` to `3` in `defaults.rs`, restart, send a prompt that deliberately keeps the agent in tool-loop territory (e.g. "list every file in /etc and read the first three"). Expected: after the third tool round-trip a new user message lands in the transcript reading `[Tool-call budget exhausted: you've used all 3 tool-call iterations …]`, immediately followed by a final assistant answer that summarises what was found and what's still unfinished. No `Error` event banner. Revert. Repeat with `SUBAGENT_MAX_ITERATIONS` lowered: trigger a sub-agent that loops, expected the sub-agent's `result` returned to the parent starts with `[Sub-agent reached the N-iteration cap; final wrap-up follows.]\n\n` and contains a real wrap-up.
10. Per-tool error path (regression): trigger a tool error (e.g. ask the agent to `read_file` a path that doesn't exist). Expected: the tool row turns red with the `{"error": "..."}` payload visible, and on the next iteration the model sees the error and reacts naturally (apologises, tries a different path, etc.) — no wrap-up triggered, no special handling needed.

## What must keep working

- Sending and aborting prompts (`Esc`) — the ring does not interfere with streaming or cancellation.
- Sub-agents (`spawn_subagent`) still spawn, run, and finish; the parent's tool dispatch still receives `result`, `sub_session_id`, `tokens_used_estimate`, `mode`, `iterations_used`. (`byte_budget_exceeded` is gone — confirm any UI didn't depend on it.)
- The on-disk JSONL transcript at `<XDG_DATA_HOME>/moon-ide/coder-sessions/<slug>/<id>.jsonl` stays full — open one in `bat` after a compaction and verify every assistant / tool record is still there.
- `refresh_system_prompt` keeps overwriting `messages[0]` on every turn — open a session, add a new bound folder, send a prompt, and verify the new folder appears in the agent's system prompt despite an in-flight compaction summary at `messages[1]`.
- Multi-folder routing: a sub-agent spawned in folder A that operates on folder B still posts events into folder A's panel bucket; the ring updates in folder A.
- Panel CSS: the ring renders correctly in light and dark themes (color uses `currentColor` + tone classes; track is 18 % opacity for both themes).

## Known limitations

- Summary target size isn't enforced — the fast model is asked for "between 4,000 and 16,000 tokens" but we don't truncate the response. If a model badly overshoots, the next prompt could still be heavy.
- Compaction events on the sub-agent's inner channel are wired but the sub-agent pop-out does not yet render its own ring. The parent's ring + transcript disclosure cover the user-visible case; sub-agent-side rendering is a follow-up.
- Compaction threshold (`0.80`), retained-turn count (`6`), and the summary system prompt are hardcoded. Per AGENTS.md "hardcode first, configure later" — they become user-tweakable when a real workload demonstrates a need.
- The `bytes / 4` fallback estimate is conservative for English text but undercounts heavily punctuated tool JSON. If the active provider stops emitting `usage` chunks, the ring will lag reality slightly.

## Related

- Specs: [coder.md § Token accounting and auto-compaction](../coder.md), [coder.md § Sub-agents](../coder.md)
- ADRs: [0010 coder rewrite (not ACP)](../decisions/0010-coder-rewrite-not-acp.md)
- Prior test plans: [0050 sub-agents](0050-sub-agents.md), [0044 coder polish](0044-coder-polish.md), [0042 coder streaming](0042-coder-streaming.md)
