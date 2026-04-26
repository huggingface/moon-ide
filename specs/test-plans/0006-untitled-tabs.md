# Test plan 0006: untitled tabs + Save As + language re-detection

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- `OpenFile` gains an `isUntitled: boolean` discriminator. The buffer's
  `path` field doubles as the stable identifier for tab arrays / active
  fields / preview-mode / editorconfig caches; for untitled buffers
  it's a synthetic `untitled:N` string (workspace paths never start
  with that prefix, so collisions are impossible). Real-file paths
  stay workspace-relative.
- `WorkspaceState.newUntitledTab(side?)`: creates a fresh untitled
  buffer (`Untitled-N`, empty text, `isDirty=false`), appends it to
  the pane's tab list, sets it active, and pulls editor focus. A
  per-process `untitledCounter` provides `N`; numbering resets on
  app launch because untitled state never persists.
- `WorkspaceState.saveActiveAs()`: opens the native Tauri save dialog
  (default path = workspace root + `Untitled-N.txt` for untitled,
  current absolute path for existing files), validates that the chosen
  path lives inside the workspace, refuses to merge with an already-
  open buffer at the destination, writes the file, re-reads it through
  the existing post-save pipeline (so the dirty fingerprint reflects
  trailing-newline / whitespace transforms), and rebinds the buffer
  in lockstep across `openFiles`, `leftTabs` / `rightTabs`,
  `leftActive` / `rightActive`, and the preview-mode map. The file
  tree is refreshed so the new file shows up without a manual reload.
- `saveActive()` now delegates to `saveActiveAs()` whenever the active
  buffer `isUntitled` — first `Ctrl+S` against a fresh tab opens the
  native dialog, every subsequent save (after the rebind) takes the
  normal write path.
- `WorkspaceState.renameTick` + `lastRename` + `isRename(from, to)`:
  explicit rename signal. `Editor.svelte`'s reactive effect watches
  the tick and, when `file.path` changes, asks `isRename` whether the
  swap was a save-as (preserve view state, swap language) or a tab
  switch (rebuild state). Content-equality detection was rejected
  because the pre-save pipeline (final newline insertion, trailing
  whitespace trim, line-ending normalisation) can leave the
  freshly-read text differing from the live view doc — that would
  silently mis-classify saves as tab switches.
- Editor language extension swaps in place via the existing
  `languageCompartment.reconfigure(...)` after a rename, so an
  untitled buffer saved as `foo.svelte` immediately gets HTML/Svelte
  highlighting (and same for `.ts` → `.svelte`, etc.).
- Persistence: `persistAppState` filters untitled paths out of both
  `open_files_left` / `open_files_right` and the active fields.
  Untitled buffers vanish on restart by design — the user-visible
  contract matches every other editor.
- `Ctrl+N` keybinding (`App.svelte`) and `New File` palette command
  (`commands.svelte.ts`). Both refuse when no workspace is open and
  flash a toast — untitled buffers piggyback on the editor pane
  scaffolding which only renders inside a workspace.
- `Save File As…` palette command. No keyboard shortcut yet
  (Ctrl+Shift+S is the obvious pick but waits for a concrete request,
  per the scope-discipline rule).
- `EditorTabs.svelte`: tab tooltip falls back to `file.name` for
  untitled buffers (so the hover doesn't read `untitled:1`); real
  files keep showing the workspace-relative path.
- `ensureEditorConfig` short-circuits on `untitled:` paths — the
  `.editorconfig` cascade has nothing to anchor to until a real path
  exists. The buffer uses `defaultEditorConfig` until first save.
- New `dialog:allow-save` permission in
  `src-tauri/capabilities/default.json` so the native save dialog is
  reachable from JS.

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
