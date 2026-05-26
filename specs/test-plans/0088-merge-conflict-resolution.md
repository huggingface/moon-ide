# Test plan 0088: Merge conflict resolution

- **Date**: 2025-11-21
- **Phase**: 5.6 (SCM polish — closes the long-deferred "resolve conflicts" gap)

## What shipped

- **`GitFileStatus::Conflicted` + porcelain mapper.** Status pipeline
  surfaces `UU`/`AU`/`UA`/`DD`/`AA`/`UD`/`DU` instead of dropping
  them. File-tree rows get a `!` badge via Pierre's
  `renderRowDecoration` on top of the regular `modified` colour.
- **`GitMergeState` DTO + `git_merge_state` / `git_merge_abort` /
  `git_add_paths` host methods.** All serialise via the per-folder
  git mutex (ADR 0015). `git_merge_state` reads `.git/MERGE_HEAD` +
  `.git/MERGE_MSG` and runs `git ls-files --unmerged`.
- **SCM panel merge-mode reshape.** When `gitMergeState.inProgress`
  flips on: `Merging <ref>` warning banner on its own row between
  the header and the composer (full-width so a long ref or the
  short-SHA fallback can't crowd the branch label), composer
  prefilled from `.git/MERGE_MSG`, **Commit merge** + **Abort
  merge** in the commit row, sync / publish / update-from-main
  hidden, "N files still have unresolved conflicts" hint below
  the row when the unmerged list is non-empty.
- **Editor conflict-marker decorator** (`editor/conflictMarkers.ts`).
  Tints the block, highlights marker lines, and renders an inline
  `Accept current` / `Accept incoming` / `Accept both` widget on
  the opening `<<<<<<<` line. Gated on
  `gitStatusEntries[path].status === 'conflicted'` so unrelated
  buffers stay inert.
- **Auto-stage on save** + **soft-warn on residual marker text.**
  Saving a previously-conflicted file whose markers are gone runs
  `git add <path>` so the row's badge clears immediately.
  Clicking **Commit merge** with leftover `<<<<<<<` / `=======` /
  `>>>>>>>` lines in any tracked file pops a confirm dialog
  before the commit lands.

## How to test

Prerequisites: a folder with at least two branches that touch the
same line of the same tracked file, and `git` on PATH. The
"setup" step below creates one in a scratch directory.

### Setup

```sh
mkdir -p /tmp/moon-conflict && cd /tmp/moon-conflict
git init -q -b main
git config user.email test@example.com
git config user.name test
printf 'one\ntwo\nthree\n' > conflict.txt
printf 'base\n' > clean.txt
git add . && git commit -q -m base
git switch -c feature
sed -i 's/two/TWO-feature/' conflict.txt
git commit -qam feature
git switch main
sed -i 's/two/TWO-main/' conflict.txt
git commit -qam main
```

Open `/tmp/moon-conflict` in moon-ide.

### 1. Trigger the conflict

1. From the SCM panel (or any terminal), run `git merge feature`.
   In moon-ide: open the integrated terminal and run the command,
   or use "Update from main" if `feature` were the default branch
   (it isn't here — use the terminal).
2. Expected: the merge command exits non-zero with the standard
   `CONFLICT (content): Merge conflict in conflict.txt` line.
3. **Within ~500 ms** (one fs-watcher debounce window) the SCM
   panel reshapes:
   - A `Merging feature` banner appears on its own row between
     the header and the composer, in the warning colour
     (yellow-ish).
   - The composer textarea is pre-filled with `.git/MERGE_MSG`
     content — typically `Merge branch 'feature'` plus a
     `# Conflicts: …` block. Cursor lands at the end.
   - The commit row now reads **Commit merge** with an **✕**
     (abort) toggle next to it. The amend / new-branch toggles
     are gone.
   - The sync, publish, and "Update from main" buttons are
     **hidden**.
   - A muted hint below the row reads
     `1 file still has unresolved conflicts.`
   - `conflict.txt` in the file tree has a yellow `!` decoration
     in the right gutter. Its regular status colour stays
     modified-blue.

### 2. Resolve via the inline widget

4. Click `conflict.txt` to open it. The editor renders the
   conflict block with a soft warning tint and a stronger tint on
   the `<<<<<<<` / `=======` / `>>>>>>>` lines. An inline button
   row appears at the end of the `<<<<<<<` line: **Accept
   current** / **Accept incoming** / **Accept both**.
5. Click **Accept incoming**. Expected: the entire block (markers
   included) is replaced with the lines between `=======` and
   `>>>>>>>` — i.e. `TWO-feature` from the feature branch. The
   editor's caret remains roughly where the click landed; the
   selection isn't lost.
6. Hit **Ctrl+S**. Expected:
   - File saves through the regular format-on-save pipeline.
   - Behind the scenes `git add conflict.txt` runs; the row's
     `!` badge disappears immediately.
   - The merge-hint under the commit row updates to
     `0 file…` (it actually goes away — the hint only renders
     when count > 0).
   - **Commit merge** becomes enabled.

### 3. Commit the merge

7. Click **Commit merge**. Expected:
   - A new merge commit (two parents) lands. The SCM panel
     reverts to its normal shape: pill gone, sync button reappears
     if the branch is ahead of upstream, commit composer is empty,
     toggles (amend, new branch) come back.
   - The flash toast reads
     `Committed <sha>: Merge branch 'feature'` (or whatever the
     composer's bytes were).
   - `git log -1 --pretty=%P` in a terminal shows two parent
     hashes.

### 4. Abort instead

Repeat **Setup** + step 1 to get back into a merge.

8. Without resolving anything, click the **✕** Abort button next
   to **Commit merge**. Expected:
   - `git merge --abort` runs.
   - The SCM panel reverts to its regular shape within one
     fs-watcher tick.
   - `conflict.txt` on disk is back to the `main` branch content
     (no markers). `git status` is clean.
   - The flash toast reads `Merge aborted.`

### 5. Soft-warn on residual marker text

Repeat **Setup** + step 1 to get back into a merge.

9. From a terminal: `git add conflict.txt` **without** opening
   the file. (Edge case: tooling stages the conflicted file
   without resolving it.) The unmerged-paths list goes empty and
   the SCM panel un-disables **Commit merge** within one
   fs-watcher tick.
10. Click **Commit merge**. Expected: a confirm dialog titled
    "Commit merge?" lists `conflict.txt` and asks "Commit
    anyway?". Click **Cancel** — nothing happens, the panel stays
    in merge-mode.
11. Now open `conflict.txt` and click **Accept current**, save,
    then click **Commit merge** again. Expected: no dialog
    (markers are gone), commit lands normally.

### 6. Multi-file resolution

Set up a fresh repo where the merge touches two files (`a.txt`
and `b.txt`). After step 1 the panel hint should read
`2 files still have unresolved conflicts.` Resolving and saving
one drops the count to `1`. The `!` badge clears per-file as you
save.

### 7. Tests

```sh
bun run check && bun run lint && bun run test
```

Expected: clean — five new Rust unit tests
(`map_porcelain_status_recognises_every_unmerged_combination`,
`map_porcelain_status_preserves_existing_priority_for_non_conflict_codes`,
`git_merge_state_returns_default_when_no_merge_in_progress`,
`git_merge_state_surfaces_in_progress_merge_with_unmerged_paths`,
`git_merge_abort_clears_merge_state`) plus four new vitest cases
in `src/lib/editor/conflictMarkers.test.ts` all pass.

## What must keep working

- Regular non-merge commits, amend, commit-to-new-branch, sync,
  publish, "Update from main". The merge-mode reshape is fully
  gated on `gitMergeState.inProgress` and disappears the moment
  the merge ends.
- `git restore` / "Discard changes" stays unchanged for
  `modified` / `deleted` / `untracked` rows. `conflicted` rows
  are **not** offered in the discard menu (you don't discard a
  conflicted file; you resolve or abort).
- Pierre's regular per-row colours (`added` / `modified` /
  `deleted` / `untracked` / `ignored`) — we map `conflicted` to
  `modified` at the Pierre boundary, so existing rows look the
  same.
- The fs-watcher's `.git/HEAD` → branch-switch refresh path.
  The `.git/` whitelist grew to also surface `MERGE_HEAD` /
  `MERGE_MSG`, but `HEAD` still gets through and
  `refreshGitBranch` still fires on it (test plan 0021's
  external-checkout case).
- The editor's git-change gutter (test plan 0033). The conflict
  decorator paints alongside it without conflict — they target
  different surfaces (line-number gutter vs full-line background).
- This very test plan file. It contains `<<<<<<<` markers in its
  body; the editor must not decorate them when the file isn't
  reported as `conflicted` by git.

## Known limitations

- **No `git merge --continue` button** when the user resolved
  files manually and just wants to commit without editing the
  message. Workaround: just click **Commit merge** — the prefill
  is already `MERGE_MSG`, so the bytes are equivalent.
- **No per-block "Edit manually" option** on the accept-widget
  toolbar. The user can just click in the buffer instead — the
  widget is an affordance, not a modal.
- **No `diff3` base-section preview.** Files merged with
  `merge.conflictStyle = diff3` get a third (`|||||||`) section
  that the accept widgets simply discard along with the other
  marker lines. Adding an "Accept base" button is a follow-up if
  anyone asks.
- **Rebase / cherry-pick conflicts** still flash and dead-end.
  The same conflict-marker surface would work — the host just
  needs a `git_rebase_continue` / `git_cherry_pick_continue`
  analogue. Out of scope here; revisit when a real user hits it.
- **No streaming progress for `git merge --abort`.** It's a
  single short subprocess and the panel just shows the spinner
  on the abort button while it runs.

## Related

- Specs:
  [`specs/frontend.md`](../frontend.md) § "Diff and conflict surfaces",
  [`specs/roadmaps/phase-05-git.md`](../roadmaps/phase-05-git.md) § 5.6.
- ADRs: [`specs/decisions/0015-git-serialisation.md`](../decisions/0015-git-serialisation.md)
  (mutex contract that the new host methods inherit).
- Prior test plans (now updated):
  [`0021-file-tree-full-git-status.md`](0021-file-tree-full-git-status.md),
  [`0035-diff-view-codemirror-merge.md`](0035-diff-view-codemirror-merge.md),
  [`0065-scm-update-from-main.md`](0065-scm-update-from-main.md).
