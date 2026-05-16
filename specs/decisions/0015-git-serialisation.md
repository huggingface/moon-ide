# ADR 0015 — Per-folder git serialisation + commit safety snapshot

Date: 2026-05-16
Status: accepted

## Context

The IDE issues git subprocesses from many independent code
paths against the same workspace folder:

- **User-driven:** `git commit` (with hooks), `git_commit_on_new_branch`,
  `git_push`, `git_pull`, `git_merge_default_branch`,
  `git_publish_branch`, `branch_switch`, `git_restore_paths`.
- **Background:** the auto-fetch loop (3-min interval + on
  window focus), `git_status_entries` triggered on every fs
  watcher event, `git_branch` ahead/behind polling, blame
  fetches, `git_diff_patch` / `git_diff_summary` for AI commit
  / branch-name suggestions, `git_ref_content` for the diff
  view.

Git itself coordinates writers via `.git/index.lock` (a file
lock created on `acquire`, removed on `release`). It is
**not** process-aware: any two git invocations that try to
take the lock at the same time will see one of them fail
with `Unable to create '.git/index.lock': File exists`. Read
operations don't take the lock, but lots of "read" operations
under the hood do — `git status` refreshes racy stat cache,
`git fetch` updates `FETCH_HEAD` and `packed-refs`, etc.

The user-visible failure mode came from pre-commit hooks
that do their own `git stash` dance — lint-staged, the
`pre-commit` Python framework. Their flow:

1. `git stash create` to back up state.
2. Run linters against the staged content.
3. On error: `git reset --hard HEAD` then
   `git stash apply --index <backup>` to restore.

Step 3 takes the index lock multiple times. When our
auto-fetch / status poll / blame fetch landed in the middle
of step 3, the apply was interrupted mid-write, the working
tree ended up missing files, and `.git/index.lock` was
left behind as a corpse. The Mongoku user lost data twice
this way (recovered both times via `git stash list` because
lint-staged's own backup happened to survive — but the
recovery was non-obvious and the lost time was real).

This is fundamentally **not lint-staged's fault** — they
assume nothing else is touching the repo while their hook
runs. We violate that assumption every few seconds.

## Decision

Two complementary changes in `crates/moon-core/src/host.rs`:

### 1. Per-folder git mutex

`LocalHost` carries an `Arc<tokio::sync::Mutex<()>>`. Every
`WorkspaceHost` method that spawns a `git` subprocess
acquires the mutex as an owned guard via
`Mutex::lock_owned()` and either:

- moves the guard into the `tokio::task::spawn_blocking`
  closure (sync-shaped commands like `git_status_entries`,
  `git_commit`, `git_blame`, …), or
- holds the guard across the inner async call's `.await`
  (already-async commands: `git_fetch`, `branch_list`).

The guard drops at the end of the closure / async block,
releasing the mutex for the next caller. Tokio's `Mutex` is
FIFO so a long-running commit can't starve a status poll
forever, and a flurry of status polls can't starve a
commit either.

`collect_paths` also takes the lock because its
`collapsed_ignored_dirs` seed step shells out to
`git status`. `collect_paths_under` does not — it's a pure
fs walk on a known-gitignored subtree.

### 2. Commit safety snapshot

`run_git_commit` snapshots the index immediately after
`git add -A` succeeds and before `git commit` runs hooks:

```text
git add -A                     # stage everything (existing)
SNAP=$(git stash create)       # snapshot the staged tree
git commit -m "..."            # may fire hooks
# on failure:
git read-tree --reset $SNAP    # restore index to snapshot
git checkout-index -a -f       # write working tree from index
```

The snapshot is a free-floating commit (not in the stash
list); on success it becomes unreferenced and git GC drops
it in the usual 30/90 day window. On any failure between
snapshot and successful commit we run the restore sequence;
if the restore itself fails we fall back to
`git stash store -m "moon-ide commit safety snapshot — recover with \`git stash pop\`" $SNAP`so the user sees a labelled stash in`git stash list` and
recovery is one command away.

We snapshot **after** `git add -A` rather than before
because `git stash create -u` silently drops untracked
files on git ≤ 2.43 — but staging-as-Added pulls them into
the index where vanilla `git stash create` captures them.

