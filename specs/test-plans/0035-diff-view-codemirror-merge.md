# Test plan 0035: Diff view on `@codemirror/merge` with editable working tree

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git)

## What shipped

- Diff engine swap: `DiffView.svelte` is now backed by `@codemirror/merge`'s `MergeView` instead of `@pierre/diffs`. Both sides are full CodeMirror editors that share the regular editor's chrome — language extension, theme, editorconfig, highlight-tabs, line numbers, search, history, brackets — so the diff feels like the rest of the IDE rather than a separate widget.
- Right side (working tree) is **editable**. Edits route through `workspace.updateText` → mark the diff tab dirty → `Ctrl+S` writes back to `realPath`. If a regular editor tab for the same file is also open, it's re-synced from the canonical bytes after save.
- `F7` / `Shift-F7` step between diff chunks (CodeMirror's `goToNextChunk` / `goToPreviousChunk`); revert-arrow controls between chunks copy text from `HEAD` into the working tree.
- Long unchanged regions collapse into clickable separators (`collapseUnchanged: { margin: 3, minSize: 4 }`) so big files don't dump megapixels of identical text.
- `@pierre/diffs` is removed from `package.json`; ADRs / specs updated to point at the new engine.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a bound git-repo folder (moon-ide itself works).

### Cold-open latency

1. Modify a medium file (`specs/roadmap.md` with a few edits is fine), close all tabs.
2. Right-click the row → **View diff**. The diff tab should paint within ~100–200 ms — noticeably snappier than the previous Shiki-backed render.
3. Open a 1 KLOC source file with edits in 5–6 places. Same: paints fast, scrolling stays smooth.

### Edit on the right side

1. Open a diff tab for a tracked, modified file. The `(diff)` tab title should not show a dirty dot yet.
2. Click into the right side and type. The dirty dot appears on the diff tab. The chunk under the caret repaints (no longer green-as-modified once it matches HEAD; new red wedge if you delete a line).
3. Press `Ctrl+S`. Tab dirty dot clears; the right side now matches disk. Check externally with `git diff <path>` — the new content is on disk.
4. With the same file's regular editor tab also open, repeat: edit on the diff tab, save. The editor tab's buffer should reflect the saved bytes (no stale text after a tab swap).
5. Reverse direction: edit in the editor tab, save. Switch to the diff tab — the right side reflects the new bytes. (The unsaved-edit live mirror from plan 0034 still works too: edit in the editor tab _without saving_, switch to the diff tab, the right side updates.)

### `Ctrl+S` on a deleted-file diff tab

1. `git rm --cached <path>` or otherwise produce a deleted row. Click it.
2. Confirm both sides are read-only — typing on either side does nothing, no dirty dot. `Ctrl+S` is a no-op (the buffer isn't dirty).

### Hunk navigation & revert

1. On a diff tab with several scattered edits: press `F7`. Caret jumps to the first chunk's start. Press `F7` again — second chunk. `Shift-F7` walks back.
2. Hover between two chunks: a tiny revert arrow appears in the gutter strip. Click it. The selected chunk on the right side is overwritten with the matching HEAD content. The chunk highlight clears (the right side now matches HEAD for that range). Press `Ctrl+S` to persist.
3. Revert all chunks one by one. The diff tab should end up empty (no green / red strips), tab dirty dot still set until you save.

### Theme + editorconfig + language flips

1. Toggle the IDE theme (status bar). Both sides repaint with the new theme; syntax highlighting stays consistent. No remount (caret position on the right side survives).
2. Open a TypeScript file's diff tab. Syntax highlighting should match the regular editor's TS rendering on both sides.
3. Save a `.editorconfig` change that flips `indent_size` for the file in the diff tab. The right-side `Tab` key picks up the new indent — verify by typing `Tab` and observing the inserted whitespace width. Left side updates `tabSize` too (you can see indentation alignment shift if HEAD content has tabs).

### HEAD refresh after external git ops

1. With a diff tab open, run `git commit -am "wip"` in an external terminal. The fs watcher fires, `refreshGitStatus` runs, `headByPath` re-fetches HEAD for open files. The diff tab's left side should swap to the new HEAD; chunks vanish (the working tree now matches the new HEAD).
2. `git reset --soft HEAD~1` from the same terminal. Left side swaps back to the previous HEAD; chunks reappear.

### Session restore

1. Open a diff tab for a modified file, type a few edits without saving, quit the IDE.
2. Relaunch. Diff tab returns. Right-side text is whatever was last on disk for that path (we don't persist unsaved diff-tab edits across restarts — same shape as a regular editor tab without auto-save). Left side reloads from HEAD.
3. If the real path was deleted between sessions, the diff tab opens with an empty right side. No crash.

### Alt+Arrow nav & focus

1. From a clean editor tab, open a diff tab, press `Alt+Left`. Focus returns to the editor tab. `Alt+Right` re-focuses the diff tab and lands the caret on the right side (editable side gets `view.focus()`).
2. Confirm all of the keyboard plumbing from plan 0034 still holds — no leak through to CM word-motion, text inputs in the palette still get native word-jump.

## What must keep working

- Plan 0034 in full: dedicated diff tabs, `openDiffTab` idempotency, deleted-file single-tab behaviour, palette / context-menu visibility, session restore, `Alt+Left` / `Alt+Right` swallow.
- Plan 0033's git-change gutter + overview ruler on regular editor tabs (separate codepath, same `headByPath` cache).
- Inline blame (plan 0029) and goto-definition / nav history (plans 0027 / 0028) on regular editor tabs — diff tabs intentionally don't host these.
- Tab dirty dot rendering (any open tab with `isDirty: true` shows it in the tab strip — diff tabs now reach this state).
- `bun run check`, `bun run lint`, `cargo check`, `cargo clippy --all-targets -- -D warnings` all clean.

## Known limitations

- No conflict-marker UI yet (`<<<<<<<` / `>>>>>>>` blocks render as plain text on both sides). When we wire conflict resolution we'll layer that on top of the same `MergeView`.
- The right side has no LSP wiring. Diagnostics, hover, completion, goto-definition all live on the regular editor tab — open it side-by-side or `Alt+Left` to it. We avoided opening the same path through two CM views with separate `didOpen` round-trips; revisit if the team actually wants in-place LSP on the diff side.
- Inline blame isn't on the diff tab either, by the same reasoning. The right side is a working buffer; blame is a regular-editor concern.
- The revert-arrow control only flows HEAD → working tree (`revertControls: 'a-to-b'`). No "promote my edit into HEAD" — that would mean staging, which we haven't built.
- The diff tab's title still uses a `(diff)` suffix; replacing it with a dedicated icon comes with the broader tab-chrome pass.
- `goToNextChunk` / `goToPreviousChunk` are bound to `F7` / `Shift-F7` to match CodeMirror's reference example. We can revisit if the team prefers `Alt+J` / `Alt+K` or similar; both keys are unused in the rest of the IDE today.

## Related

- `specs/test-plans/0034-diff-view-dedicated-tab.md` — dedicated-tab refactor; this plan continues that line by swapping the engine and making the right side editable.
- `specs/test-plans/0033-git-change-gutter.md` — in-editor git-change gutter (separate code path; both pull from `headByPath`).
- `specs/test-plans/0032-git-diff-view.md` — original Pierre-based diff view; superseded by 0034 + this plan.
- `specs/decisions/0001-stack.md` — diff-view dependency entry now reads `@codemirror/merge`.
- `specs/roadmap.md` — Phase 5 bullet rewritten for the engine swap and editable working-tree side.
