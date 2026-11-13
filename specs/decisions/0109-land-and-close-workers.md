# ADR 0109 — `land_worker` and `close_worker`

Date: 2026-11-10
Status: accepted. Supersedes the coordinator tool surface of
[ADR 0037](0037-cross-project-workers.md) (`merge_worker_changes`),
[ADR 0044](0044-idempotent-worktree-removal.md)
(`discard_worker_worktree`), and
[ADR 0064](0064-worker-retirement.md) (`retire_worker`), plus
`commit_worker_changes` (ADR 0030).

## Context

Finishing one worker took four or five calls in a fixed order:
`commit_worker_changes` → `check_worker_base` → `merge_worker_changes`
→ `discard_worker_worktree` → `retire_worker`. Each step had its own
refusals pointing at the next, the merged branch was never deleted,
and coordinators routinely stopped halfway, leaving bound worktrees
and idle fleet rows. The old descriptions also baked in a landing
policy ("for repos with a remote, leave the branch for a PR"), which
is a per-project decision, not the tool's.

## Decision

Two coordinator tools replace the four; both take `worker_id` or
`worker_ids` and refuse a worker with a turn in flight.

- **`land_worker(base_branch?, message?)`** — merge locally and clean
  up: commit leftover changes (given or AI-suggested message), run the
  `check_worker_base` gate (a flagged stale-base revert returns
  `status: "blocked"` before merging; plain "behind" or no remote
  default does not block), switch the parent to `base_branch`
  (default: the repo's default branch, else `main`) and merge
  (conflicts abort, ADR 0055, leaving everything untouched), then
  remove the worktree, `git branch -d` the merged branch, and retire.
  An in-place worker (`in_place_worker` in its session header, set at
  spawn) is committed and retired; a worker whose worktree was removed
  earlier is refused, since its session now routes to the parent
  folder, whose uncommitted changes may be the user's. Post-merge
  cleanup failures are reported on the result, not raised.
- **`close_worker(delete_branch?, force?)`** — clean up without
  merging: remove the worktree (dirty needs `force`), optionally
  delete the branch (unmerged needs `force`, i.e. `-D`), retire.
  Idempotent on an already-gone checkout. On a user-disconnected
  worker only the coordinator's bookkeeping is dropped.

Landing policy stays out of the tools. The coordinator prompt says to
follow the project's conventions (e.g. its AGENTS.md) and ask if
unclear: land locally, have the worker push / open a PR then close it,
or leave the worktree and branch in place for the user.

`WorkspaceHost::git_delete_branch` gains a `force` flag (safe `-d`
by default).

## Rejected alternatives

- **Keep the granular tools alongside the new ones.** More surface,
  same halfway-cleanup failure mode; `review_worker_changes` and
  `workspace_scm_status` already cover inspection.
- **Force-delete the branch after landing.** `-d` after a successful
  merge always succeeds; `-D` would hide a merge that didn't land.
- **Refuse `land_worker` on repos with a remote.** Whether to merge
  locally or PR is the project's call.
