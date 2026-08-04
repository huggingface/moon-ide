# ADR 0055 — `git_merge_default_branch` aborts on conflict

Date: 2026-08-04
Status: accepted; implemented.

## Context

`git_merge_default_branch` was a bare `git merge --no-edit <ref>`. On a
content conflict it returned git's error and **left the working tree
stranded mid-merge** — `MERGE_HEAD` set, conflict markers written into
the files. The SCM panel's "Update from main" flow tolerates this (it
polls `git_merge_state` and shifts into merge-in-progress mode), but the
coordinator's `merge_worker_changes` tool runs the _same_ host method
against the parent repo's main checkout — and a conflict there strands
the user's shared `main` in a half-merged state with no coordinator-side
way to resolve it (the coordinator is read-only on the parent tree).

`git_pull` already solved the identical problem the other way: a
conflicted `git pull --rebase` is `rebase --abort`ed before returning so
"the tree is left exactly as it was" (host.rs). The merge path had no
equivalent guard.

## Decision

`git_merge_default_branch` now **aborts the merge before returning an
error**: if `.git/MERGE_HEAD` exists after a failed `git merge --no-edit`,
it runs `git merge --abort` (best-effort — a failed abort still surfaces
the original merge error). The contract becomes "the merge either lands
or the tree is exactly as it was", matching `git_pull`.

The error itself still propagates git's stderr verbatim (the `CONFLICT`
lines), so the caller — SCM panel or `merge_worker_changes` — learns the
merge conflicted. The difference is only that the tree is restored, so
the human resolves deliberately (terminal, or a follow-up rebase of the
worker branch) instead of inheriting a checkout stuck mid-merge.

## Consequences

- `merge_worker_changes` on a stale-base / conflicting worker branch now
  fails cleanly instead of leaving the parent's `main` half-merged. Pair
  it with [ADR 0056](0056-coordinator-stale-base-and-fleet-tools.md)'s
  `check_worker_base` and the conflict is usually _anticipated_ before
  the merge is even attempted.
- The SCM panel's "Update from main" still works: the merge either
  fast-forwards / merge-commits (success) or errors with the tree
  restored. Its merge-in-progress UI now only appears for merges started
  _outside_ this method (a terminal `git merge`), which is the case that
  UI exists for.

## Alternatives considered

- **Leave the tree in MERGE_HEAD and let the caller resolve.** Rejected
  for the coordinator path: the coordinator has no conflict-resolution
  affordance, and the user's main checkout is the worst place to strand
  a half-merge. The SCM panel is the one caller that _wanted_ the
  in-progress state, and it can still reach it via externally-started
  merges.
- **`--no-commit` + inspect + conditionally commit.** Rejected: more
  moving parts for no gain over "land or restore".

## Related

- [ADR 0056 — coordinator stale-base & fleet tools](0056-coordinator-stale-base-and-fleet-tools.md)
  — the `check_worker_base` gate that anticipates the conflict.
- [ADR 0037 — cross-project workers](0037-cross-project-workers.md) —
  where `merge_worker_changes` comes from.
