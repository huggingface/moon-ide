# ADR 0037 — Cross-project workers and session navigation

Date: 2026-07-15
Status: accepted

## Context

ADR 0030 landed the coordinator / worker model, but `spawn_worker`
always created the worktree off the coordinator's own folder —
hardcoded via `Some(sink.folder().to_string())`. The coordinator
system prompt listed `init_repo` and `spawn_worker` side by side
without caveat, so a model reading it would reasonably try
`init_repo("/scratch/new-service")` then `spawn_worker("build the
service")` expecting the worker to run in the new repo. It wouldn't —
the worker landed in a worktree off the coordinator's project.

A second gap: the sub-agent card under a `spawn_worker` tool row
offered "Open transcript →" (the sub-agent pop-out), but workers are
real top-level sessions in worktrees, not hidden sub-agent
transcripts. A worker in another project was unreachable from the
coordinator's panel without manually switching folders and finding it
in the sessions list.

## Decision

### `spawn_worker` gains an optional `folder` parameter

The tool definition and `handle_spawn_worker` accept an optional
`folder` (absolute host path of a bound workspace folder). When
provided, the worktree is created off that folder instead of the
coordinator's own. The handler validates the folder is bound in the
workspace and returns an actionable error pointing to `init_repo` /
`clone_repo` if the model forgot that step.

The `SubagentSpawned` event gains an optional `worktree_root` field
(absent for `task` sub-agents, set for `spawn_worker` workers) so the
frontend can distinguish the two and navigate to the worker's session.

### Worker cards navigate to the session

The sub-agent card under a `spawn_worker` tool row shows "Open
session →" when `worktree_root` is set. Clicking it calls
`CoderPanelState.openWorkerSession(worktreeRoot, sessionId)`, which
switches the folder bar to the worktree (if it isn't already active)
and opens the session by id — the same flow as clicking a row in the
sessions list.

`spawn_worker` / `observe_worker` / `steer_worker` / `abort_worker` /
`review_worker_changes` / `commit_worker_changes` /
`respond_to_worker_prompt` tool rows also gain a `toolHint` so the
collapsed row shows the worker id (or the task text for
`spawn_worker`) inline, making them recognizable without expanding.

### `init_repo` takes a name, not a path

The first cut let the model pick any absolute host path — and given a
free path it picked `/tmp`, stranding the new project outside the
user's project tree. `init_repo(name)` now creates the repo as a
**sibling of the coordinator's project folder** (same parent
directory), mirroring `clone_repo`'s no-path default. The model
chooses a directory name; the location is fixed.

### Host routing for folders the running container doesn't mount

A folder bound while the workspace shell container is already running
(`init_repo` / `clone_repo`) is not in the container's compose mount
set — container-routed subprocesses would land in a cwd that doesn't
exist, which is how "the worker can't write files" manifested. All
three shell-routing decisions (coder `bash`, format-on-save, LSP) now
check the folder's effective mount root (worktree → parent) against
`bound-folders.json` — the set the compose state was last emitted
from — and fall back to the **host** toolchain when it's missing.
Routing flips to the container automatically once the user restarts /
re-syncs it. `init_repo` / `clone_repo` / `spawn_worker` results carry
a `note` when this applies so the coordinator can tell the user.

Rejected alternative: auto-running the compose bound-folder sync from
the coder tool. `up -d` on a mount-set change **recreates the dev
container**, killing every other session's container processes
mid-turn — too disruptive as an implicit agent side effect.

### `merge_worker_changes` for local repos

ADR 0030's prompt said "you do not merge work back" — correct for repos
with a remote (the branch is the deliverable, the PR is the merge), but
wrong for a local repo the coordinator just created with `init_repo`
(no remote, no PR flow). The coordinator gains a
`merge_worker_changes(worker_id, base_branch?)` tool that switches the
parent repo to `base_branch` (default `main`) and runs
`git merge --no-edit <worker_branch>` on the parent's host. The
worker's worktree and branch are left intact — only the commits land.
The system prompt now distinguishes: leave the branch for PR when
there's a remote; `merge_worker_changes` for local repos.

## What this deliberately does not do

- **Does not change the worker's session filing.** The worker is still
  filed under the parent folder's coder root (the `folder` parameter
  controls where the worktree is created, not where the session JSONL
  lands). This keeps the coordinator's session list coherent — all
  workers it spawned are in its project's session list.
- **Does not add `folder` to the other coordinator control tools**
  (`observe_worker`, `steer_worker`, etc.). They resolve the worker
  by `worker_id` (session id) across all folders via
  `runtime_for_session`, so they already work cross-project without a
  `folder` parameter.
- **Does not remove the worktree or delete the branch after a merge.**
  `merge_worker_changes` only merges — the worktree and branch stay.
  The coordinator (or user) can clean up via the existing
  `coder_merge_and_remove_worktree` flow if needed.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the coordinator / worker model this builds on.
- [ADR 0036 — worker takeover](0036-worker-takeover.md) — the
  "user messaged a worker directly" semantics, unchanged here.
