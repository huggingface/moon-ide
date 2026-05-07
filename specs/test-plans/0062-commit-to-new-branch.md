# Test plan 0062: Commit to a new branch (with optional AI-suggested name)

- **Date**: 2026-05-07
- **Phase**: 6.x (SCM polish) — adds an IPC + a UI surface, so it earns a test plan even though the user-visible affordance is small.

## What shipped

- New `WorkspaceHost::git_commit_on_new_branch(branch, message)` trait method on `LocalHost`. Validates `branch` with `git check-ref-format --branch` server-side, runs `git switch -c <branch>` from the current `HEAD`, then defers to the existing `git_commit` path (`git add -A && git commit -m <message>`). On commit failure the host rolls back: switches back to the previous ref (by name when `HEAD` was a branch, by SHA when detached) and deletes the freshly-created branch so the user's `HEAD` is back where it started — best-effort, errors logged but not surfaced.
- New `WorkspaceHost::git_diff_summary()` returns `git diff HEAD --stat=200,80 -M -C --no-color`, capped at 4 kB and `\n[truncated]`-suffixed past the cap. Drives the AI branch-name suggester; empty string on any failure (no repo, git missing, clean tree).
- `LC_ALL=C` is now set on the `git commit` subprocess in `run_git_commit` so the "nothing to commit" stdout-detection path works regardless of the user's system locale (it was previously broken on French / German / etc. installs — caught by the new rollback test).
- New Tauri commands `fs_git_commit_on_new_branch` and `coder_suggest_branch_name`. The latter pulls the active folder's `git_diff_summary` itself, then asks the fast model (`DEFAULT_FAST_MODEL` — same model used for session-title auto-rename) for a kebab-cased name. Output goes through a new `sanitise_branch_name` pass that lowercases, replaces whitespace / `_` / `/` with `-`, drops anything outside `[a-z0-9.-]`, collapses dash runs, trims leading/trailing punctuation, and clamps to 60 chars.
- SCM panel composer grew a second inset toggle (`BranchIcon`) next to the amend toggle. Clicking it expands an inline row under the textarea: a branch-name `<input>`, an AI sparkle suggest button (only rendered when the user is signed into Hugging Face — the suggester is a one-shot inference call), and a primary "Commit" button. `Enter` in the input commits, `Escape` cancels and clears the draft.
- Frontend state helper `WorkspaceState.commitChangesOnNewBranch(branch, message)` mirrors `commitChanges`: validates emptiness client-side, surfaces success / failure via flash + branch refresh + active-folder refresh.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, an active folder bound that is a git repo with a configured author identity. Have at least one working-tree change to commit.

### Happy path — manual branch name

