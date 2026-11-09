# ADR 0108: Read-only git commands don't take the git lock

Revises [ADR 0015](0015-git-serialisation.md) and follows up the rejected
alternative in [ADR 0092](0092-git-lock-scope.md).

## Context

Switching branch (or removing a worktree) in moon-landing took 4–16 s.
Lock timings showed `git switch` itself at ~100 ms, waiting 3–13 s on the
per-folder git mutex behind a queue of `git blame` calls (50–700 ms each):
a branch switch moves `.git/HEAD`, and the frontend re-blamed every open
tab, then queued every tab's `HEAD` content read, status, log, and branch
probe behind them. ADR 0092 left "drop the mutex for read-only commands"
open "if waits remain"; they did.

## Decision

- **Read-only commands run without the git mutex:** blame, ref content
  (`git show`), blob SHA (`hash-object` without `-w`), permalink, log,
  commit diff, HEAD commit message, merge state, branch info, local
  branch list, exclude path. None of them write the index, refs, or the
  working tree (blame runs with `--no-optional-locks`), so they can't
  break a hook's or an agent's writes — the hazard ADR 0015 guards
  against. A read that races a write may return a stale snapshot; the
  fs-watcher refresh that the write triggers corrects it.
- **Only visible buffers are re-blamed** after a git-state change or
  save. Background tabs drop their cached blame and refetch when shown.
- Everything that writes (commit, add, restore, switch, merge, pull,
  revert, worktree ops) and the status / diff refreshes stay as before.

## Rejected alternatives

- **Keep the lock, make blame cheaper** (cap per-file history, skip big
  files). Still serialises every other command behind N blames.
- **A reader/writer lock.** Readers don't need exclusion from each other
  or from writers (git's own object/ref reads are safe under concurrent
  writes); an RwLock would still make the switch wait for in-flight
  blames to drain.
