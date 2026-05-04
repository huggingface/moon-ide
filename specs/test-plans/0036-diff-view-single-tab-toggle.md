# Test plan 0036: Single-tab diff view with mode toggle

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git)

## What shipped

- Diff view goes back to **one tab per file** with a mode toggle. The dedicated-diff-tab model (plans 0034 / 0035) is gone: no synthetic `moon-diff:` paths, no `OpenFile.isDiffTab` / `realPath`, no dual-buffer fanout in `saveActive`. The diff and editor views share a single `OpenFile.text` buffer.
- `workspace.diffModes: Set<string>` (per-folder, like `previewModes`) controls which open paths render in `DiffView` instead of `Editor`. Deleted files are always in diff mode (no editor counterpart). Mode is transient — cleared on close, folder swap, and not persisted across sessions.
- Five toggle surfaces, all funneling through `setDiffMode` / `toggleDiffMode`:
  1. **Tab toolbar** (top-right of the strip): a `Source` / `Preview` / `Diff` tri-state group. `Preview` shows for markdown, `Diff` shows when the file's git status is `modified`.
  2. **Keybind** `Ctrl/Cmd+Shift+D`.
  3. **Palette** entry `Git: Toggle Diff View` / `Git: Hide Diff View` (title flips with mode).
  4. **File-tree context menu** `View diff` (still there for files not yet open; flips diff mode + opens the file).
  5. **Click on a per-line gutter marker** in the regular editor (added / modified / deletion wedge) — flips the buffer into diff mode for that line in context.
- LSP, inline blame, goto-definition, navigation history, and editorconfig all keep working unmodified across the toggle: there's only one CM editor mounted at a time (Editor or DiffView for that path), and both route their edits through `workspace.updateText`. The single shared buffer means `Ctrl+S` works the same in either mode.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a bound git-repo folder with at least one commit.

### Tab toolbar toggle

1. Open a clean file. The right-edge tab toolbar shows nothing (no `Diff` button, no `Preview` unless markdown).
2. Edit the file so its tree row flips to `modified`. The toolbar grows a `Source` / `Diff` button group, with `Source` selected.
3. Click `Diff`. The pane swaps to the side-by-side merge view. `Source` ↔ `Diff` swaps in the toolbar.
4. Click `Source`. Editor returns; caret is at the position you left it (the buffer is the same `OpenFile`).
5. Open a markdown file with edits. Toolbar shows `Source` / `Preview` / `Diff`. Cycling through all three lands in the right view each time. From `Preview` directly to `Diff`: `Preview` deselects, `Diff` activates, MergeView paints.
6. Open a `.png`. No toolbar — the tri-state hides for non-text and untitled buffers.

### Keybind

