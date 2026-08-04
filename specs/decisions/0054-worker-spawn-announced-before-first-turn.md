# ADR 0054 — `spawn_worker` announces the worker before its first turn

Date: 2026-07-28
Status: accepted; implemented.

## Context

A coordinator-spawned worker must not steal the panel: the user is
looking at the coordinator, and the worker is a background session the
coordinator drives. The frontend guards against the jump in its
`session_loaded` handler — a `SessionLoaded` whose id is already
registered as a worker in some session bucket's `subagentSummaries`
does not rebind `visibleSessionId`. That registration comes from the
coordinator's `SubagentSpawned` event.

`handle_spawn_worker` originally seeded the worker's first turn
(`send_to`) **before** emitting `SubagentSpawned`. The first `send_to`
fires the worker's own `SessionLoaded` (its `persisted_records == 0`
announce), so the event stream went `SessionLoaded(worker)` →
`SubagentSpawned(worker)`. The guard read the `SessionLoaded` before
the worker was registered anywhere, found no `subagentSummaries`
entry, treated it as a plain open, and the panel jumped to the worker
on every spawn.

## Decision

In `handle_spawn_worker`, register the worker in
`CoordinatorRegistry`, persist the `SubagentSpawned` record into the
coordinator's JSONL, and emit the `SubagentSpawned` event **before**
calling `send_to`. The worker's `SessionLoaded` then lands after the
frontend already knows the id is a worker, and the guard holds.

If `send_to` fails, the registration is rolled back
(`CoordinatorRegistry.remove`) so the coordinator isn't left holding a
worker that never started; the already-emitted `SubagentSpawned` is
left in place (the seed error surfaces through the orchestrator's own
`tool_result`).

## Rejected alternatives

- **Make the frontend guard order-independent** — e.g. key off the
  `SessionLoaded`'s `mode` or `worktree_root`. A worker's `mode` is
  `agent` (indistinguishable from a user-opened session) and
  `worktree_root` is set for any worktree session, so neither marks a
  coordinator-spawned worker without the `SubagentSpawned` link.
- **Defer / queue the worker's `SessionLoaded` in the frontend** until
  a `SubagentSpawned` arrives. Adds reordering state to the dispatcher
  to work around a bug that is cheaper to fix at the source.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the worker model and the events-as-messages channel.
- [ADR 0052 — disconnect worker](0052-disconnect-worker-from-coordinator.md)
  — the registry whose `register` / `remove` ordering this ADR
  constrains.
