# Test plan 0053: diff view shows full file + change-bar gutter and overview ruler

- **Date**: 2026-05-06
- **Phase**: 1.5 (editor polish). Single component change; no IPC, no persisted state.

## What shipped

- Diff view (`Ctrl+Shift+D`, click a tree row in SCM-filter mode, click a change marker in the editor gutter) no longer collapses unchanged regions. The full file renders on both sides.
- On open, the right pane scrolls to the **first change chunk** (centred). For a 3000-line file with one edit at line 2500 the user lands at the change instead of line 1.
- Right pane now carries the same `gitChangesExtension` as the regular editor:
  - **Change-bar gutter** — per-line green/blue/red bars + deletion wedges, identical glyphs to the editor. Lives inside the editor frame and scrolls with the code (same as the regular editor).
  - **Overview ruler** — thin clickable strip pinned to the **merge view's** scrollbar gutter (right edge of the diff host) mapping every change line onto a scaled-down position. Click a marker → cursor moves to that line and centres. The overview is re-parented from `.cm-editor` to `.cm-mergeView` via the new `overviewMountFacet` because the merge package forces `.cm-scroller` to `height: auto / overflow-y: visible` and scrolls on the outer container — without the re-parenting the strip would render at doc height and scroll with the code instead of pinning to the viewport.
- HEAD updates (commits, fs-watcher firing after `git checkout`) reach both the left pane's doc and the right pane's overview, so the markers stay in sync without remounting the view.
- Deleted-file diffs (right pane empty, read-only) skip the change gutter and overview — there's no working tree to chart.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, an active folder that's a real git repo with at least one file modified relative to HEAD.

### Auto-scroll to first chunk

1. Make a tiny edit far down a long file: open `src/lib/state.svelte.ts`, jump to ~line 2000, append a comment, save.
2. `Ctrl+Shift+D` to open the diff view.
3. Expected: the editor opens scrolled to the chunk around line 2000, not the top of the file. The change is visible without manual scrolling.
4. The cursor lands on the chunk's first line.

### No collapsed placeholders

1. Same diff view as above. Scroll up.
2. Expected: every line of the file is rendered. There is **no** `… N unchanged lines` placeholder anywhere.
3. `Ctrl+F` (search) finds matches in unchanged regions — they're real document content now.

### Change-bar gutter on the right pane

1. Look at the right pane's gutter (between line numbers and content).
2. Expected: the same per-line green / blue / red glyphs you see in the regular editor. Pure additions are green, replacements are blue, deletions show a red wedge at the line boundary.
3. Switch back to the regular editor (`Escape` in the diff view). The same lines carry the same glyphs.

### Overview ruler

1. In the diff view, look at the right edge of the right pane.
2. Expected: thin coloured marks reflecting where every change is in the file, mapped onto the editor's vertical extent.
3. Click any marker. The right pane scrolls to that line, the cursor lands on it.
4. Hover a marker — it widens slightly (matches the regular editor's overview behaviour).

### HEAD update sync

1. Diff view open on a modified file.
2. From a terminal, `git add <file> && git commit -m 'wip'` (or `git checkout -- <file>` to revert).
3. Within a watcher tick, expected:
   - The left pane updates to the new HEAD content.
   - The right pane's change-bar gutter and overview ruler repaint against the new HEAD — markers vanish for a clean revert, change to "modified" wherever HEAD now differs from the buffer.

### Regression checks

- `F7` / `Shift-F7` still hop chunk-to-chunk inside the diff view.
- `Escape` still flips back to the editor view.
- Edits in the right pane still mark the buffer dirty and propagate through the format-on-save pipeline.
- Deleted-file diff (open a deleted file from the SCM panel) shows the HEAD content on the left and an empty right pane, no overview ruler, no gutter — no console warnings about `headTextFacet` not being installed.

## Known limitations

- The overview ruler only paints HEAD-vs-current changes, same as the regular editor's. It does not annotate diff-mode-specific things like merge conflicts (we don't render conflicts yet).
- No "minimap" view — the overview ruler is a thin strip with discrete marks, not a thumbnail of the file.
