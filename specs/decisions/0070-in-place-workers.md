# ADR 0070 — In-place workers: `spawn_worker` without a worktree

## Context

Every coordinator worker got its own git worktree on a fresh branch
(ADR 0028/0030): isolation, branch-as-deliverable. But sometimes the
coordinator legitimately wants the opposite — a quick fix on the
branch the user already has checked out, work that must build on
uncommitted state (a worktree spawn only sees committed bases), or a
follow-up agent inside an existing worker's checkout. The runtime
already half-supported the shape: after `discard_worker_worktree`, a
worker keeps running with `worktree_root = None` against the parent's
main tree, and every fleet tool except `merge_worker_changes` has a
non-worktree fallback. Only the spawn path couldn't produce it.

## Decision

- `spawn_worker` grows `worktree?: boolean` (default `true`).
  `false` mints the worker session directly against the target
  folder: no `git worktree add`, no `moon/<name>` branch, no folder
  bind, no `WorkspaceFoldersChanged`. The worker runs on whatever
  branch the folder has checked out, uncommitted changes included.
  When the target folder _is_ a worktree, the routing header is
  stamped so tools route there — "second worker in an existing
  worker's worktree" falls out of the existing `folder` arg.
- `base_branch` + `worktree: false` is refused — switching the
  shared tree's branch under the user is exactly the surprise to
  avoid.
- **The shared-tree race is accepted, explicitly.** Multiple
  in-place workers on one folder are allowed; the tool description
  tells the coordinator the tree is shared and to spawn siblings
  only when their files can't collide. ADR 0053's rejection of a
  worktree-less variant was scoped to regular agents (double-driver
  risk); a coordinator is edit-less by construction, and the opt-in
  is deliberate.
- `merge_worker_changes` refuses in-place workers (commits already
  land on the checked-out branch); `commit_worker_changes` and the
  rest of the control surface work through their existing
  non-worktree fallbacks; `discard_worker_worktree` stays the usual
  idempotent no-op.
- **New fleet discriminator.** `SubagentSpawned` (record + event)
  carries `worker: bool`; the restart-time fleet fold and the
  panel's worker-card navigation key on it. Supersedes ADR 0065's
  "`worktree_root.is_some()` already discriminates" — that stopped
  being true the moment a worker could lack a worktree. Old JSONLs
  fold to an empty fleet once (accepted, per the no-migrations
  rule).

## Rejected alternatives

- **One in-place worker per folder.** Considered as a stomp guard;
  dropped — the coordinator's explicit `worktree: false` is the
  opt-in, and parallel in-place workers on disjoint files are a
  legitimate pattern.
- **A separate `spawn_in_place` tool.** Two tools with nine shared
  arguments; a flag on the existing one keeps the fleet surface
  uniform.
- **Auto-detecting "should this be in-place".** The isolation
  trade-off is a planning decision; the model should make it
  visibly, not have it guessed.
