# Test plan 0099: worktrees inside the parent repo (relative links)

- **Date**: 2026-06-04
- **Phase**: Phase 6 follow-on (worktree sessions — container rework)

## What shipped

Reworks how worktree-backed coder sessions ([ADR 0028](../decisions/0028-coder-worktree-sessions.md))
live on disk and in the dev container, per
[ADR 0029](../decisions/0029-worktrees-inside-parent.md).

- Worktrees now live **inside the parent repo** at
  `<parent>/.worktrees/<branch-slug>`, created with `git worktree add
--relative-paths`, instead of out-of-repo under the state dir.
- The relative git links resolve on the host **and** in the container
  (the worktree rides the parent's bind mount), so the shared
  `/workspace/.worktrees` mount and all `git worktree repair` machinery
  are **deleted**, and host git keeps working when the container is down.
- `/.worktrees/` is added to the parent's `.git/info/exclude` so it
  never dirties the parent's `git status`.
- Worktree creation version-gates on **git >= 2.48** (host) with an
  actionable error; **moon-base installs a prebuilt git** from the
  git-core PPA (jammy build, compatible with bookworm's glibc) so the
  container git understands the `extensions.relativeWorktrees` repo
  config that `--relative-paths` sets.

## How to test

Prerequisite: host git >= 2.48 (`git --version`) and a rebuilt
`moon-base` (`git --version` inside the container must be >= 2.48).

1. **Create an isolated session (host mode).** In a git repo folder,
   start a new worktree session. Confirm a nested folder row appears
   and a checkout exists at `<repo>/.worktrees/moon-agent-<id>`.
   - `cat <repo>/.worktrees/<slug>/.git` → a **relative** `gitdir:
../../.git/worktrees/<id>` (not an absolute path).
   - `git -C <repo> status` → clean (no `?? .worktrees/`); the exclude
     line `/.worktrees/` is present in `<repo>/.git/info/exclude`.
2. **Container round-trip.** With the workspace running in a container,
   open a terminal in the worktree folder and the coder `bash` tool:
   both land in `/workspace/<parent>/.worktrees/<slug>`, on the right
   branch, and `cargo build` / `pnpm install` use the **container**
   toolchain. Make a commit from the container; confirm it's visible
   from the host (`git -C <repo>/.worktrees/<slug> log`).
3. **Container down.** Stop the dev container. The worktree folder's
   SCM view, diff, blame, and branch label still work (host git reads
   the relative links directly) — this is the regression that the old
   repair-based design failed.
4. **Old git errors clearly.** On a host with git < 2.48 (or temporarily
   shadowing `git` with an old build), creating a worktree session
   returns "Isolated worktree sessions need git 2.48 or newer …" and
   makes no partial checkout.
5. **Discard.** Remove the worktree folder; `git worktree remove` runs
   against the parent (force-confirm on a dirty tree). The branch
   survives; `<repo>/.worktrees/<slug>` is gone.

## What must keep working

- The full ADR 0028 surface unchanged by this rework: per-project
  session list, the branch chip, move-into-worktree, restart survival,
  prune-lock, branch-is-the-deliverable (no auto merge-back).
- A pure-host workspace (no container) needs no container at any point.
- Non-worktree sessions and ordinary bound folders are untouched —
  their container path is still `/workspace/<name>`.

## Known limitations

- `git worktree add --relative-paths` writes `extensions.
relativeWorktrees` into the parent's **local** `.git/config` and it
  persists after `git worktree remove`. Local-only (not cloned), so
  teammates/CI are unaffected, but on this machine any git touching the
  repo must be >= 2.48 thereafter. Accepted (ADR 0029).
- The host-first/container-fallback path in `git_status_entries` is now
  redundant for worktrees (host git works) but kept as harmless
  robustness; not re-tested here.
