# 0065 — Restart-resilient coordinator↔worker links

## Context

The orchestrator→worker registry (`CoordinatorRegistry`, ADR 0030) and
the dispatch feeder were in-memory only, on the theory that "neither
the feeder task nor a background turn survives a process restart, so a
restarted coordinator has no live workers anyway". Field use proved
the theory wrong about everything _except_ the turns: a `moon-remote`
redeploy mid-fleet severed all links — worker `TurnComplete` wakes
stopped reaching the coordinator, user messages into workers stopped
producing notices (ADR 0043), `list_workers` reported an empty fleet,
and unmounted workers weren't reachable by the coordinator's control
tools at all. Every deploy of a headless IDE hit this.

## Decision

Persist both halves of the link and rebuild lazily on remount:

- **Worker side**: `SessionHeader.orchestrator_session_id`, stamped at
  `spawn_worker` before the seed send persists the header.
- **Coordinator side**: the fleet is a fold of its own JSONL — a
  `SubagentSpawned` record carrying a `worktree_root` is a worker
  (`task` sub-agents never have one); a new `WorkerDetached { worker_id }`
  record removes it. Detaches are appended on explicit disconnect
  (ADR 0052), `retire_worker` (ADR 0064), and spawn-seed rollback.
- **Rebuild** happens on the coordinator's cold remount: re-register
  the folded fleet, respawn the dispatch feeder, then quietly remount
  surviving workers in the background (observe-mode, no focus steal);
  a worker whose JSONL is gone is unregistered instead of becoming a
  ghost. A user message into a worker whose registry link is missing
  falls back to the worker's header field and quietly remounts the
  coordinator first — which runs the same rebuild.

## Rejected alternatives

- **Persisting the registry itself** (a sidecar file): a second source
  of truth that can drift from the JSONLs; the records already tell
  the whole story.
- **Eager rebuild at startup** (scan all session headers when a folder
  binds): pays a full-directory scan on every bind for state that's
  only needed when a coordinator actually remounts; lazy rebuild costs
  nothing until then.
- **A `worker: bool` flag on `SubagentSpawned`**: redundant —
  `worktree_root.is_some()` already discriminates, including for all
  records persisted before this ADR (yesterday's fleets rebuild too).
