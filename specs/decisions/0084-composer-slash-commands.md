# ADR 0084 — Composer slash commands, starting with `/attach`

Date: 2026-09-18
Status: accepted; implemented.

## Context

Coordinators only acquire workers by spawning them (`spawn_worker`,
ADR 0030). Two real workflows don't fit: a session the user has been
driving solo turns out to belong in a fleet, and a worker
disconnected via ADR 0052 sometimes needs to go back. There was no
attach path — and no obvious home for its UI. A button per direction
(session bar on the worker, fleet strip on the coordinator) adds
chrome for an occasional operation, and the team asked for
command-style invocation instead.

## Decision

Two things, deliberately coupled: a minimal **composer slash-command
surface**, and **`/attach`** as its first (only) command.

- **Surface.** A single-line draft starting with `/` mounts a
  completion menu over the composer — the `@`-mention picker's
  interaction grammar (arrows / Enter / Tab / Escape, mousedown
  picks) and visual classes, derived directly from the draft rather
  than input-handler state. Commands execute on pick and never reach
  the model. A leading `/` that resolves to no known command falls
  through to a plain send (prose starting with `/` keeps working) —
  no escape syntax needed.
- **`/attach` is direction-aware.** In a coordinator's composer the
  menu lists attachable sessions of the current project (adopt); in
  an ordinary session it lists coordinators (hand this session over).
  One backend call either way: `attach_worker(coordinator_id,
worker_id)`.
- **Attach = spawn minus mint/seed.** After it, the link is
  indistinguishable from a spawned worker's: registry entry (+
  dispatch feeder on first link), `orchestrator_session_id` stamped
  into the worker header and **rewritten on disk** (an existing
  session is already persisted — the spawn path's lazy first-write
  never happens), a `SubagentSpawned { worker: true }` record in the
  coordinator's JSONL (ADR 0065 rebuild), the live event, and a
  **parked** notice (ADR 0062) carrying the worker's branch snapshot
  (ADR 0056) so the coordinator plans from the handover state at its
  next turn. No seed task: the notice says "read its state before
  dispatching".
- **Ownership rules.** A worker still _attached_ to another
  coordinator is refused — auto-stealing a live link would silently
  break the other fleet's plan; the user disconnects it there first.
  Disconnected residue (any coordinator) is purged from the registry
  before the new link lands — without the purge, a stale
  `disconnected` mark vetoes the new coordinator's control tools
  forever (`controls()` refuses ids in _any_ disconnected set).
  Coordinators can't be attached as workers (fleets don't nest, ADR
  0030's single-level posture). Either session is mounted from disk
  when needed, searching every bound folder (cross-project fleets,
  ADR 0037).
- **No transcript card.** The synthetic spawn record's
  `tool_call_id` matches no tool row, so the coordinator transcript
  shows no collapsed card — the parked notice and `list_workers` are
  the visible surface. Accepted: a card needs a tool row, and
  synthesising fake tool records for a user action would put words
  in the model's mouth.

## Rejected alternatives

- **Buttons** (session-bar picker on the worker, fleet strip on the
  coordinator). Permanent chrome for an occasional operation; the
  command menu costs nothing when unused and scales to later
  commands without new surfaces.
- **A coordinator `adopt_session` tool.** Attach is the _user's_
  decision about _their_ session; giving the model the lever invites
  it to grab sessions mid-conversation. Revisit if a concrete
  orchestration need shows up (it would also make the transcript
  card real, via a genuine tool_call_id).
- **Auto-steal from the previous coordinator.** See ownership rules.
- **Seeding the attached worker with a synthetic task.** The session
  already has a history and possibly an in-flight turn; a seed would
  fork its intent. The coordinator observes first.

## Related

- [ADR 0052 — disconnect worker](0052-disconnect-worker-from-coordinator.md)
  — the inverse operation; its registry `disconnected` residue is
  what attach purges.
- [ADR 0065 — restart-resilient worker links](0065-restart-resilient-worker-links.md)
  — the record/header pair attach must write to survive restarts.
- [ADR 0062 — parked coordinator notices](0062-parked-coordinator-notices.md)
  — the delivery shape of the attach notice.
- [ADR 0056 — stale-base and fleet tools](0056-coordinator-stale-base-and-fleet-tools.md)
  — the branch-snapshot line the notice reuses.
