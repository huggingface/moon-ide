# ADR 0081 — Nested research sub-agents: `agent`-mode sub-agents may delegate

Date: 2026-09-14
Status: accepted; implemented.

## Context

The depth-1 cap ([`specs/coder.md` § Sub-agents](../coder.md#sub-agents))
meant a sub-agent had no delegation lever at all: an `agent`-mode
sub-agent handed a meaty refactor ("port this client to the new
endpoints") had to do every grep-then-read sweep inline, polluting the
very context whose isolation motivated delegating to it in the first
place. The parent enjoys `task` for exactly this; one level down the
same need exists and the cap was the only thing in the way.

## Decision

`agent`-mode sub-agents advertise a **trimmed `task`**; `research`
sub-agents still see nothing, so the tree bottoms out at depth 2.

- **Research-only, synchronous-only.** The nested schema is `task` +
  optional `folder` — no `mode`, no `detach`, no `system_prompt`.
  Passing `mode: "agent"` or `detach: true` errors with an actionable
  message rather than being coerced: a model that assumed the parent's
  full surface should learn the constraint, not silently get different
  semantics.
- **Sequential dispatch.** The sub-agent loop has no
  homogeneous-batch path; nested calls run one at a time. Parallel
  nested fan-out waits for a real need (and for an answer to
  concurrent same-file appends into the outer's JSONL).
- **Same wire shape as depth-1.** The nested run emits a top-level
  `SubagentSpawned` / `SubagentFinished` and single-wrapped
  `SubagentEvent`s on the same parent-session sink — never
  double-wrapped (the frontend reducer drops inner `subagent_event`s).
  Both frontends' reducers are id-keyed and order-tolerant, so the
  only UI change is rendering the collapsed card on tool rows inside
  the pop-out (previously gated to the parent transcript) plus a
  small view stack so "← Back" unwinds nested pop-outs level by
  level.
- **Flat persistence under the top-level session.** The nested spec
  reuses the outer's `parent_session_id` and `parent_folder`, so its
  JSONL lands flat in the existing
  `<sessions-dir>/<slug>/<parent-session-id>/` directory and
  parent-deletion cleanup covers it untouched. The spawn/finish
  _records_ go into the **outer sub-agent's** JSONL — that's where
  the spawning `task` tool call lives, so replay reconstructs the
  card at the right spot; `replay_subagent_spawned` recurses one
  level through them (boxed future, bounded by the depth cap).
- **Cancellation cascades.** The nested token is a child of the
  outer's, mirroring the parent-turn → sub-agent relationship one
  level up.

## Rejected alternatives

- **Full `task` surface at depth 2 (agent mode, detach).** Nested
  `agent` mode invites unbounded write-capable trees for no observed
  need; `detach` requires the detached registry, finish feeder, and
  session runtime, all of which live on `CoderState` — unreachable
  (by design) from the sub-agent loop.
- **Persist nested spawn records in the top-level parent's JSONL.**
  Zero replay changes, but the records would sit next to a transcript
  that doesn't contain the spawning tool call, and interleaving order
  against the outer's records would be lost. The replay recursion is
  ~30 lines.
- **Unlimited depth.** Nothing asks for depth 3; the tool-list-shape
  enforcement stays trivially auditable with exactly two shapes
  (full parent `task`, trimmed nested `task`).

## Related

- [ADR 0053 — detached `task` sub-agents](0053-detached-task-subagents.md)
  — the detach surface that stays parent-only.
- [`specs/coder.md` § Sub-agents](../coder.md#sub-agents) — depth-1
  language replaced by § Nested sub-agents.
