# ADR 0092: Keep the git lock off network calls, read refreshes, and turn baselines

Revises [ADR 0015](0015-git-serialisation.md) (which git work takes the
per-folder git mutex) and the baseline mechanism of
[ADR 0030](0030-orchestrator-sessions.md).

## Context

Agents spent a lot of time waiting on git:

- Their own `git add` / `commit` / `checkout` (through `bash`) failed
  with "index.lock exists": the IDE runs `git status` on every
  working-tree change, and `status` / `diff` opportunistically rewrite
  the index to refresh its stat cache, taking `.git/index.lock`.
- Every agent turn started with `git stash create` (the per-turn diff
  baseline) under the per-folder mutex — itself taking `index.lock` —
  and so queued behind whatever held the mutex.
- The mutex was held across network calls: `git fetch` (up to 30 s,
  auto-fetch every 3 min, on focus, on every sub-agent spawn), the
  base-check fetch, `gh pr list` / PR-URL lookups, push, clone.

## Decision

- **Read refreshes don't lock.** `status` and `diff` run with
  `--no-optional-locks`: they never create `.git/index.lock`, so they
  can't break an agent's or a hook's git writes.
- **Turn baselines don't lock.** The baseline is built in a throwaway
  copy of the index (`GIT_INDEX_FILE`: `add -u` + `write-tree`) — the
  same tracked-files snapshot `stash create` took, as a tree SHA — so
  it touches neither `.git/index` nor the mutex. `HEAD` remains the
  fallback.
- **Network calls don't hold the git mutex.** Fetch, push, publish, and
  the base-check fetch serialise on a separate per-folder
  `remote_mutex`; `gh` lookups (`branch_list`, existing-PR URL) and
  clone (a different repo) take no lock. Pull stays under the git mutex
  — it rewrites the working tree. The base check's local probe still
  runs under the git mutex.

ADR 0015's guarantee is unchanged: everything that writes the index or
working tree (commit, add, restore, switch, merge, pull, worktree ops)
is still serialised, and no background IDE command takes
`index.lock` while a hook runs.

## Rejected alternatives

- **Drop the mutex for all read-only commands** (blame, log, ref
  reads). Safe once the above is in, but a broader change; left as a
  follow-up if waits remain.
- **Retry agent git commands on "index.lock exists".** Treats the
  symptom; the IDE shouldn't be creating the lock in the first place.
- **Skip the baseline when the mutex is busy.** Loses the turn diff
  exactly when several agents are active.
