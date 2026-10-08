# ADR 0093: Worktrees never nest; the sessions-list button branches off main

## Context

The sessions list's "new isolated session" button created the worktree
off the **active folder**. With a worktree session open, the active
folder is that worktree, so the new checkout landed at
`<project>/.worktrees/<a>/.worktrees/<b>`, branched off the other
agent's branch. Two things then broke: the folder bar only nests one
level, so the new row never rendered; and the session-list root
resolved one hop (to worktree `<a>`), while the session had been filed
under the project — opening the session showed an empty panel.

## Decision

- **Worktrees always live under the project root** (`<project>/.worktrees/`),
  whichever folder requested them — UI button or `spawn_worker` with a
  worktree `folder`. The worktree folder's `parent_path` is the project.
- **The sessions-list button branches off the default branch**
  (`origin/main` via `origin/HEAD`, else `HEAD`), not whatever the
  current checkout is on. The branch is created `--no-track`, so a
  branch started from `origin/main` doesn't track main.
- **An agent-driven spawn from a worktree keeps that worktree's branch
  as its start point** (a follow-up on a worker's work), just placed
  under the project. A spawn from a project folder still branches off
  its `HEAD`.
- **No compatibility handling for existing nested worktrees.** One
  root hop is enough when worktrees never nest; the one nested checkout
  that existed was removed by hand.

## Rejected alternatives

- **Render nested worktrees recursively.** Keeps a layout nobody wants
  and still leaves the confusing base branch.
- **Branch off the current checkout's `HEAD`** (the old behaviour). A
  new session from the sessions list is new work; inheriting a
  colleague agent's half-done branch by accident was the surprise.
- **Resolve roots through the whole parent chain** so legacy nested
  worktrees keep working. Compatibility code for a state that no longer
  gets created (AGENTS.md: no premature migrations).
