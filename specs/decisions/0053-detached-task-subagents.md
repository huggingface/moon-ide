# ADR 0053 — Detached `task` sub-agents: async delegation for regular agents

Date: 2026-08-03
Status: accepted; implemented.

## Context

The coder has two delegation primitives with a hard wall between
them:

- **`task`** ([`specs/coder.md` § Sub-agents](../coder.md#sub-agents))
  dispatches a sub-agent that runs to completion and returns one
  string. The parent's tool call **blocks on it** — even the
  homogeneous-batch parallel path (`dispatch_subagent_batch`)
  `join_all`s before the parent's loop advances. "Background
  detached sub-agents" is explicitly
  [out of scope](../coder.md#out-of-scope-explicitly).
- **`spawn_worker`** ([ADR 0030](0030-orchestrator-sessions.md))
  returns a handle immediately and the worker keeps running; the
  coordinator is woken by a dispatch feeder on `TurnComplete` and
  drives the worker via `observe_worker` / `steer_worker` /
  `abort_worker`. This whole async surface is gated to the
  `Coordinator` top-level mode by tool-list shape.

In dogfooding, regular `agent` sessions keep wanting the
coordinator's shape on the delegation axis: fire off a slow,
independent piece of work ("run the full workspace test suite and
summarise the failures", "audit the sibling folder's auth module")
and **keep working** while it runs, rather than park the whole
turn behind a blocking `task` call. The foreground `task` is the
right default for "the answer is my next input"; it's the wrong
shape for "kick this off, I'll collect it when it's done."

## Decision

`task` gains an optional `detach: bool` (default `false`). A
detached call **returns a handle immediately**; the sub-agent runs
in the background and the parent collects the report later.

- **Wire shape.** `task({ ..., detach: true })` returns
  `{ detached: true, subagent_id, status: "running" }` as its tool
  result and the turn continues. Two companion tools —
  `task_collect(subagent_id, wait_ms?)` and
  `task_abort(subagent_id)` — are advertised to `Agent` mode
  alongside `task`. Synchronous `task` is unchanged.
- **The finish wakes the parent.** A per-parent feeder task
  watches the event broadcast for `SubagentFinished` from that
  parent's detached sub-agents and injects a short steer-style
  user message ("Detached sub-agent `sub-…` finished (status:
  done). Call `task_collect(\"sub-…\")` …"), waking the parent's
  LLM loop. Same events-as-messages pattern as the coordinator's
  dispatch feeder ([ADR 0030](0030-orchestrator-sessions.md) §a),
  but keyed to sub-agent ids instead of worker session ids, and
  deliberately thinner: a `research` sub-agent that silently
  failed would otherwise never be reported.
- **`task_collect` returns the report.** `{ status: "done",
result, tokens_used_estimate, iterations_used }` /
  `{ status: "error", error }` once settled, `{ status:
"running" }` while in flight. `wait_ms` (capped at 60 s) blocks
  until the run settles or the cap elapses, so the model can
  park-and-collect without busy-polling — the same shape as
  `read_process` ([ADR 0034](0034-detached-background-processes.md)).
  The report is cached after the run settles, so a second collect
  returns instantly. `task_abort` cancels the run's own token —
  scoped to the one sub-agent, never the parent turn or its
  siblings.
- **Registry, not persistence.** Detached handles live in an
  in-memory registry on `CoderState` (same pattern as
  `coordinator_workers`). A process restart loses the live runs
  exactly the way a coordinator's workers are lost; the parent
  transcript keeps the `detached: true` tool result and the
  sub-agent's JSONL on disk, and `task_collect` on a lost id
  returns a "no longer running; transcript on disk" error rather
  than hanging.
- **Parent abort cascades; turn end does not.** A detached run
  uses a **fresh root** cancellation token, not a child of the
  spawning turn's — the run must outlive the turn that launched
  it. The _user-level_ abort (Esc) walks the parent session's
  detached set and cancels each run's own token, so "stop
  everything" still stops everything. Because a detached run
  survives its spawning turn, the turn-end format-on-save flush
  can't cover its writes — each detached sub-agent flushes its own
  format queue when it settles, the same as a user-resumed
  sub-agent already does.
- **Depth-1 cap is untouched.** Detached sub-agents are spawned
  via the same `task` tool, from the same `Agent`-mode parents.
  Sub-agents still don't see `task` in their tool list, so a
  detached sub-agent cannot spawn sub-sub-agents.
- **Any mode may detach.** `research` and `agent` alike. Two
  detached `agent` sub-agents pointed at the same folder race the
  same way two foreground parallel ones already do today — no new
  hazard. Users who want guaranteed isolation spawn workers in
  worktrees (or run the session itself in a worktree) instead;
  making isolation the default is the rejected `spawn_worker`-lite
  shape below.

## Rejected alternatives

- **Give regular agents `spawn_worker` (or a worktree-less variant).**
  This was the obvious "let agents do what coordinators do" path.
  Rejected: workers are _top-level sessions_ with worktree
  machinery, a branch deliverable, a session-list row, and the
  whole steer/commit/merge/discard control surface. Bolting that
  onto a session that's already doing its own edits invites the
  double-driver problem ([ADR 0036](0036-worker-takeover.md)) on
  the delegation axis, and a worktree-less variant just re-creates
  the race this ADR accepts on purpose. The delegation axis needs
  a background _sub-agent_, not a peer.
- **Per-turn detached registry (runs die at turn end).** The
  `bash`-detach registry is per-turn; making `task`'s per-turn too
  would be more consistent — and useless. The entire point of
  detach is that the sub-agent outlives the spawning turn so the
  parent can keep working (or end the turn and be woken later).
- **Notify only on `TurnComplete` of the parent's own session.**
  Cheaper than a feeder, but a detached run that finishes _while
  the parent is idle_ (the common case for a long background
  audit) would never surface until the user happened to send
  another message. The feeder wake is what makes detach
  fire-and-forget instead of fire-and-poll.
- **Surface results only via `task_collect`, no wake.** Symmetric
  with `read_process`, but the model can't know _when_ to collect
  without polling, and a parked `wait_ms` still burns a turn. The
  wake tells the parent the moment the report exists.
- **Auto-collect the report into the wake message.** The report is
  the sub-agent's one string — potentially long. Keeping the wake
  a pointer and letting the parent `task_collect` when it wants
  the content preserves the context-preservation property that
  motivates `task` in the first place.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md)
  — the async worker/feeder pattern this adapts to the delegation
  axis; the feeder here is its thinner sub-agent-keyed analogue.
- [ADR 0034 — detached background processes](0034-detached-background-processes.md)
  — the `detach: bool` + `wait_ms`-poll + per-id abort shape this
  mirrors for sub-agents, lifted from per-turn to per-parent.
- [`specs/coder.md` § Sub-agents](../coder.md#sub-agents) — the
  layer this extends; the "background detached sub-agents are out
  of scope" line is removed there.
