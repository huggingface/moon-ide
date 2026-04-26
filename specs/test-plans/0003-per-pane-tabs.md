# Test plan 0003: per-pane open file lists

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- `WorkspaceState.openFiles` keeps every loaded buffer (one
  `OpenFile` per path), but tab order is now stored separately in
  `leftTabs` and `rightTabs`. The two arrays are independent.
- `WorkspaceState.openFile` adds the path to the focused pane's tab
  list if it isn't already there, and only that pane.
- `WorkspaceState.closeFile(path, side)` removes the path from one
  pane only. The dirty-discard prompt is skipped when the buffer is
  still open in the other pane (no data is at risk). Buffers that
  fall out of every pane are GC'd from `openFiles`.
- `WorkspaceState.moveFile(from, before, side)` reorders within one
  pane only.
- `WorkspaceState.splitActive` mirrors only the focused pane's
  active tab into the new pane (one tab to start; the user opens
  more from the file tree).
- `WorkspaceState.closeSplit` clears `rightTabs` and GCs any buffers
  it owned exclusively.
- `EditorTabs` renders `workspace.tabsFor(side)` instead of the
  shared list. Tab drag is locked to its source pane (drag-between-
  panes is intentionally not supported yet — drops from the other
  pane silently no-op).
- `WorkspaceSession` schema replaced `open_files` with
  `open_files_left` + `open_files_right`. Restore loads each path
  exactly once (Set deduplication) and reconstructs each pane's
  tab order from its own list.

## How to test

Prerequisites: `bun install`, host deps installed per `README.md`,
`bun run dev` running.

1. Open the moon-ide repo. Open three files in left pane (e.g.
   `README.md`, `src/App.svelte`, `Cargo.toml`).
2. `Ctrl+Shift+P → Split Editor Right` (or `Ctrl+\`). Expected: the
   right pane appears showing the previously-active tab as its only
   tab. Left pane still has all three.
3. Click a different file in the file tree while right is focused.
   Expected: it opens in the right pane and is **not** added to
   left's tab list.
4. Drag tabs around in left strip. Expected: order changes only on
   the left; the right strip is untouched.
5. Close a tab on left that is also open on right (the seed tab
   from the split). Expected: it closes only on left, no discard
   prompt even if it's dirty (the right pane still has it).
6. Make the seed tab dirty (type something). Switch focus to left,
   close it on left. Expected: closes silently (still on right).
   Close the right copy. Expected: discard prompt fires (it was
   the last copy).
7. Close every right-pane tab one by one. Expected: at the last
   close, `closeFile` should still leave the split open with an
   empty pane (we don't auto-close the split). Or if you call
   `Ctrl+\` again, the split closes and only-right-pane buffers
   fall out of memory.
8. Open a file, split, drag a tab off the right strip onto the
   left strip. Expected: nothing happens (drag-between-panes is
   intentionally not implemented). Drop on the same strip works
   as before.
9. Quit the app. Reopen. Expected: each pane comes back with the
   same tab order; `focused_side` is preserved; the buffer for a
   path that was open in both panes loads exactly once but
   appears in both strips.

## What must keep working

- `Ctrl+W` closes the active tab in the focused pane only.
- `Ctrl+S` save still reaches the right buffer regardless of which
  pane is focused (test by typing on right, saving, verifying disk).
- Theme toggle, command palette, file search, content search.
- Editorconfig honoring (test plan 0002): `Tab` insert is right per
  pane regardless of which pane is focused.
- Image previews still work in either pane.
- `cargo test --workspace --exclude moon-desktop` passes.

## Known limitations

- **Drag-between-panes is not implemented.** Spec calls for it as a
  follow-up; drop is silently rejected today. We add it when there's
  a concrete request.
- **Splitting with zero open tabs** leaves the new pane on the
  Welcome screen. Edge case nobody hits in practice; left as-is.
- **No "open in other pane" command.** Adding one is trivial but
  speculative; surface when a workflow needs it.

## Related

- Specs: `specs/roadmap.md` (Phase 1.5 — Per-pane open file lists),
  `specs/frontend.md`.
- ADRs: none directly.
- Prior test plans: `0002-editorconfig.md`.
