# 0039 — Watch git refs for live SCM state

## Context

The SCM panel (branch label, ahead/behind, status list, commit log)
went stale after external git activity — an agent or terminal
running `git commit`, `git push`, `git fetch` — until the 3-minute
auto-fetch tick or a window refocus. Two causes:

1. The fs watcher watched `.git/` only one level deep and
   whitelisted just `HEAD` / `index` / `MERGE_HEAD` / `MERGE_MSG`.
   Ref moves live under `.git/refs/**` (nested — invisible to a
   non-recursive watch), and fetches/pushes touch `FETCH_HEAD` /
   `packed-refs` / `refs/remotes/**` (filtered). This was a
   deliberate "event-storm tradeoff" from before the debounce and
   the modify-only fast path existed.
2. A filter-ordering bug: paths that passed the `.git` whitelist
   were then dropped by the `node_modules`/`.git`
   excluded-component check, so in practice _no_ `.git` event ever
   reached the frontend — even branch switches rode on incidental
   working-tree events.

Additionally, linked worktrees (how coder workers run) keep their
git metadata outside the workspace root (`.git` is a pointer
file), so an active worktree folder had no git watching at all.

## Decision

- Watch `.git/` non-recursively **plus `.git/refs/` recursively**.
  Widen the observed set to `HEAD`, `index`, `MERGE_HEAD`,
  `MERGE_MSG`, `ORIG_HEAD`, `FETCH_HEAD`, `packed-refs`, and
  everything under `.git/refs/` except `*.lock`. Everything else
  under `.git/` (objects, logs, lock files) stays invisible to the
  frontend. `.git` paths never flip `topologyChanged` — git
  metadata isn't rendered in the tree, and a commit's loose-ref
  create/remove must not force a full tree re-walk.
- For a linked worktree, resolve the `gitdir:` pointer and its
  `commondir`, watch those (gitdir + commondir non-recursive,
  `commondir/refs` recursive), and fold their events into a
  synthetic `.git/<suffix>` namespace so the frontend sees one
  shape regardless of checkout kind.
- Keep `.git/` paths out of the LSP `didChangeWatchedFiles`
  forward — no server wants per-commit metadata churn.
- The auto-fetch loop's HEAD-SHA snapshot stays as the safety net
  for the cases inotify structurally can't cover (watch
  exhaustion, attach failure, network mounts, changes while the
  IDE was closed).

The event-storm concern that motivated the original blanket filter
is handled by the existing 250 ms leading+trailing debounce and
the whitelist: a commit or fetch collapses to one `fs:changed`
emit, and the frontend's modify-only path answers it with a single
`git status` + branch probe.

## Rejected alternatives

- **Polling git state on an interval.** Burns a serialized git
  invocation (ADR 0015) per folder per tick for state that changes
  rarely; still adds seconds of latency where inotify gives
  milliseconds.
- **Having the coder runner push SCM refreshes after tool calls.**
  Covers agents but not terminals or any other external process;
  the watcher covers all three with one mechanism.
- **Watching all of `.git/` recursively.** `objects/` and `logs/`
  produce the real event storm and carry no signal the whitelist
  doesn't already get from the ref/metadata files.
