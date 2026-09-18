# ADR 0082 — Sub-agents and workers inherit the host-mode override

Date: 2026-09-15
Status: accepted; implemented. Refines [ADR 0022](0022-coder-host-mode-override.md)'s
"fresh session always starts Auto" for coordinator-spawned workers.

## Context

The per-session force-host toggle (ADR 0022, read live at dispatch
time per ADR 0041) stopped at the session boundary: sub-agents always
ran with auto bash routing ("a forced-host parent session doesn't
leak its override into delegated work"), and `spawn_worker` minted
workers in the fresh-session Auto default. In practice the opposite
is what's wanted: the user flips force-host precisely because the
container is broken, stopped, or wrong for the work at hand — and a
delegating session then fans that work out to sub-agents (or a
coordinator to workers) whose commands land back in the environment
the user just routed away from. The override is a statement about the
_work_, not about one session's own tool calls.

## Decision

- **Sub-agents share the parent's live flag.** The `Subagent` spec
  carries the parent runtime's `force_host_bash` `Arc` into the
  sub-agent's `ToolContext` — the same handle the parent's own
  dispatch reads, so a mid-run toggle re-routes the sub-agent's next
  command exactly like the parent's (ADR 0041, one level down).
  Nested research sub-agents (ADR 0081) and detached runs ride the
  same handle. A user-resumed sub-agent re-resolves the flag from the
  parent's mounted runtime; an unmounted parent has no live toggle to
  follow, so the resume falls back to auto.
- **Not persisted in sub-agent headers.** `bash_target_override` in
  the JSONL header persists a _top-level_ session's own toggle;
  sub-agent inheritance is runtime state. Writing it would create a
  second source of truth that goes stale the moment the parent
  toggles.
- **Workers snapshot at spawn.** `spawn_worker` reads the
  coordinator's live flag and, when force-host, stamps the worker's
  header `bash_target_override` (riding the existing pre-seed header
  write, ADR 0065's pattern) plus its runtime mirror. A snapshot, not
  a live link: a worker is a top-level session with its own toggle
  UI, and live-linking would put two drivers on one setting
  (ADR 0036's double-driver problem, on the config axis).

## Rejected alternatives

- **Keep the no-leak rule.** The original reasoning ("delegated work
  shouldn't silently run somewhere unexpected") had it backwards:
  auto-routing _is_ the unexpected place once the user forced host —
  the parent's report then mixes environments per nesting level.
- **Snapshot for sub-agents too.** Cheaper to reason about, but
  ADR 0041 already established that host-mode reads are live within a
  session's work; a sub-agent is that session's work.
- **Live-link workers to the coordinator's toggle.** Rejected above —
  workers outlive coordinator turns, appear in the session list, and
  own their toggle.

## Related

- [ADR 0022 — coder host-mode override](0022-coder-host-mode-override.md)
  — the per-session toggle; its fresh-session-Auto default is refined
  here for coordinator-spawned workers.
- [ADR 0041 — live host-mode toggle](0041-live-host-mode-toggle.md)
  — the read-at-dispatch-time semantics the shared flag extends.
- [ADR 0081 — nested research sub-agents](0081-nested-research-subagents.md)
  — nesting rides the same shared handle.
