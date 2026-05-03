# Test plan 0006: untitled tabs + Save As + language re-detection

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- `Ctrl+N` / `New File` palette command creates an untitled
  tab (`Untitled-N`) in the focused pane. Untitled buffers use
  a synthetic `untitled:N` path internally and never persist —
  they vanish on restart by design.
- First `Ctrl+S` on an untitled tab detours through the native
  Save As dialog; `Save File As…` palette command covers the
  same flow on real files. Saves are refused outside the
  workspace, and refused when they'd collide with another
  open buffer.
- Save-as rebinds the buffer across open-file maps, tab arrays,
  active-side pointers, preview-mode, and editorconfig caches
  in lockstep. Editor view state (cursor, undo) is preserved;
  the language extension swaps via the existing
  `languageCompartment` so renames pick up the new filetype's
  syntax highlighting immediately.
- New `WorkspaceState.renameTick` + `isRename(from, to)` gives
  `Editor.svelte` a reliable signal to distinguish save-as from
  a tab switch (content-equality would misclassify, because the
  pre-save pipeline canonicalises bytes).

## How to test

Prerequisites: `bun install`, host deps installed per `README.md`,
`bun run dev` running.

1. Open a folder. Press `Ctrl+N`. Expected: a new tab labelled
   `Untitled-1` appears in the focused pane, editor focus is in it,
   no dirty marker yet.
2. Type any text (e.g. `let x = 1`). Expected: dirty dot appears on
   the tab; tooltip on the tab reads `Untitled-1`, not `untitled:1`.
3. Press `Ctrl+S`. Expected: native save dialog opens with the
   workspace as the starting directory and `Untitled-1.txt` prefilled.
   Choose `foo.ts` inside the workspace. Expected:
   - Tab label updates to `foo.ts`, dirty dot clears.
   - Selection / cursor position is preserved (no jump back to
     start-of-file).
   - The file appears in the file tree (refresh happens
     automatically).
   - Syntax highlighting kicks in for TypeScript on the existing
     buffer (no rebuild → undo history still works).
4. With `foo.ts` active and dirty, press `Ctrl+Z` until undo runs
   out. Expected: every keystroke since the original Ctrl+N is
   undoable, the dirty marker reverts when the buffer matches the
   on-disk bytes, and the tab stops being dirty when undo reaches
   the saved baseline (or stays dirty if pre-save pipeline added a
   final newline — that's the canonical bytes, not a regression).
5. Press `Ctrl+N` again. Expected: `Untitled-2`. Type something.
   Without saving, press the close button (or `Ctrl+W`). Expected:
   the discard prompt fires (`Untitled-2 has unsaved changes.
Discard them?`); cancel keeps the tab, discard drops it.
6. Open `Save File As…` from the command palette while focused on a
   real file. Expected: native save dialog defaults to the file's
   current absolute path. Pick a different path with a different
   extension (e.g. `bar.svelte`). Expected: buffer rebinds to
   `bar.svelte`, language extension swaps to HTML/Svelte
   highlighting, the original file on disk is left untouched, and
   the file tree shows the new file.
7. Try `Save File As…` to a path **outside** the workspace.
   Expected: toast "Save target must be inside the current
   workspace." and no file is written.
8. With the same buffer open in both split panes, run `Save File
As…` and pick a path that's already open as another tab.
   Expected: toast "A buffer for `<path>` is already open. Close
   it before saving here." (refuses the merge).
9. Open one untitled tab and one real file, then reload the window
   (`Ctrl+R`, dirty prompt, accept). Expected: only the real file
   tab comes back; the untitled buffer is gone (untitled state
   never persists).
10. Without an open workspace (close folder if needed), press
    `Ctrl+N` or run `New File`. Expected: toast "Open a folder
    before creating a new file." and no untitled tab is created.

## What must keep working

Regression checks. If any of these break, the commit needs a follow-up.

- `Ctrl+S` on a real, dirty file still saves through the existing
  write path with no dialog (only untitled buffers detour through
  save-as).
- The post-save fingerprint refresh still works (saving twice in a
  row with no edits stays clean — the pre-save pipeline canonicalises
  the bytes once, the second save short-circuits on `!isDirty`).
- The `.editorconfig` reload after saving a `.editorconfig` still
  fires (refreshes every open file's resolved settings).
- The dirty-discard prompt still gates closing dirty tabs on both
  panes (untitled and real).
- Tab drag-and-drop (within and between panes) still works for tabs
  that include an untitled buffer — synthetic paths flow through
  `moveTab` like any other.
- Image tabs are not offered Save As (refused with a toast). They
  remain read-only previews.

## Known limitations

Things we deliberately did not do, with one-line justification.

- No `Ctrl+Shift+S` keybinding for Save As yet — wait for a concrete
  team request, per the scope-discipline rule.
- No multi-root / out-of-workspace saves. Picking a path outside the
  current workspace is refused with a toast; multi-root support is
  Phase 7's problem.
- No "Save All" command — wait for an ask. Today the editor pane
  only edits one buffer at a time, so the gain over per-tab Ctrl+S
  is theoretical.
- No language picker UI for untitled buffers before save. Until you
  save with an extension, there's no syntax highlighting; once you
  pick `foo.ts` it appears. Adding an explicit picker is speculative
  until typing in a fresh tab without highlighting becomes a
  complaint.
- The synthetic `untitled:N` path is visible in the buffer's
  identifier internally but never surfaced to the user (tab label,
  tooltip, status bar all use `Untitled-N`).
- Untitled tab numbering (`Untitled-N`) doesn't reuse freed numbers
  — closing `Untitled-2` and opening a new one yields `Untitled-3`.
  Matches the VS Code / Cursor convention; not worth the bookkeeping
  for tighter numbering.

## Related

- Specs: [roadmap.md](../roadmap.md) Phase 1.5 acceptance bullet
  "New untitled tab" + "Save-as / language re-detection on rename".
- Prior test plans: [0003-per-pane-tabs.md](0003-per-pane-tabs.md)
  for the per-pane tab list this builds on,
  [0002-editorconfig.md](0002-editorconfig.md) for the pre-save
  pipeline that runs on every save (untitled or not).
