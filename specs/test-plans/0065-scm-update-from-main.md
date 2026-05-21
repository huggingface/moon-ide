# Test plan 0065: SCM "Update from main" merge affordance

- **Date**: 2026-05-07
- **Phase**: 5.x (SCM polish) — adds a new `WorkspaceHost` trait method, a Tauri command, an IPC binding, two new fields on `GitBranchInfo`, and a new SCM-panel button. Earns a test plan on layer-count alone.

## What shipped

- **Two new fields on `GitBranchInfo`**: `default_branch_remote_ref` (e.g. `"origin/main"`) and `default_branch_behind` (commits the remote default has that the current branch's HEAD doesn't). Resolved server-side from `refs/remotes/origin/HEAD` with fallbacks to `origin/main` then `origin/master`. Folded into the existing `git_branch` IPC so the SCM panel doesn't need an extra round-trip.
- **New `WorkspaceHost::git_merge_default_branch(remote_ref)`** trait method + `LocalHost` impl. Shells out to `git merge --no-edit <remote_ref>`; errors propagate git's stderr verbatim (conflicts, dirty tree, unknown ref). Tauri command `fs_git_merge_default_branch` and `ipc.fs.gitMergeDefaultBranch` frontend wrapper follow the existing `git_pull` / `git_push` shape.
- **New "Update from main" button in `ScmPanel.svelte`**, gated on `defaultBranchBehind > 0` (and hidden when we're on the default branch — the regular Sync Changes button covers that). Renders below Sync Changes with secondary outlined styling so when both buttons stack the eye reads sync as the primary CTA. Spinner + "Merging…" label while in flight; flash on success / failure.
- **`busy` state split** to add a third flag `merging` alongside `committing` / `syncing`. The merge button spins independently while the broader `busy` derived value still gates "disable everything else while one git op runs".
- **Tool-call elapsed-time formatter** now ticks whole seconds while a tool is live (`23s → 24s` instead of `23.4s → 23.8s`); finished tool rows still show one decimal (`12.3s`) for precision after the fact. Sub-1s and over-60s formatting unchanged.
- **Three new backend tests** covering the happy path (`default_branch_behind` lands at 1 after upstream advances), the on-default-branch hide rule (`default_branch_behind == 0` even though `behind` is 1), and the merge fast-forward (`default_branch_behind` drops to 0 after `git_merge_default_branch`).

## How to test

Prerequisites: `bun install`, `cargo build`, `bun run tauri dev`. Same two-clone setup as test plan 0064 (a primary moon-ide workspace plus a sibling clone you can push from), with at least one feature branch you can spin up.

### Default-branch button surfaces only when needed

1. Open moon-ide on a repo whose `origin` has both `main` and a feature branch you can switch to.
2. Check out the feature branch in moon-ide. The SCM panel header shows `feature` (or whatever the branch is called); no "Update from main" button is visible.
3. From the sibling clone, land a commit on `main` and push:
   ```
   git -C ~/work/sibling-clone checkout main
   echo new >> README.md
   git -C ~/work/sibling-clone commit -am "main moves on"
   git -C ~/work/sibling-clone push
   ```
4. Wait up to 3 minutes for the auto-fetch loop, or alt-tab away/back to nudge a fetch.
5. Expected: the SCM panel grows an outlined "Update from main 1↓" button below the Sync Changes block (or in its place if there's nothing to sync upstream-wise).

### Merge runs cleanly when the working tree is clean

6. With "Update from main 1↓" showing, click it.
7. Expected: the icon spins, the label flips to "Merging main…", every other SCM control is disabled. After the merge returns, a flash appears: `Merged main into current branch.`
8. The button disappears (count is now 0). If your branch was previously up to date with its upstream the Sync Changes button now shows `↑1` because the merge created/advanced a commit on your local branch that hasn't been pushed.
9. From a terminal, `git -C <repo> log --oneline -5` shows the merge or fast-forward in the right place.

### On the default branch the button hides

10. Switch moon-ide's active checkout to `main`. From the sibling clone, land another commit on `main` and push.
11. Wait for auto-fetch / alt-tab nudge.
12. Expected: only the regular Sync Changes button appears with `↓1`. No "Update from main" button — `Sync Changes` already covers the same commits via `behind`.

### Repos with no `origin` or no `main` / `master`

13. Open a repo with **no** remote (`git -C <repo> remote remove origin` if needed). The SCM panel renders a clean header (or the `Publish branch` affordance for a branch with no upstream); no "Update from main" button regardless of the branch state.
14. Open a repo whose default branch is `develop` and which has neither `origin/main` nor `origin/master`. The button stays hidden — we don't speculate beyond `origin/HEAD` → `origin/main` → `origin/master`. (Adding a `develop` fallback gets a follow-up if the team runs into one.)

### Conflict path

15. On a feature branch, edit a file that `main` has also touched in a conflicting way (e.g. both branches change the same line of `README.md`).
16. Click "Update from main".
17. Expected: a flash with git's stderr verbatim — something like `Merge failed: git merge origin/main: CONFLICT (content): Merge conflict in README.md`. The button stays visible (the merge didn't land), the working tree is in the conflicted state. Resolve from a terminal as usual; the button disappears on the next branch refresh.

### Tool-timer cosmetic check

18. Open the Coder panel. Send a prompt that triggers a long tool call (e.g. ask the agent to read a slow file or run a bash command with `sleep 5`).
19. Watch the tool-row summary. While the call is live and elapsed ≥ 1s the timer should read `1s`, `2s`, `3s`, … (whole seconds, no decimal). It should not flicker between `0.8s` / `1.2s` / `1.5s`.
20. After the tool finishes, the displayed duration switches to one decimal (e.g. `5.3s`). Sub-second tools still show in milliseconds (e.g. `12ms`) live and final.
21. Tool calls over a minute show `1m 05s` style — unchanged.

### Backend unit tests

22. `cargo test -p moon-core --lib -- host::tests::git_branch_reports_default_branch_behind_after_remote_advances` — green.
23. `cargo test -p moon-core --lib -- host::tests::git_branch_default_branch_behind_is_zero_when_on_default_branch` — green.
24. `cargo test -p moon-core --lib -- host::tests::git_merge_default_branch_fast_forwards_local_branch` — green.

## What must keep working

- **Sync Changes / Publish Branch / Open PR** — same code paths, same gating. The new button is additive; it doesn't replace or shadow the existing affordances.
- **`git_branch` IPC + `GitBranchInfo` shape** — only added two trailing fields (`defaultBranchRemoteRef`, `defaultBranchBehind`). All existing consumers (folder bars, sync button, PR button) read the same prefix.
- **Auto-fetch loop (test plan 0064)** — unchanged. The new "Update from main" surface piggybacks on the existing periodic fetch; we don't fetch during the merge itself.
- **`busy` gate across the SCM panel** — commits, reverts, sync, publish, and merge all disable each other. Verified by clicking through each combination: while one runs, every other button is `disabled`.
- **`bun run check`, `bun run lint`, `cargo check --workspace --exclude moon-desktop`, `cargo clippy --workspace --exclude moon-desktop --all-targets -- -D warnings`** all clean.

## Known limitations

- **Hardcoded fallback list.** `origin/HEAD` → `origin/main` → `origin/master`. A repo whose default is `develop` (or `trunk`) with no `origin/HEAD` set won't surface the button. The fix is `git remote set-head origin --auto` once on the repo; we'll add a fallback if the team runs into this repeatedly. (`hardcode first, configure later` from AGENTS.md.)
- **Always merges, never rebases.** Per the user's wording ("merge main branch into current branch"). If the team prefers `git rebase origin/main` we'll add a setting (or a second button) when there's a concrete second use-case.
- ~~**No abort affordance.** A merge that ends in conflicts leaves the user in a `MERGING` state with no in-app "abort merge" button. They handle it from a terminal (`git merge --abort`).~~ Shipped in Phase 5 §5.6 (test plan 0088): when `.git/MERGE_HEAD` exists the SCM panel renders **Commit merge** + **Abort merge** in the commit row, with the conflict-marker editor decorator handling resolution.
- **No fetch as part of the merge.** We rely on the auto-fetch loop (or a manual sync) to keep `origin/main` current. The merge runs against whatever the local remote-tracking ref points at right now — if it's stale you'll merge stale commits. The auto-fetch interval (3 minutes) is the floor.
- **Single remote.** Only `origin` is consulted. Multi-remote teams can still sync against their custom upstream via the regular Sync Changes button; "Update from main" assumes the canonical-upstream model.
- **Default branch detection runs on every `git_branch` call.** Each invocation shells out to `git symbolic-ref` (and possibly two `git rev-parse`s). All three are sub-millisecond local-only ops; no measurable impact on the SCM panel refresh hot path.

## Related

- ADRs: [0002 — workspace host](../decisions/0002-workspace-host.md) (`git_merge_default_branch` is one more `WorkspaceHost` method; same shape as `git_pull` / `git_push`).
- Specs: [roadmap.md § Phase 5](../roadmap.md#phase-5--git).
- Prior plans: [0062 — commit to a new branch](0062-commit-to-new-branch.md), [0064 — git auto-fetch](0064-git-auto-fetch.md) (the auto-fetch keeps `origin/main` current so this button surfaces on its own).
