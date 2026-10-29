# ADR 0100: Don't switch branches under a worktree session

## Context

A session's worktree was switched to another branch between two of its
turns. The reflog entry was written host-side (container git runs in
UTC; the entry carried the host's offset), and the only host-side
switch path that targets the active folder is the IDE's own: clicking a
session's branch chip whose branch wasn't checked out anywhere ran
`git switch` in the **active folder**, which happened to be another
session's worktree. Nothing told the owning session; it found foreign
code mid-edit. `enter_worktree` then couldn't re-check out its branch,
because the worktree directory is derived from the branch name and the
derived `.worktrees/<slug>` now held the other branch.

## Decision

- **The branch chip never switches a worktree.** If the branch lives in a
  bound worktree, the chip focuses it (as before). Otherwise it switches
  the project's **main checkout**, focusing it first when a worktree was
  active.
- **Branch switches refuse a worktree a session runs in.** The desktop
  branch switcher and the companion's switch RPC error out naming the
  owning session, pointing at the main checkout or an isolated agent
  instead. Project folders are never refused.
- **A session hears about a switch it didn't make.** Each session
  remembers its worktree's branch at every turn end; when the next turn
  starts on a different branch, a `[moon-ide]` notice is queued like a
  steer before the first round-trip. It says what changed and steers the
  agent to `enter_worktree` instead of editing, committing, or switching
  back. Not repeated for the same branch. A commit-on-new-branch made
  with the session open doesn't count, since it already stamps the
  header. The memory is in-process: after a restart the comparison falls
  back to the branch the session entered on.
- **Worktree directories take a free `-N` suffix** when the derived path
  already exists, for every IDE creation path (`enter_worktree`, `task`'s
  `worktree`, workers, the worktree button), so a branch whose directory
  was taken can always be checked out again.

## Rejected

- **Lock / owner marker files per worktree, or intercepting
  `git checkout` in `bash`.** The `bash` tool can't reliably intercept
  raw git, and the observed switch came from the IDE, not an agent.
- **Re-stamping the session header to the new branch.** That would make
  the session's branch chip, its deliverable, follow whatever got
  switched in.
- **Switching back automatically.** Whoever switched may be using the
  checkout.