1. Make a change in the editor and save it (so the SCM panel's change count goes up).
2. Open the SCM panel composer. Type a commit message (`Add tail param to fetch_job_logs`).
3. Click the new branch-icon toggle (the leftmost of the two inset buttons). Expected: an inline row appears under the textarea with a branch-name input and a "Commit" button. The amend toggle (✎) becomes disabled while the form is open.
4. Type a branch name (`feature/tail-param`). The "Commit" button enables.
5. Hit `Enter` (or click Commit). Expected:
   - Flash toast: `Committed <sha> on feature/tail-param: Add tail param to fetch_job_logs`.
   - The SCM panel header now shows `feature/tail-param` as the current branch.
   - The "Publish Branch" button appears in the sync slot (the new branch has no upstream).
   - The composer textarea + branch input are cleared and the inline form collapses.

### Happy path — AI-suggested name

6. Repeat step 1-3 with a different change.
7. Type a commit message (`Refactor cache invalidation`).
8. With the inline form open, click the sparkle (✨) button. Expected:
   - The button shows a spinner for a moment.
   - The branch-name input fills with a kebab-cased suggestion (e.g. `refactor-cache-invalidation`, `cache-invalidation-refactor`, etc.). Output is always lowercase, hyphenated, no slashes, no quotes.
   - On success the input gets focus so you can edit it before committing.
9. Edit the suggestion if you don't like it (the input is plain text — no auto-revert), or hit `Enter` to commit.
10. Try the suggest button **with an empty commit message**. Expected: the suggestion still works — it's based on the diff summary alone (`git diff HEAD --stat`).
11. Try the suggest button on a **clean tree** (commit step 9, then click ✨ before making any new edits). Expected: the diff summary is empty so the model gets `(none) / (none)` — it'll usually return something generic like `update`, `tweak`, or refuse with an empty answer (which surfaces as a flash `Could not suggest a branch name: branch name suggestion was empty`).

### Sparkle visibility tied to AI auth

12. Sign out via the coder panel's sign-out affordance. Refresh the SCM panel.
13. Open the new-branch form. Expected: the sparkle (✨) button is **gone**. The branch-name input + "Commit" button still work — manual entry is unaffected.
14. Sign back in. The sparkle reappears.

### Branch-name validation (server-side)

15. Open the form, type an invalid name (`feature with spaces`). Click Commit.
16. Expected: flash toast like `Commit failed: "feature with spaces": ...check-ref-format... is not a valid ref name`. `HEAD` stays on the original branch (verify in a terminal: `git -C <folder> symbolic-ref --short HEAD`).
17. Try a name that already exists (`main` if you're on a feature branch, or pick the current branch's own name). Expected: flash with git's own "already exists" stderr; no rollback needed because we never moved `HEAD`.

### Rollback on commit failure

18. With a clean working tree (no changes), open the form, type a valid name (`feature/empty`), click Commit. Expected:
    - Flash: `Commit failed: ...nothing to commit...`.
    - `git -C <folder> symbolic-ref --short HEAD` still points at the previous branch.
    - `git -C <folder> branch --list feature/empty` returns nothing — the host deleted the freshly-created branch.

### Detached HEAD

19. From a terminal: `git -C <folder> checkout <some-commit-sha>` so HEAD is detached. Refresh the SCM panel header (it shows `(<short-sha>)`).
20. Make a working-tree change. In the SCM panel, open the new-branch form, suggest or type a name, commit.
21. Expected: branch is created off the detached commit and `HEAD` lands on the new branch. If the commit fails, rollback uses the SHA snapshot to switch back to the same detached state.

### Amend interlock

22. Toggle Amend on (✎ pill lights up), then click the branch toggle. Expected: amend turns off when the new-branch form opens — the gestures don't compose, and we drop amend silently.
23. Click the branch toggle again to close. Amend stays off (we don't restore it).

### Keyboard

24. Open the new-branch form. Tab from the input → the sparkle button (if signed in) → the Commit button. `Esc` from the input closes the form and refocuses the textarea. `Enter` from the input submits.

### Existing flows unchanged

25. Don't open the new-branch form; commit normally with `Ctrl+Enter` in the textarea. Behaviour is identical to before — fresh commit on the current branch via `git_commit`.
26. Toggle amend, commit. Same behaviour as before.
27. Push, pull, sync, publish-branch, revert-all — all unchanged.

## What must keep working

- `commitChanges`, `pushChanges`, `pullChanges`, `publishBranch` — unchanged code paths. The new method lives alongside them on `WorkspaceHost` and reuses `run_git_commit` internally so a future fix to commit handling lands in both paths at once.
- The auto-rename session-title pipeline — uses the same `DEFAULT_FAST_MODEL` and the same `chat_completion` shape as the new branch-name suggester. Either feature breaking the other would surface here.
- Sign-out / sign-in flows — the suggest button's visibility is gated on `coder.signedIn`, which is the exact same flag the rest of the panel uses.
- The amend toggle — still works as before; only difference is it's mutually-exclusive with the new-branch form (per step 22).
- `LC_ALL=C` on the commit subprocess only affects git's own message strings, not paths. Verify by committing a file with a non-ASCII name (`mv foo.txt fôô.txt`, then commit normally). Expected: commits cleanly, the file shows up in `git log -1 --name-only` with its real name.

## Known limitations

- The branch-name suggester is one-shot — no "regenerate" affordance. Click ✨ again to re-roll. We could add temperature dialing or a "give me 3 options" pop-out later if anyone asks; one shot at a time is the smallest thing that works.
- The suggester sends both the commit message and a `git diff --stat` summary (paths + line counts only — no actual diff content). For a sufficiently abstract message the model may suggest a name that doesn't match the diff. Today we accept that — the user can see the suggestion before clicking Commit and edit or replace it.
- Container-host parity isn't tested here because Phase 2 has no `ContainerHost` impl yet. When that lands the new trait method lands with it.
- We don't auto-publish the new branch (`git push -u origin <name>`). The existing "Publish Branch" button surfaces in the sync slot and the user clicks it when they want to share. Auto-publishing on every new branch would surprise people working on private experiments.
- Branch name suggestions don't include any prefix like `feature/`, `fix/`, `chore/` — the prompt explicitly tells the model not to. The team convention isn't well-defined yet; if/when it is, we'll add it to the prompt then.

## Related

- Specs: [frontend.md](../frontend.md) — SCM panel section.
- Prior test plans: [0044-coder-polish.md](0044-coder-polish.md) (the auto-rename pipeline this re-uses), [0061-unify-subagent-iteration-cap.md](0061-unify-subagent-iteration-cap.md) (most-recent coder change, for chronology).
