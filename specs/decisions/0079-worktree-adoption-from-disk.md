# ADR 0079 — Worktree rows adopt from disk at startup

Date: 2026-09-05
Status: accepted

## Context

A worktree's folder binding lives in the workspace's `session.json`
(`FolderOrigin::Worktree`). That file is UI state, and per the
warn-and-default policy a corrupt copy falls back to an empty session
and heals on the next save. Worktree checkouts, on the other hand,
persist on disk **locked** and are the deliverable of an isolated
session — the branch is what the user PRs.

That asymmetry produced a real incident: `session.json` for the
`hugging-face` workspace corrupted (disk-level garbage; the
write-fsync-rename guard in `moon_core::session` landed the day
after), healed to the five user-picked folders, and four
IDE-managed worktrees under `moon-landing/.worktrees/` became
unreachable — no folder-bar row, no discard affordance, nothing in
the coder panel to re-bind them. Their branches stayed pinned: git
refuses `git branch -D` / `git switch` on a branch checked out in a
worktree, so the repo was blocked until the user unlocked and pruned
the worktrees by hand.

The same hole exists whenever a checkout outlives its binding without
disk loss — the binding is created with the session, but nothing
guarantees the pair stays in sync.

## Decision

**Disk is the source of truth for worktree rows.** At workspace
startup (desktop `restore_session` and `moon-remote serve`), and on
every `workspace_open_local`, the registry sweeps each bound
non-worktree folder for worktrees git lists under
`<parent>/.worktrees/` (the IDE-managed location, ADR 0029) and binds
any that aren't bound yet, with the branch git reports. Rows
reappear; the `×` (delete, per the user's rule: removing a worktree
means deleting it) and the merge button work on them immediately.

`session.json` remains the restore path for ordinary (user-picked)
folders and for the per-folder UI state (open tabs, terminals,
forwards); the sweep is not a general "reconstruct the workspace from
disk" mechanism. For worktree rows the persisted origin stays as an
optimisation — when it's present the row is already bound and the
sweep no-ops.

Scope guards:

- Only `<parent>/.worktrees/` tails are adopted. A worktree the
  user created themselves elsewhere is never taken over — the row's
  `×` deletes, and adopting a user-made checkout would aim a
  destructive affordance at something the user owns.
- Only bound **project** folders adopt; a worktree row is never an
  adoption parent, so a nested worktree-of-worktree (ADR 0037
  cross-project workers) is not re-parented to a project it isn't a
  child of. If its project is bound it gets adopted through its own
  parent like any other.
- A checkout that git still lists but whose `.git` link is gone
  (mid-removal) is skipped, not bound as a dead row.
- Detached-HEAD worktrees are skipped: no branch to label the row or
  deliver.
- The sweep is best-effort and per-folder — a git failure (not a
  repo, no git) logs and continues. It never fails startup and never
  changes the active folder.

## Consequences

- A corrupt `session.json` now costs the user their open tabs once,
  not access to their isolated-session deliverables.
- Branches checked out in IDE-managed worktrees can't be stranded
  invisible; the "used by worktree" git refusal always has a UI path
  to resolve it.
- Startup runs one `git worktree list --porcelain` per bound repo
  (plus a stat per candidate). Negligible next to the per-folder
  status refreshes the IDE already does at startup.
- The adoption logs at info when a row reappears, so a
  resurrected-after-corruption workspace explains itself in the
  diagnostics panel.
- No persistence change: `session.json` is not rewritten by the
  sweep; the next frontend persist tick simply includes the adopted
  rows.

## Alternatives considered

- **Tombstones for deliberately-unbound worktrees.** Rejected: per
  the user's rule, removing a worktree _is_ deleting it — there is
  no "dismissed but kept on disk" state for the sweep to respect,
  so there is nothing to record. If the discard flow ever gains a
  keep-the-checkout variant, revisit this.
- **Adopt every worktree git lists, regardless of location.**
  Rejected for the same `×`-means-delete reason as the scope guard
  above.
- **Repair `session.json` from the coder-session JSONLs** (walk
  headers for `worktree_root`s). More precise than the disk sweep
  but rebuilds the folder registry from a _different_ ephemeral
  store, adds a second code path for the same result, and still
  misses worktrees whose owning session was deleted. The disk sweep
  covers both with one mechanism.
- **A repair command the user runs by hand.** Rejected — the
  incident showed the failure is silent and the workaround requires
  understanding git worktree internals; the IDE owns the state it
  created.

## Related

- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md)
- [ADR 0029 — worktrees inside the parent repo](0029-worktrees-inside-parent.md)
- [ADR 0044 — idempotent worktree removal](0044-idempotent-worktree-removal.md)
- [ADR 0063 — stale-worktree reconciliation](0063-stale-worktree-reconciliation.md)
