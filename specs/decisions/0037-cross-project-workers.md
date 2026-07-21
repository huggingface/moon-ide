# ADR 0037 — Cross-project workers, no UI hijack, session navigation

Date: 2026-07-15
Status: accepted

## Context

ADR 0030 landed the coordinator / worker model with three gaps that
surfaced once the `init_repo` / `clone_repo` tools were available:

1. **`spawn_worker` could not target a different project.** It always
   created the worktree off the coordinator's own folder, hardcoded
   via `Some(sink.folder().to_string())` in `handle_spawn_worker`. The
   coordinator system prompt listed `init_repo` and `spawn_worker`
   side by side without caveat, so a model reading it would reasonably
   try `init_repo("/scratch/new-service")` then
   `spawn_worker("build the service")` expecting the worker to run in
   the new repo — it wouldn't. The worker landed in a worktree off the
   coordinator's project.

2. **The UI hijacked the visible session when a worker was spawned.**
   `handle_spawn_worker` seeds the worker via `send_to`, which emits
   `SessionLoaded` when the worker's first message lands. The
   frontend's `session_loaded` handler unconditionally set
   `folder.visibleSessionId = event.id`, switching the panel to the
   worker — even though the user was looking at the coordinator.

3. **No way to navigate from a `spawn_worker` / `observe_worker` tool
   row to the worker's session.** The sub-agent card's "Open
   transcript" button opened the sub-agent pop-out view, which doesn't
   exist for workers (they're real top-level sessions in worktrees, not
   hidden sub-agent transcripts). A worker in another project was
   unreachable from the coordinator's panel without manually switching
   folders and finding it in the sessions list.

## Decision

### `spawn_worker` gains an optional `folder` parameter

The tool definition and `handle_spawn_worker` accept an optional
`folder` (absolute host path of a bound workspace folder). When
provided, the worktree is created off that folder instead of the
coordinator's own. The handler validates the folder is bound in the
workspace — a folder that isn't bound can't host a worktree — and
returns an actionable error pointing to `init_repo` / `clone_repo` if
the model forgot that step.

The `SubagentSpawned` event gains an optional `worktree_root` field
(absent for `task` sub-agents, set for `spawn_worker` workers) so the
frontend can distinguish the two and navigate to the worker's session.

### The UI stays on the coordinator when a worker is spawned

The frontend's `session_loaded` handler now checks whether the
session id matches a known worker (by scanning the folder's session
buckets' `subagentSummaries` for a matching `id`). If it does, the
handler skips setting `folder.visibleSessionId` and
`folder.view = 'session'` — the coordinator stays visible, and the
worker's events land in its own session bucket in the background.

### Worker cards navigate to the session

The sub-agent card under a `spawn_worker` tool row now shows "Open
session →" (instead of "Open transcript →") when
`worktreeRoot` is set. Clicking it calls
`CoderPanelState.openWorkerSession(worktreeRoot, sessionId)`, which
switches the folder bar to the worktree (if it isn't already active)
and opens the session by id — the same flow as clicking a row in the
sessions list.

`spawn_worker` / `observe_worker` / `steer_worker` / `abort_worker` /
`review_worker_changes` / `commit_worker_changes` /
`respond_to_worker_prompt` tool rows also gain a `toolHint` so the
collapsed row shows the worker id (or the task text for
`spawn_worker`) inline, making them recognizable without expanding.

## What this deliberately does not do

- **Does not change the worker's session filing.** The worker is still
  filed under the parent folder's coder root (the `folder` parameter
  controls where the worktree is created, not where the session JSONL
  lands). This keeps the coordinator's session list coherent — all
  workers it spawned are in its project's session list.
- **Does not change `clone_repo` / `init_repo`'s active-folder
  behavior.** Those tools use `add_folder` (which sets the folder
  active) — but since the coordinator can't edit files, the active
  folder flip is harmless (the coordinator doesn't operate on the
  active folder's visible session).
- **Does not add `folder` to the other coordinator control tools**
  (`observe_worker`, `steer_worker`, etc.). They resolve the worker
  by `worker_id` (session id) across all folders via
  `runtime_for_session`, so they already work cross-project without a
  `folder` parameter.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the coordinator / worker model this builds on.
- [ADR 0036 — worker takeover](0036-worker-takeover.md) — the
  "user messaged a worker directly" semantics, unchanged here.
