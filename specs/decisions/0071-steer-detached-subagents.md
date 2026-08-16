# ADR 0071 — `task_steer`: steering detached sub-agents

Date: 2026-08-14
Status: accepted; implemented.

## Context

A detached `task` run ([ADR 0053](0053-detached-task-subagents.md))
gives the parent a handle plus `task_collect` / `task_abort` — a
fire-and-collect surface. The coordinator's worker surface
([ADR 0030](0030-orchestrator-sessions.md)) additionally has
`steer_worker`: redirect a run mid-flight instead of choosing between
"let it drift" and "abort and respawn". Dogfooding hit the same gap
one level down: a parent kicks off a long detached audit, learns
something two turns later that changes the audit's scope, and its
only options are to kill the run (losing its progress) or let it
finish wrong.

The delivery mechanism already exists: the pop-out composer steers a
running sub-agent via a per-run steer channel
(`queue_subagent_steer`), drained at the top of the sub-agent's next
iteration. Only the user could reach it; the parent agent could not.

## Decision

Add **`task_steer(subagent_id, text)`**, advertised to `Agent` mode
alongside `task_collect` / `task_abort` (so sub-agents and
coordinators never see it).

- **Same channel as the user's steer.** The tool queues into the
  existing per-run steer channel; delivery semantics are identical
  (queued now, drained at the next iteration top, extra round-trip
  granted for late steers). No new queue kind.
- **Tagged as agent-sent.** The queued row rides the existing
  `from_coordinator` flag ([ADR 0043](0043-user-message-notifies-coordinator.md))
  through the event and the persisted record, so the sub-agent's
  transcript distinguishes "the supervising agent said this" from
  "you said this". We reuse the flag rather than mint a
  `from_parent` sibling — the semantic is the same ("sent by the
  agent managing this run, not by you") and the UIs already render
  it; the pill's "coordinator" caption is accepted imprecision.
- **Ownership mirrors `task_collect`.** Only ids the session's own
  `task({ detach: true })` calls returned are steerable. Settled
  runs return `{ status: "not_running" }` (collect the report
  instead); unknown ids error.
- **Detached-only by construction.** Synchronous runs (single or
  batch) block the parent's tool dispatch, so the parent literally
  cannot issue a `task_steer` while one is in flight. No gate needed.

## Rejected alternatives

- **A separate parent→sub-agent message queue.** Two queues into one
  loop invites ordering questions between user and parent steers for
  zero benefit; the existing channel is already multi-entry FIFO.
- **`from_parent` as a new wire flag.** A third message-provenance
  bool whose only consumer would render it identically to
  `from_coordinator`. Add it when a UI actually needs to tell the
  two apart.
- **Steer implies observe (add `task_observe` too).** The coordinator
  steers off `observe_worker` snapshots; a parent steers off what it
  _asked for_ plus the wake events. The parent's motivation is new
  information on its side, not drift detection on the sub-agent's —
  and the user can already watch the pop-out. Deferred until a real
  need surfaces.

## Related

- [ADR 0053 — detached `task` sub-agents](0053-detached-task-subagents.md)
  — the lifecycle surface this completes.
- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md)
  — `steer_worker`, the coordinator-axis analogue.
- [ADR 0043 — user messages notify the coordinator](0043-user-message-notifies-coordinator.md)
  — origin of the `from_coordinator` provenance flag reused here.
