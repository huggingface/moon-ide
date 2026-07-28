# ADR 0044 — Worktree removal is idempotent, and coordinators can do it themselves

Date: 2026-07-28
Status: accepted; implemented.

## Context

Three related holes in worktree removal, all surfaced by one session:
a coordinator was asked to clean up after its workers, said it had,
and the worktree rows stayed in the project bar.

1. **The coordinator had no removal tool.** Its worker tools stop at
   `commit_worker_changes` / `merge_worker_changes`, both of which
   deliberately leave the checkout in place. Asked to clean up, the
   model did the only thing it could: `git worktree remove` through
   `bash`. That deletes the checkout but the workspace registry never
   hears about it, so the folder stays bound.
2. **Removal wasn't idempotent.** With the checkout already gone,
   clicking `×` on the row ran `git worktree remove` again, which fails
   ("is not a working tree"). The UI's reaction to a failure is to
   re-confirm and retry with `--force`, which fails the same way — so
   the row was **unremovable**. The user's ask: closing a worktree that
   doesn't exist should silently succeed.
3. **Agent-driven folder changes never reached the folder bar.** Every
   UI-driven bind / unbind returns a fresh `Workspace` snapshot from
   its own Tauri command, but `spawn_worker` / `clone_repo` /
   `init_repo` mutate the registry with no round trip to piggyback on,
   so the bar showed a stale folder set until something unrelated
   triggered a snapshot.

## Decision

**Removal is idempotent.** `coder_discard_worktree` (and the new
coordinator tool) split on whether the checkout still exists on disk —
a host-side `is_dir`, valid under either shell target since worktrees
live under `<parent>/.worktrees/<slug>` and ride the parent's bind
mount (ADR 0029):

- **Checkout present** → `git worktree remove [--force]` as before, so
  a genuine refusal (dirty tree without `force`) still errors and the
  UI's force-confirm flow still means something.
- **Checkout gone** → `WorkspaceHost::git_worktree_forget`: unlock the
  entry (IDE worktrees are locked at creation and `git worktree prune`
  skips locked entries) then `git worktree prune`, best-effort. The
  folder unbinds either way. Pruning matters beyond tidiness: stale
  metadata refuses a later `git worktree add` at the same path, which
  ADR 0042's deterministic names make likely.

**Coordinators get `discard_worker_worktree(worker_id, force?)`** —
remove a worker's checkout, unbind its folder, keep the branch. It
refuses a worker with a turn in flight (abort first) and refuses a
dirty worktree without `force`, and it clears the worktree routing on
sessions that pointed there so they fall back to the parent project.
The system prompt tells the coordinator to use it once work is landed
and **never** to remove a worktree with `bash`.

**A new `CoderEvent::WorkspaceFoldersChanged`** is emitted by every
coordinator tool that binds or unbinds a folder (`spawn_worker`,
`clone_repo`, `init_repo`, `discard_worker_worktree`). The frontend
re-fetches `workspace_active` and adopts the snapshot. No payload — the
snapshot is the payload, and the change isn't folder-scoped.

## Rejected alternatives

- **Self-heal in `WorkspaceRegistry::snapshot`** (drop folders whose
  directory is gone). Puts filesystem stats on a hot, frequently-read
  path, and silently unbinds folders on a transient mount hiccup.
  Startup restore already skips vanished folders; that plus idempotent
  removal covers the real cases.
- **Have the coordinator call `coder_discard_worktree`.** It's a Tauri
  command; agents drive the in-process `CoderHandle` surface (ADR
  0030). Sharing the small git decision as a helper on each side is
  cheaper than routing an agent through the command layer.
- **Swallow the `git worktree remove` error text instead of testing for
  the checkout.** Pattern-matching git's stderr would also swallow real
  refusals; the existence test is precise.
- **Push the whole `Workspace` snapshot in the event.** The frontend
  already has a snapshot-fetch path and adopts by value; duplicating
  the shape into the coder event stream would give us two wire copies
  of the same thing to keep in sync.
- **Let the coordinator delete the branch too.** The branch is the
  deliverable (ADR 0028); the IDE never deletes one outside the
  explicit merge-and-remove flow, and an agent shouldn't either.

## Related

- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md)
  — creation / lifecycle and "the branch is never deleted".
- [ADR 0029 — worktrees inside the parent](0029-worktrees-inside-parent.md)
  — why a host-side path test is valid in container mode.
- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the coordinator tool surface this extends.
- [ADR 0042 — named worker branches](0042-named-worker-branches.md) —
  deterministic worktree paths, which make stale metadata a real
  collision risk rather than a curiosity.
