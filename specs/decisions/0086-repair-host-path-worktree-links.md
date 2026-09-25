# ADR 0086 — Repair host-path worktree links too, and before every remove

Date: 2026-09-25
Status: accepted; implemented. Extends
[ADR 0080](0080-repair-container-path-worktree-links.md).

## Context

ADR 0080 rewrites worktree git links that are absolute **container**
paths (`/workspace/<base>/…`) and deliberately leaves "a real host
path" alone. The mirror case bites just as hard: a worktree created
on the host outside moon-ide (`git worktree add` in a shell, another
tool) without `--relative-paths` carries absolute **host** links
(`gitdir: /home/…/<parent>/.git/worktrees/<name>`). Host git is
happy, but when the workspace runs in the dev container, discard runs
`git worktree remove` there, the host path doesn't exist, and git
fails "is not a working tree" — the worktree becomes undeletable from
the IDE. The repair also only ran in the adoption sweep, so an
already-bound worktree never got it.

## Decision

- `repair_absolute_worktree_links` accepts both spellings of the
  parent repo as placeable prefixes: the container mount **and** the
  parent's host path. A link under either is rewritten to the
  relative form; anything else (foreign mount, unrelated host path)
  is still untouched. Prefix matches now require a `/` boundary, so
  `<parent>-other/…` never matches `<parent>`.
- `git_worktree_remove` runs the repair (host-side file I/O,
  idempotent) immediately before the git call, so discard works from
  either side of the bind mount regardless of how the worktree was
  created. The adoption sweep keeps running it too.

## Alternatives considered

- **Always run `worktree remove` on the host.** Fixes this one call
  but leaves the worktree broken for every other container-side git
  command the agents run in it.
- **`git worktree repair --relative-paths`.** Needs a git that can
  resolve the links from where it runs — exactly what's missing on
  the wrong side of the mount; the textual rewrite doesn't care.
