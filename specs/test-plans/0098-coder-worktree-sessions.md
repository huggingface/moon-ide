# Test plan 0098: coder worktree sessions

- **Date**: 2026-06-03
- **Phase**: Phase 6 follow-on (worktree sessions, staged W.0–W.4)

## What shipped

This plan covers the full worktree-sessions feature; it is written
ahead of the staged rollout and refined as each sub-phase lands.
**W.0–W.4 have landed.** Create, route, review, restart-survival, and
discard all work; in a containerised workspace an isolated session
runs its git / `bash` / format-on-save **in the container** so builds
use the container toolchain (worktrees tree mounted once at
`/workspace/.worktrees`, created host-side then `git worktree
repair`'d inside the container). Git mechanics validated end-to-end
against a live `moon-base` container. W.4.1 (AI branch names) is the
only remaining item.

- A coder session can opt into running in its own git worktree — an
  isolated checkout on a fresh branch — so several agents work one
  project at once without colliding.
- The worktree is a first-class bound folder (nested under its parent
  in the folder bar), so the file tree, SCM panel, diff, review, and
  terminal all work against it unchanged.
- Each isolated agent's work stays on its own branch; the user makes
  one PR per branch. No forced merge-back.
- New `WorkspaceHost` git primitives — `git_worktree_add` / `_list` /
  `_remove` — routed through the active host, serialised behind the
  per-folder git mutex (W.0).
- Worktree folders survive restart (`FolderOrigin` rides
  `session.json`) and are pruned by the row's `×` with a dirty-guard
  re-confirm; the branch is always kept (W.3).
- In a containerised workspace an isolated session's git / `bash` /
  format-on-save run **in the container** (worktrees mounted once at
  `/workspace/.worktrees`, repaired to container paths on create and
  on container start), so builds use the container toolchain. Each
  worktree is `git worktree lock`ed so a stray prune can't sever it
  (W.4).

## How to test

Prerequisites: `bun install`, host deps per `README.md`. A scratch
git repo with at least one commit bound as a workspace folder.

### W.0 — git worktree primitives (Rust)

1. `cargo test -p moon-core worktree`. Expected: the worktree
   round-trip unit test passes (add a worktree on a new branch in a
   temp repo, list it, remove it).
2. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all
--check`. Expected: clean.

### W.2+ — end to end (once the UI lands)

1. Bind a repo with a clean working tree. In the coder panel, use the
   "new isolated session" affordance. Expected: a new branch +
   worktree are created; a nested folder row appears under the parent
   in the folder bar with a branch glyph.
2. Ask the agent to edit a file and run a build. Expected: the edits
   land in the worktree checkout, not the parent's working tree;
   `git status` in the parent stays clean.
3. Start a second isolated session in the same folder and have it edit
   the same file. Expected: the two agents' changes are on two
   different branches in two different worktrees — no interleaving.
   3b. Open the branch switcher, click the "start isolated agent" (✦)
   action on an existing branch — a local one, or an open PR's branch
   that only exists on the remote. Expected: an isolated session whose
   worktree has that branch checked out (DWIM-created locally tracking
   the remote for the PR case); the **parent's checked-out branch is
   unchanged** and other agents keep running.
4. Click the worktree folder row → SCM panel shows the worktree's
   diff; commit / push the branch. Expected: a normal commit on the
   isolated branch; the parent branch is untouched.
5. Reload the app. Expected: the worktree folder re-binds (nested row
   reappears) and reopening its session keeps routing tools to the
   worktree.
6. Click the worktree row's `×`. Expected: a "Discard worktree on
   `<branch>`?" confirm; on confirm the git worktree is pruned and the
   row disappears, but `git branch --list <branch>` still shows the
   branch. With uncommitted changes present, expect a second "discard
   changes anyway?" confirm before it's forced. Deleting the session
   instead (trash icon) leaves the worktree bound.

### Container git mechanics (reproducible without the IDE)

Mirrors moon-core's exact create → repair → discard sequence. Needs
docker + a `moon-base` image.

1. `git init` a scratch parent + one commit. Boot the container with
   the parent **and** a shared worktrees-root mount: `docker run -d -v
<parent>:/workspace/parent -v <state>/worktrees:/workspace/.worktrees
moon-base:dev sleep 600`.
2. Host-create + lock (what `git_worktree_add` does): `git -C <parent>
worktree add -b moon/agent-1 <state>/worktrees/p/wt1` then `git -C
<parent> worktree lock --reason "moon-ide isolated session
(ADR 0028)" <state>/worktrees/p/wt1`.
3. Repair into the container (`git_worktree_repair`): `docker exec -w
/workspace/parent <cid> git worktree repair
/workspace/.worktrees/p/wt1`. Expected: container git + `bash` now
   work at `/workspace/.worktrees/p/wt1` (right branch, build context).
4. Create a **second** worktree the same way while the container keeps
   running. Expected: same container (no recreate), wt2 visible
   immediately, isolated on its own branch.
5. Hostile prune from the container: `git gc --prune=now` +
   `git worktree prune --expire=now` against `/workspace/parent`.
   Expected: both worktrees **survive** (they're locked).
6. Discard (`git_worktree_remove`, container-side): `docker exec -w
/workspace/parent <cid> git worktree unlock /workspace/.worktrees/p/wt1
&& git worktree remove --force /workspace/.worktrees/p/wt1`.
   Expected: clean removal; the branch still exists.

## What must keep working

- Ordinary (non-isolated) sessions drive the folder's main working
  tree exactly as before; their header stays byte-identical (no
  `worktree_*` fields).
- Concurrent sessions per folder ([ADR 0016](../decisions/0016-coder-concurrent-sessions.md))
  are unaffected when none are isolated.
- Every other git command (`git_commit`, branch switch, status, diff)
  still serialises correctly through the per-folder git mutex with the
  new worktree commands in the mix.
- A worktree session's sub-agents run against the worktree, not the
  parent's main tree.

## Known limitations

- While the dev container is **down**, a worktree's git is
  unavailable (its metadata holds `/workspace/.worktrees/…` paths that
  only resolve in the container) — same degraded state as the rest of
  the workspace; starting the container repairs it. A pure-host
  workspace keeps everything host-side.
- The git mechanics were validated end-to-end against a live
  `moon-base` container (create → repair → isolate → build → discard,
  shared-root mount with no recreate, prune survival), but the full
  IDE-in-container path still wants a real smoke-test.
- The IDE never deletes a branch — pruning a worktree leaves its
  branch for a later PR.
- No per-agent merge/rebase automation: combining branches is the
  user's job through the normal SCM flow.
- The branch name is a fixed `moon/agent-<id>` until W.4.1 wires the
  AI suggester.

## Related

- Specs: [coder.md § Worktree sessions](../coder.md#worktree-sessions),
  [roadmaps/phase-06-coder.md § Follow-on](../roadmaps/phase-06-coder.md#follow-on-worktree-sessions).
- ADRs: [0028 — worktree-backed coder sessions](../decisions/0028-coder-worktree-sessions.md),
  [0016 — concurrent sessions](../decisions/0016-coder-concurrent-sessions.md),
  [0015 — git serialisation](../decisions/0015-git-serialisation.md).
- Prior test plans: [0085 — concurrent sessions](0085-coder-concurrent-sessions.md).
