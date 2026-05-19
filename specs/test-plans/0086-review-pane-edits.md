# Test plan 0086: Editable working-tree side in the Review changes tab

- **Date**: 2026-05-19
- **Phase**: post-Phase 5 SCM polish (extends test plan 0074)

## What shipped

- The **Review changes** pseudo-tab is no longer read-only. Each section's **right (working-tree) side** is now editable for `modified`, `added`, and `untracked` rows; `deleted` rows stay read-only (no working-tree bytes to edit). The left (base / `HEAD`) side stays read-only on every row.
- Edits route through `workspace.updateText` against a lazily-attached `OpenFile`. A new `workspace.ensureBackingBuffer(path)` helper opens the underlying file silently — added to `openFiles`, **not** added to any tab strip and **not** made active — on the first keystroke so `updateText` / dirty / fingerprint / LSP `didOpen` all run exactly once per session per file. Subsequent keystrokes chain off the same one-shot promise so a racing `readFile` can't reorder a stale `updateText` over a fresh one.
- New `workspace.saveReviewSection(path)` is the per-section save flow: same `fs.writeFile` + post-format re-read + `lspNotifyAfterSave` + blame refresh as `saveActive`, just keyed off `path` instead of `activeFile`. `Ctrl+S` from inside a review section's CodeMirror editor lands here via a new `reviewFocusPath` workspace pointer: the global `Ctrl+S` handler in `App.svelte` detects when the active tab is a `review://` buffer and a section has focus, then delegates to `saveReviewSection` instead of saving the empty synthetic buffer.
- Section header gains a small green `●` "unsaved edits" pip on any section whose underlying file is dirty in `workspace.openFiles`. Sourced from the same state Ctrl+S writes, so saving clears the pip on the next reactive tick.
- Working-tree side picks up the regular editor's editing stack: `history`, `closeBrackets`, `indentOnInput`, `highlightActiveLineGutter`, plus the standard `defaultKeymap` / `historyKeymap` / `searchKeymap` / `indentWithTab` keymaps and a live editorconfig compartment (`ecB`) reconfigured whenever `workspace.editorConfigFor(path)` changes — so indent / tab settings match the team's `.editorconfig` instead of CM defaults.
- **No LSP wiring in review sections.** Hover, goto-def, diagnostics, completion are deliberately out of scope: review renders N files at once and `didOpen` per file would explode broker traffic on a 50-file branch. Click the section header's path to open the file as a regular tab when LSP affordances are wanted.

## How to test

Prerequisites: any git repo with an `origin/main` (or `origin/master`) the current branch isn't on. The moon-ide repo itself works once you're on a feature branch with a few changed files.

1. Run `bun run check`, `bun run lint`, `bun run fmt`. Expected: clean.
2. Launch `bun dev`. Open a folder that's on a feature branch with at least one modified file, one added file, one deleted file (`git rm <path>`), and one untracked file. Click the SCM review icon to open the **Review changes** tab. Expected: stacked diff sections render as in plan 0074.
3. Click into the **right pane** of a `modified` section and type a few characters. Expected:
   - The caret appears and the characters land inline (CM is editable).
   - The MergeView re-highlights the affected hunk on the next frame (live diff recompute).
   - A green `●` pip appears in the section header next to the path label, with the tooltip `Unsaved edits — Ctrl+S to save`.
   - The file does **not** appear in either pane's tab strip — only the review tab is in tabs.
4. Press `Ctrl+S` while still focused inside the section's right pane. Expected:
   - The dirty pip disappears within a frame or two.
   - The file's bytes on disk now match what you typed (`git diff <path>` in the shell shows your edits as part of the working-tree change).
   - The review section's right pane redraws against the new working-tree content (which is what it already was — no flicker, no scroll jump).
   - Format-on-save still runs: typing trailing whitespace before saving leaves clean bytes on disk if the team's pre-save pipeline (or formatter) strips it. The CM buffer re-syncs to the post-format text.
