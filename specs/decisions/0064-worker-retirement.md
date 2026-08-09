# ADR 0064 — Worker retirement and idempotent worktree discard

Date: 2026-08-05
Status: accepted

## Context

Field report from a long-running coordinator session cleaning up
after ~13 landed workers:

- `discard_worker_worktree` **errored** ("worker has no
  `worktree_root`") for every worker whose checkout was already gone
  — removed by a merge flow, by the end-of-turn stale-worktree
  reconciliation ([ADR 0063](0063-stale-worktree-reconciliation.md)),
  or by an out-of-band `git worktree remove`. Both paths clear the
  session's `worktree_root`, so the later discard had nothing to
  point at and refused. ADR 0044 made the _folder-level_ removal
  idempotent; the coordinator tool predated the reconciliation and
  never got the same treatment.
- Landed workers **linger in `list_workers` forever**. The registry
  only ever removes a link when a disconnected worker's final wake
  lands; a worker that runs to completion and gets merged stays an
  idle row for the coordinator's whole lifetime, burying the live
  fleet under a graveyard.

## Decision

1. **`discard_worker_worktree` is idempotent.** No `worktree_root` on
   the header → `{ status: "already_gone" }`, success. A
   `worktree_root` whose folder is no longer bound → best-effort
   `git worktree` metadata prune (stale metadata refuses a later
   `git worktree add` at the same deterministic path), clear the
   worker's stale routing, same `already_gone` success. Only a live
   bound checkout runs the real removal path, unchanged.
2. **New coordinator tool `retire_worker(worker_id)`.** Removes the
   orchestrator → worker registry link: the worker leaves
   `list_workers`, stops waking the coordinator, and the control
   tools stop treating it as this coordinator's worker. The session,
   transcript, and branch are untouched — retirement is bookkeeping,
   not deletion. Refuses a worker with a turn in flight (abort first)
   and one whose worktree is still bound (discard first), so it can't
   strand an orphan folder the coordinator no longer has tools for.
   Disconnected workers may be retired (drops only the coordinator's
   own bookkeeping of a session the user already owns).

The registry stays in-memory (a restart empties the fleet anyway),
so retirement needs no persisted record.

## Rejected alternatives

- **Auto-retire on merge / on discard.** The coordinator often keeps
  steering a worker after its first PR lands (follow-ups, review
  fixes); implicit retirement would cut a link the model still
  relies on. Explicit retire keeps the lifecycle legible: land →
  discard worktree → retire.
- **Bulk `prune_workers`.** One call per worker is cheap for the
  model and keeps the refusal cases (running / still-bound worktree)
  per-worker instead of a partial-failure report. Revisit if fleets
  get big enough that the loop hurts.
- **Re-provisioning a worker's worktree on steer after its checkout
  was removed** (the reporter's third ask). Deferred: it needs a
  branch-state decision (re-checkout the old branch vs fork a new
  one) that the coordinator can already express today by spawning a
  fresh worker with `base_branch`.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md)
- [ADR 0044 — idempotent worktree removal](0044-idempotent-worktree-removal.md)
- [ADR 0052 — disconnect a worker from its coordinator](0052-disconnect-worker-from-coordinator.md)
- [ADR 0063 — stale worktree reconciliation](0063-stale-worktree-reconciliation.md)
