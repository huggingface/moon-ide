# ADR 0063 — Stale worktree folders are pruned by reconciliation, not trusted from state

Date: 2026-08-04
Status: accepted; implemented. Extends
[ADR 0044](0044-idempotent-worktree-removal.md).

## Context

The workspace registry (persisted in the per-workspace session state)
is the project bar's source of truth for bound worktree folders, but
nothing guaranteed it matched the disk. A `git worktree remove` that
bypasses the discard flows — a coordinator reaching for `bash`
despite the prompt telling it not to (likely after a restart emptied
the in-memory worker registry and `discard_worker_worktree` refused
the id), or the user in a terminal — deletes the checkout without
unbinding the folder. The row stays in the project bar until the user
dismisses it by hand (idempotent since ADR 0044, but still manual).
Restore was worse: a checkout removed while the IDE was closed
re-bound as a dead row on startup.

## Decision

Reconcile the registry against the disk at the moments staleness can
appear or be observed:

- **Turn end.** After every coder turn (any exit path), stat each
  bound worktree folder's checkout; for each missing one, forget the
  stale git metadata (best-effort, ADR 0044's rationale — stale
  metadata refuses a later `git worktree add` at the same
  deterministic path), unbind the folder, clear the worktree routing
  on sessions that pointed there, and emit
  `WorkspaceFoldersChanged`. This makes agent-driven `bash` removal
  harmless: the row disappears when the turn ends.
- **Startup restore.** A persisted worktree folder whose checkout is
  gone from disk is skipped (like the orphan-parent case) and its git
  metadata forgotten, instead of re-binding a dead row.

A checkout removed in a terminal while the IDE sits idle still shows
until the next turn ends, the folder is re-fetched, or the user
clicks `×` (which ADR 0044 made succeed silently). Cost: one stat per
bound worktree folder per turn.

## Rejected alternatives

- **Watch `.worktrees/` via the fs watcher.** The watcher covers only
  the active folder, `.worktrees/` is deliberately git-excluded (and
  the walk is gitignore-aware), and wiring folder-unbind side effects
  into the watch pipeline couples two subsystems for a case turn-end
  reconciliation already covers.
- **Derive worktree rows from `git worktree list` instead of the
  registry.** The registry entry is what sessions route through
  (host, shell target, LSP brokers); deriving rows from disk would
  also surface worktrees the IDE never created and still needs a
  bound-folder entry the moment a session routes there.
- **Auto-`--force` removal when only ignored/untracked files remain
  (e.g. a copied `.env`).** `discard_worker_worktree` already takes
  `force` and its description tells the model when to use it;
  silently discarding files the user may want is worse than a refusal
  the coordinator can escalate.

## Related

- [ADR 0044](0044-idempotent-worktree-removal.md) — idempotent
  removal and the coordinator discard tool this extends.
- [ADR 0029](0029-worktrees-inside-parent.md) — why a host-side stat
  is a valid liveness test under either shell target.
