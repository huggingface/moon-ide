# Test plan 0061: Unify sub-agent iteration cap with the parent

- **Date**: 2026-05-07
- **Phase**: 6.x (coder polish) — small but observable behaviour change: sub-agents now share the parent's 200-roundtrip ceiling instead of being throttled at 50.

## What shipped

- Removed `SUBAGENT_MAX_ITERATIONS` (was `50`) from `crates/moon-coder/src/subagent.rs`. Sub-agents now use `MAX_TURN_ITERATIONS` (`200`, the same constant the parent loop uses) for both the main loop and the wrap-up sentinel message.
- Rationale: the original 50-cap assumed sub-agents would only ever run small scoped tasks. In practice the team delegates real refactors (multi-file edits, propagating an API change across a client library, etc.) and the tighter cap was bailing mid-flight where the parent would have kept going. With auto-compaction backstopping context growth, there's no reason to throttle the sub-agent harder than its parent.
- Updated `specs/coder.md`'s "Budget" section to point at `MAX_TURN_ITERATIONS`. Older test plan `0054` keeps its historical reference to the now-defunct constant — per the test-plans append-only rule, plans are snapshots of "what was true at commit X", not living docs.
- No protocol or schema change. The sub-agent report's `iterations_used` field still exists; it just reflects 200 instead of 50 when the cap is actually hit.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, an active folder bound, and a parent agent session ready to spawn sub-agents.

### Default behaviour (cap not hit)

1. From a parent session, spawn a sub-agent with a normally-sized task (e.g. "summarise the structure of `crates/moon-protocol`"). It should finish in well under 50 roundtrips and produce a final answer.
2. Expected: nothing visibly different from before. The sub-agent pop-out renders turns, ends with a final assistant message, and the parent's transcript shows the sub-agent's `result` summary inline. No cap-hit banner.

### Cap-hit wrap-up still works

3. Temporarily lower `MAX_TURN_ITERATIONS` in `crates/moon-coder/src/defaults.rs` to `3`. Rebuild (`cargo build -p moon-desktop`).
4. Spawn a sub-agent with a task that will keep it in tool-loop territory (e.g. "list every file in the folder and read the first three"). Open the sub-agent pop-out so you can watch the turns stream.
5. After the third tool-roundtrip, expected:
   - A new user message lands in the sub-agent transcript reading `[Tool-call budget exhausted: you've used all 3 tool-call iterations available for this sub-agent. …]`.
   - The next assistant message is a tools-disabled wrap-up summarising what was found.
   - The parent's transcript shows the sub-agent's `result` starting with `[Sub-agent reached the 3-iteration cap; final wrap-up follows.]\n\n…`.
6. Revert `MAX_TURN_ITERATIONS` to `200` and rebuild.

### Crossover sanity check

7. Confirm: temporarily set `MAX_TURN_ITERATIONS` to `1`. Spawn a sub-agent. Expected: the sub-agent's first reply is forced into the wrap-up branch on the very next iteration; the cap message reads `available for this sub-agent`. (Same constant on parent and child means tweaking it for testing flips both — that's the point.) Revert.

### Regression: parent loop unchanged

8. Run a normal parent prompt that involves several tool-roundtrips (e.g. "find all uses of `MAX_TURN_ITERATIONS` and tell me what they do"). It should finish well under 200 roundtrips. The status-bar context ring still renders. No behavioural regressions in the parent loop.

## What must keep working

- Auto-compaction inside sub-agents — uses the same `compact_if_needed` call against the same threshold. No change here.
- Sub-agent persistence — JSONL still lands under `<XDG_DATA_HOME>/moon-ide/coder-sessions/<parent-folder-slug>/<parent-session-id>/<sub-id>.jsonl`. The cap change has no effect on the persistence path.
- Sub-agent depth cap of `1` — still enforced by tool-list filtering (`spawn_subagent` is omitted from the sub-agent's own tool definitions). Constant rename doesn't touch that path.
- The `iterations_used` field on `SubagentReport` is still emitted, just with a different ceiling. Anything downstream that reads it (currently only the spec's wire-shape doc) keeps working.

## Known limitations

- A truly runaway sub-agent now eats up to 4× more inference cost before the cap fires. We accept that — auto-compaction means the wall-clock and token costs flatten well before the iteration ceiling matters, and a misbehaving sub-agent is more often a logic bug than a budget bug.
- We did not lift the depth cap (sub-agents still cannot spawn sub-sub-agents). That's a separate decision driven by the sub-agent tool list, not by the iteration cap.

## Related

- Specs: [coder.md](../coder.md) — Budget section, updated.
- Prior test plans: [0054-token-usage-and-auto-compaction.md](0054-token-usage-and-auto-compaction.md) (where the now-removed `SUBAGENT_MAX_ITERATIONS = 50` originally shipped), [0058-cross-folder-routing-and-drop-tiers.md](0058-cross-folder-routing-and-drop-tiers.md) (the previous round of "stop treating sub-agents as second-class" simplifications).