5. Repeat step 3 + 4 on an **`added`** section. Expected: same behaviour. The left pane stays empty (the merge-base has no version of this file) and your edits land on disk normally.
6. Repeat step 3 + 4 on an **`untracked`** section. Expected: same behaviour as `added`. The right side is editable; saving writes through to disk.
7. Try to type into a **`deleted`** section. Expected: nothing happens — the right side stays read-only (there's no working-tree file to write to). No pip ever appears. The left side (which still shows the `HEAD` content) was already read-only and still is.
8. Try to type into the **left side** of any section. Expected: nothing happens — the left side is always read-only regardless of status. No pip.
9. Edit one file in the review pane, then click another file's section header path (the `dir/name` text). Expected: that file opens as a regular editor tab with the **same dirty bytes** you typed (the underlying `OpenFile` is shared). The original review tab is still open. `Ctrl+S` from the regular editor tab also saves.
10. Edit a section, then **without saving**, click the same file's section header path to open it as a regular tab. Verify the regular tab shows the unsaved edits. Switch back to the review tab — the section still shows the same edits. Edit a few more characters from the review surface, then switch back to the regular tab. Expected: the regular tab's editor reflects the new edits too (single shared buffer).
11. Edit a section, then **don't save**, and click the section's path header to open it as a regular tab. Press `Ctrl+S` from the regular tab. Expected: bytes land on disk; the review section's pip clears.
12. Edit two different sections (e.g. file A and file B). Move focus between them by clicking inside each. Each click should update `reviewFocusPath` to the clicked section. Press `Ctrl+S` while focused inside section A: only A saves (A's pip clears, B's pip stays). Then click into B and Ctrl+S: B saves.
13. Edit a section, then **commit** that file from the SCM panel without closing the review tab. The entry leaves `gitStatusEntries` and the section unmounts. Expected:
    - The dirty pip and the section both disappear from the review view.
    - No stale `reviewFocusPath` afterwards: pressing `Ctrl+S` somewhere else in the review tab is now a no-op (no section to save). The `clearOurFocus` teardown hook handles this.
14. Edit a section, then **close** the review tab via its `×` button. The synthetic `review://` buffer is GC'd. Expected: the underlying file's `OpenFile` is still in `openFiles` if it was attached (lazy attach on first edit), still dirty, still saveable via `Ctrl+S` if you open it as a regular tab. (We don't tear down lazily-attached buffers on review close — they behave the same as buffers the user opened explicitly.)
15. Edit a section in a `.editorconfig`-governed file. Verify that pressing Tab inserts the configured indent unit (tabs or N spaces per `.editorconfig`), not CM's default of two spaces.
16. With a section focused inside the review tab, press `Ctrl+F`. Expected: CM's search panel opens scoped to that section. Press `Escape` to close it. Same for `Ctrl+Z` (undo) — the per-section history works inside the section.
17. Type into a section, then immediately press `Ctrl+Z` a few times. Expected: undo walks back through the typed characters (history compartment is wired). `Ctrl+Shift+Z` redoes.

## What must keep working

Regression checks.

- All the gestures from test plan 0074: lazy mount on `IntersectionObserver`, header collapse/expand, `n` / `p` / Alt-Arrow navigation, SCM-tree click to scroll, baseline toggle remounts sections, `Ctrl+L` adds the section's selection to the coder.
- The synthetic `review://default-branch` buffer is still filtered out of session persistence, LSP `didOpen`, blame, HEAD fetch (`isSyntheticBufferPath` is unchanged).
- The synthetic buffer's `OpenFile.text` stays empty. Edits in a section's right pane do **not** flow into it — they flow into the lazily-attached `OpenFile` for the **real** file path.
- `Ctrl+S` outside the review tab still saves the active file unchanged. `Ctrl+S` inside the review tab when **no** section has focus (e.g. the scroll container itself has focus from `n` / `p` navigation) is a no-op — `saveActive` falls through to its normal branch and the synthetic clean buffer short-circuits.
- Editing a `modified` file from the review pane, then switching to the per-file `DiffView` for the same path (via `Ctrl+Shift+D` after opening the file as a regular tab), shows the same dirty bytes — single shared `OpenFile`.
- The `Ctrl+N` untitled-buffer flow, the `Ctrl+O` external-file flow, and format-on-save for regular editor saves all continue to work exactly as before — `saveActive`'s new review branch is gated by `isReviewPath(file.path)` so non-review saves are untouched.
- Untracked-file edits from the review pane don't accidentally stage the file. The save path is a plain `fs.writeFile` against the working tree; `git status` should still show the file as `??` after save (unless the user runs `git add` explicitly).

## Known limitations

Things we deliberately did not do, with one-line justification.

- **No LSP / hover / completion / diagnostics in review sections.** `didOpen` per file would explode broker traffic on a 50-file branch; the user clicks the section header to open the file as a regular tab when they want LSP.
- **No "save all dirty sections" command.** A user editing 10 sections has to focus and Ctrl+S each one. Easy to add a palette command later (`workspace.openFiles.filter(f => f.isDirty && f.kind === 'text').forEach(saveReviewSection)`) when someone asks.
- **No visual cue that an edit is unsaved on the left (base) side.** Pure mechanical: the left is read-only and the dirty pip lives in the section header; that's enough.
- **No "save and stage" shortcut.** Out of scope for this commit — the SCM panel's existing stage/commit flow still handles that after save.
- **No undo grouping across sections.** Each section's `history()` is independent (one CM editor each). `Ctrl+Z` inside section A doesn't undo edits in section B. Matches how separate editor tabs already behave.
- **Lazily-attached `OpenFile`s aren't garbage-collected on review tab close.** A user who scrolls through 50 sections and edits 5 keeps those 5 in `openFiles` until they close the tabs explicitly (the tabs don't exist in any tab strip, but the buffers do). Acceptable: the same files are usually what the user opens next, and the dirty-flag accounting is correct.

## Related

- Specs: [`specs/frontend.md`](../frontend.md) § "Diff and conflict surfaces" (Review changes bullet updated).
- Prior test plans: [0074](0074-review-changes-tab.md) (the original Review tab — this plan supersedes its "Read-only" known limitation), [0035](0035-diff-view-codemirror-merge.md) (editable working-tree side in `DiffView`, the pattern this plan mirrors), [0036](0036-diff-view-single-tab-toggle.md), [0070](0070-diff-mode-goto-def.md).
- ADRs: none new. [0001 — stack](../decisions/0001-stack.md) covers the "CM for every editable surface" choice this plan extends.
