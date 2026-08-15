# ADR 0068 — Half-gone worktree checkouts are force-discardable

Date: 2026-08-13
Status: accepted; implemented. Extends
[ADR 0044](0044-idempotent-worktree-removal.md).

## Context

ADR 0044 made worktree removal idempotent by splitting on "does the
checkout directory still exist". That test misses a third state
observed in the wild: `git worktree remove` run out-of-band (an agent
in `bash`, or a force-remove whose recursive delete partially failed)
strips the git metadata and the `.git` link but leaves ignored files
behind — `node_modules`, generated build output. The directory still
exists, so the discard flow classifies the checkout as live and runs
`git worktree remove`, which fails "is not a working tree" **with or
without `--force`**. The UI's force re-confirm retries into the same
error, so the folder row is permanently unremovable — the exact bug
class ADR 0044 fixed, one notch over.

## Decision

Liveness is now "is this still a git worktree" — does `<path>/.git`
exist — not "does the directory exist", classified by
`moon_core::worktree::checkout_state` into live / gone / stale
leftovers. One shared helper (`discard_checkout`, called by both the
`coder_discard_worktree` command and the coordinator's
`discard_worker_worktree` tool) acts on it:

- **Live** → `git worktree remove [--force]`, as before.
- **Gone** (directory missing or empty) → forget metadata,
  best-effort; reap an empty husk directory. Never errors.
- **Stale leftovers** → refused without `force` — same posture as
  ADR 0063's rejected auto-force: the leftovers _might_ be files the
  user wants, so deleting them stays behind the same re-confirm as a
  dirty tree. With `force`, forget the metadata and
  `remove_dir_all` the directory.

Turn-end reconciliation and startup restore (ADR 0063) deliberately
keep treating stale-leftover checkouts as **present**: auto-unbinding
would either silently delete files or hide the row while junk
accumulates in `.worktrees/`. The row stays visible until an explicit
discard, which now works.

## Rejected alternatives

- **Auto-force when only ignored files remain.** Would need a
  gitignore evaluation against a tree git no longer tracks, and
  ADR 0063 already rejected silently discarding files.
- **Unbind without deleting the leftovers.** Leaves growing junk in
  `.worktrees/` and a non-empty directory that refuses a later
  `git worktree add` at the same deterministic path (ADR 0042).
- **Pattern-match git's "is not a working tree" stderr.** Same
  reasoning as ADR 0044: the on-disk test is precise, stderr is not
  (and is locale-dependent).

## Related

- [ADR 0044](0044-idempotent-worktree-removal.md) — idempotent
  removal; this refines its liveness test.
- [ADR 0063](0063-stale-worktree-reconciliation.md) — reconciliation,
  and the no-silent-discard posture the force gate preserves.
- [ADR 0029](0029-worktrees-inside-parent.md) — why host-side path
  tests are valid under either shell target.
