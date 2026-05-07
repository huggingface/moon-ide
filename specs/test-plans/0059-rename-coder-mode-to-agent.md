# Test plan 0059: rename sub-agent `coder` mode to `agent`

- **Date**: 2026-05-07
- **Phase**: 6.x (coder polish). Tiny surface but it changes a wire string the LLM reads back, and the model picks `mode: "..."` based on the JSON-schema enum, so it's worth a turn at the panel before merging.

## Why

Dogfooding the [0058](0058-cross-folder-routing-and-drop-tiers.md) cross-folder + dropped-tiers changes turned up one residual issue: even with the model selector gone and cross-folder access available, the parent kept treating `mode: "coder"` sub-agents as junior workers and refused to delegate non-trivial tasks to them. The token "coder" reads as a narrower role than the parent's own self-conception. Renaming the wire string to `agent` cleared that up — `agent` reads as "another instance of you" in the model's vocabulary, and the parent stopped second-guessing the delegation.

This is a name-only change. Capabilities are identical to the previous `coder` mode (full toolkit, can edit files, scoped to the assigned folder).

## What shipped

- `CoderMode::Coder` → `CoderMode::Agent` in [`crates/moon-coder/src/tools.rs`](../../crates/moon-coder/src/tools.rs). The Rust enum's surrounding type stays `CoderMode` (the crate is `moon-coder`; renaming the enum itself would touch every callsite for no behavioural reason).
- Wire string: `as_wire()` now returns `"agent"` for the `Agent` variant. `subagent_mode` field on `SessionHeader`, `SubagentSpawned.mode` event field, `spawn_subagent` tool result `mode` echo all carry `"agent"` instead of `"coder"`.
- `spawn_subagent` JSON-schema enum: `["research", "agent"]`; default is `"agent"` when omitted. Parser rejects unknown strings with an error message naming `research` and `agent` as the valid values.
- `CODER_SYSTEM_PROMPT` constant renamed to `AGENT_SYSTEM_PROMPT` and its first sentence rewritten from "You are a coder sub-agent inside moon-ide" to "You are an agent sub-agent inside moon-ide. … Your capabilities are the same as the parent's — you can read, search, run commands, and edit files freely." That last bit is the hint that lifts the "I'm more capable than this delegate" worry.
- `PHASE_6_0_SYSTEM_PROMPT`'s "When to use sub-agents" guidance updated: `mode: "coder"` → `mode: "agent"` with a parenthetical "(an `agent` sub-agent has the same capabilities you do)".
- `spawn_subagent` tool description updated: `mode: "agent" (default)` is described as "the full toolkit — same capabilities as you have, including edits".
- TypeScript: `SubagentMode = 'research' | 'agent'`. CSS class `.subagent-mode.research` is unchanged (the styled variant); the default fall-through still applies to the unified mode (now `agent`). Doc comments in [`src/lib/protocol.ts`](../../src/lib/protocol.ts) and `CoderPanel.svelte` updated.
- `runner.rs`: replaced an inline `match report.mode { CoderMode::Research => "research", CoderMode::Coder => "coder" }` with `report.mode.as_wire()` so a future variant rename only touches the enum.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, signed in, two folders bound, the active one is `moon-ide`.

### Schema is what the model sees

1. Open the active session's trace (test plan 0055).
2. The system prompt's `## Bound folders` and `## When to use sub-agents` sections should mention `mode: "agent"` (not `"coder"`). Confirm.
3. Find the `tools` payload sent to the inference API (it's not in the trace, but you can check it via the network tab if needed). The `spawn_subagent` tool's `parameters.properties.mode.enum` is `["research", "agent"]`.

### Default mode

4. Ask: _"Spawn a sub-agent against this folder to add a comment at the top of `src/main.rs`."_
5. The agent calls `spawn_subagent({task: "...", folder: "moon-ide"})` (no `mode` arg). The sub-agent runs in agent mode (full toolkit) and successfully edits the file.
6. The sub-agent card in the panel shows the mode badge as `agent` (accent-tinted, not the `research` quiet-neutral).
7. Open the sub-agent's session JSONL: header's `subagent_mode` is `"agent"`.

### Explicit research mode still works

8. Ask: _"Use a research sub-agent to summarise `src/lib/`."_
9. The agent calls `spawn_subagent({task: "...", mode: "research"})`. The sub-agent has the read-only toolkit; if it tries to call `write_file` or `edit_file`, those errors with `read_only_mode`.
10. Card mode badge is `research` (quiet-neutral fill).

### Explicit agent mode

11. Ask: _"Use an agent-mode sub-agent to ..."_ (some scoped editing task).
12. The agent calls `spawn_subagent({task: "...", mode: "agent"})`. Sub-agent runs with the full toolkit, edits land in the assigned folder.

### Old `"coder"` value is rejected

13. Ask the agent to send `spawn_subagent({task: "x", mode: "coder"})` literally — easiest way is to manually POST it via the dev console / a one-off test. Expected: the dispatcher returns `CoderError::InvalidArgs` with a message naming `research` and `agent`. Per AGENTS.md "no premature migrations", we don't ship a backwards-compat alias for the old value.

### Persisted sub-agent transcripts from before this change

14. If you have an old session with `subagent_mode: "coder"` in its JSONL header, opening it should still load — `subagent_mode` is just a stringly-typed field, the loader doesn't validate against the enum. The mode badge in the UI will render the literal text `"coder"`. This is acceptable per the AGENTS.md "schemas can be renamed freely; user loses last session at worst" rule.

### Tests + lints

```
cargo test -p moon-coder
cargo clippy --all-targets -- -D warnings
bun run check
bun run lint
bun run fmt
```

All green. The `subagent::tests::build_spec_defaults_to_active_folder_and_coder_mode` test still asserts the default is `CoderMode::Agent` (the test-fn name is now slightly stale but accurate — "the default sub-agent mode is the full-toolkit one"; renaming the test would force everyone to remember which old name maps to which new name on a code-review and isn't worth the churn).

## Side cleanup

- [`specs/test-plans/0044-coder-polish.md`](0044-coder-polish.md) had a malformed list-item-11 (multi-line inline code span breaking the indent + an "Expected" paragraph mis-indented to ~80 columns). Every `bun run fmt` run was repeatedly trying to normalize it and adding 4 spaces to the indent each time. Fixed the underlying markdown so fmt is now a fixed point.
- [`crates/moon-coder/src/runner.rs`](../../crates/moon-coder/src/runner.rs) had an inline `match report.mode { ... }` to derive the wire string; replaced with the existing `as_wire()` helper so the next variant rename touches one site instead of two.