`run_git_commit_on_new_branch` doesn't need its own
snapshot: its only destructive call is `run_git_commit`,
which already carries one.

## Consequences

**What gets safer:**

- The Mongoku-style data-loss scenario (lint-staged hook
  crashes mid-flight, working tree ends up empty) is
  recoverable automatically: `try_restore_commit_safety_snapshot`
  brings the staged content back. No manual `git stash list`
  required.
- Every git invocation against the same workspace serialises,
  so the upstream cause of the lint-staged crash (concurrent
  index-lock contention from our background ops) goes away
  too. The mutex is the root-cause fix; the snapshot is
  belt-and-braces for the case where the hook misbehaves
  for some other reason.

**Side effects:**

- A successful pre-commit hook that auto-fixes files
  (`eslint --fix`, `prettier --write`) followed by a
  non-zero exit (the "I modified files, please re-add"
  pattern) used to leave the auto-fix in the working tree.
  With the safety snapshot, the auto-fix is wiped on
  restore. This matches lint-staged's own "revert to
  original state" path, so the user-visible end state is
  the same as a successful lint-staged abort — minus the
  data-loss possibility.
- A long auto-fetch (worst case ~30s under the existing
  fetch timeout) can hold the mutex while a user-initiated
  commit waits behind it. In practice fetch is sub-second
  on a fast network and only fires every 3 minutes; the
  contention window is small. The alternative — a separate
  fetch mutex, or fetch using a try-lock with backoff — is
  more code for not much real benefit. Revisit if it bites.
- `branch_list`'s `gh pr list` call can be slow (network).
  It holds the mutex too, so a commit started while the
  branch palette is fetching PRs queues behind. Same
  reasoning: rare, sub-second in practice, revisit if it
  bites.

**What doesn't change:**

- IPC surface: the mutex is internal to `LocalHost`. Tauri
  commands and frontend types are unchanged.
- Test surface: existing host tests still drive the same
  `WorkspaceHost` trait methods. Two new tests cover the
  added behaviour:
  - `safety_snapshot_restores_after_destructive_pre_commit_hook`
    installs a hook that `rm`'s tracked + untracked files
    and exits non-zero, and asserts both files are back on
    disk afterwards.
  - `concurrent_commits_serialise_via_git_mutex` fires two
    `git_commit` calls concurrently with a slow hook;
    without the mutex one would fail with
    `index.lock` contention; with it, both succeed.

## Alternatives considered

- **Per-`Command::new("git")` mutex inside `host.rs`.**
  Rejected: each git method composes multiple subprocesses
  (`git add` + `git commit` + `git rev-parse`), and we want
  them to share a single uninterrupted window — a per-call
  mutex would let a background op slip in between
  `git add` and `git commit`. The per-method guard wraps
  the whole composed operation correctly.
- **`std::sync::Mutex` instead of `tokio::sync::Mutex`.**
  Rejected: blocking-pool tasks are fine to hold a sync
  mutex, but the trait methods are async and need to await
  the lock without blocking the runtime. `tokio::sync::Mutex`
  - `lock_owned()` gives us `Send + 'static` guards we can
    move into `spawn_blocking`, which is exactly the shape we
    need.
- **`git stash create -u` for the safety snapshot.**
  Rejected: silently no-ops untracked files on git ≤ 2.43.
  Snapshotting after `git add -A` sidesteps the bug.
- **`git stash apply --index` for restore.** Rejected: it
  reads the existing index state and refuses (or silently
  no-ops) when the worktree is in a state the apply can't
  cleanly merge against. `git read-tree --reset` followed by
  `git checkout-index -a -f` is unconditional and matches
  the "we want exactly this index, period" intent.
- **`--no-verify` toggle in the SCM panel.** Out of scope
  for this ADR — it's a UI affordance the user can opt into
  when their hooks are misbehaving. The mutex + snapshot
  fix the data-loss case independent of that toggle, which
  can ship later if the team asks for it.
- **A separate "git ops are quiesced" signal** the SCM
  panel could observe, so it knows not to fire status
  refreshes during a commit. Rejected: that's the mutex
  by another name, and threading a state machine into the
  frontend would be more code for the same end. The mutex
  is invisible to the UI and just makes things work.