1. Modified file, editor focus. Press `Ctrl+Shift+D`. Diff view appears, focus lands in the right-side editor.
2. Press `Ctrl+Shift+D` again. Editor returns, caret in place.
3. On a clean file: press `Ctrl+Shift+D`. Nothing happens (no diff to show); the press is swallowed (event isn't bubbled up to the browser).
4. On a deleted-file tab: press `Ctrl+Shift+D`. Nothing happens (deleted is permanently in diff mode).
5. On an untitled buffer: same — nothing.

### Palette

1. Active modified buffer in editor mode: open the palette (`Ctrl+Shift+P`), search `Git`. The entry reads **Git: View Diff**. Run it. Pane swaps; reopening the palette now shows **Git: Hide Diff View**.
2. On a clean / added / untracked / ignored / deleted / untitled buffer: the entry is hidden.

### File-tree context menu

1. Right-click a clean file → no `View diff` entry.
2. Right-click a modified file → `View diff` entry. Click it. The file opens (if not already), focus snaps to it, the diff view is shown. Calling it a second time (file already open in editor mode) flips the same buffer to diff mode.
3. Right-click a deleted row → `View diff` is shown (and is functionally a no-op — deleted is already diff mode); clicking it just opens the (always-diff) tab.

### Gutter-marker click

1. Modified file, editor mode, scroll to a hunk. Click on the green `added` / blue `modified` bar in the dedicated git-change gutter (the narrow strip just left of the line numbers). Pane flips to diff view.
2. Click on a `deletion wedge` (the red triangular tick at the boundary of a removed block). Same — flips to diff view.
3. Click on a clean row (the gutter shows the spacer, no marker). Nothing happens — the click falls through to CodeMirror's default gutter behavior (no caret movement, nothing flipped).
4. The overview ruler on the right edge of the scrollbar still scrolls to the clicked line (its previous behavior); only the per-line gutter is the diff-mode trigger.

### Edits on the right side

1. In diff mode, click into the right pane and type. Tab dirty dot lights up. The chunk under the caret repaints: a previously `modified` chunk that now matches HEAD turns clean; new edits surface fresh chunks.
2. Press `Ctrl+S`. Tab dirty dot clears, bytes hit disk, `git diff <path>` (external) shows the new content.
3. Flip to `Source`. Editor view shows the same edits — same buffer.
4. Flip back to `Diff`. The chunks match the post-save state.

### Theme + editorconfig + language

1. Toggle the IDE theme. Both diff sides repaint; caret + selection on the right side survive (no remount).
2. Save a `.editorconfig` change touching the active file's `indent_size`. The right side picks up the new indent on the next `Tab`. The left side updates `tabSize` (visible if HEAD content has tabs).

### HEAD refresh

1. With a buffer in diff mode, run `git commit -am wip` externally. `refreshGitStatus` re-fetches HEAD; the left side updates to the new HEAD; chunks on the right collapse since the new HEAD matches.
2. `git reset --soft HEAD~1` externally. Left side reverts; chunks reappear.

### Deleted file

1. `git rm --cached <path>` (or external `rm`). Click the deleted row. A single tab opens; both sides are read-only; left = HEAD, right = empty.
2. Toolbar tri-state is hidden (deleted has no editor mode). `Ctrl+Shift+D` is a no-op.
3. Discard the deletion (file-tree context menu). The next status refresh removes the row; the open tab auto-reloads as a regular editor view of the restored file.

### Session restore

1. Open a buffer, flip to diff mode, quit the IDE.
2. Relaunch. The buffer is restored in **editor mode** (diff is transient). The user can flip back into diff mode in one keystroke if they want.
3. Deleted-file tabs return in diff mode (forced by `isDeleted`).

### LSP + blame + goto-def keep working

1. In editor mode, hover over an identifier — LSP hover popover appears. Ctrl/Cmd-click — goto-def jumps. Both still work.
2. Inline blame badge sits at end of the caret's line. Hovering it opens the commit tooltip.
3. Flip into diff mode. **LSP affordances also work on the right pane**: hover popovers, Ctrl/Cmd-click goto-def, completion (manual trigger via `Ctrl-Space`), and diagnostics squigglies are all wired into the right-side editor. Edits route through the same `workspace.updateText` → `lspScheduleUpdate` path, so as you type the LSP server stays in sync.
4. Inline blame is **not** wired into the diff view (the blame extension hooks into a single CM view; we keep it on editor mode only). Flip back for blame.
5. Flip back to editor. All editor-side affordances return immediately, no state was lost.
6. Open a fresh untitled file. Tri-state is hidden. LSP / autocomplete on a typed `untitled:` TS buffer works as before.

## What must keep working

- All prior Phase-5 gestures: tree markers (0020), refresh on fs events (0021), discard (0022), inline blame + PR linking (0029), git-change gutter + overview ruler (0033). The gutter overview ruler's scroll-to-line click survives.
- Markdown preview toggle (0004) for non-modified markdown files. For modified markdown files, all three views (Source / Preview / Diff) are reachable from the new tri-state.
- Goto-definition + navigation history (0027 / 0028) on regular editor tabs.
- Tabs: drag, reorder, drag-between-panes, close, persistence, Save As, Untitled-N — all unaffected by the toggle.
- `bun run check`, `bun run lint`, `cargo check`, `cargo clippy --all-targets -- -D warnings` clean.

## Known limitations

- Toggle is transient. We considered persisting it (like markdown preview's `previewModes` is) but diff is more of a "right now I want to see what changed" gesture; persisting it would mean a buffer the user left in diff mode three days ago re-opens to a `MergeView` instead of an editor, which is rarely what they want. Trivial to flip if the team disagrees in practice.
- Inline blame doesn't render on the diff view (the blame badge widget assumes a single non-merge CM view); flip back to editor mode for blame.
- `revertControls: 'a-to-b'` only goes HEAD → working tree. No "promote my edit into HEAD" — that's staging, which we haven't built.
- The Diff button reads as "Diff" rather than a richer label like "Changes" or an icon. Consistent with `Source` / `Preview`'s plain-text style; we'll revisit when icons land.
- F7 / Shift-F7 (next/prev chunk) only fire when the right-side editor has focus. Outside it (e.g. focus is in the left pane), they don't.

## Related

- `specs/test-plans/0034-diff-view-dedicated-tab.md` — dedicated-tab approach. Superseded by this plan; the model proved noisier than helpful in practice (every modified file produced two tabs, save bookkeeping fanned out across both buffers, and LSP couldn't ride the diff side without synthetic-URI hoops).
- `specs/test-plans/0035-diff-view-codemirror-merge.md` — engine swap to `@codemirror/merge`. Engine is unchanged in this plan; only the wrapping (single tab + toggle) moves.
- `specs/test-plans/0033-git-change-gutter.md` — the gutter the new click handler attaches to.
- `specs/test-plans/0032-git-diff-view.md` — original Pierre-based diff view with `diffModes` toggle. The shape we're back on, with a different engine and richer toggle UI.
- `specs/decisions/0001-stack.md` — diff-view dependency entry already points at `@codemirror/merge`.
- `specs/roadmap.md` — Phase 5 bullet rewritten for the single-tab + toggle model.
