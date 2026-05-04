# Test plan 0033: Git-change gutter in the editor

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git)

## What shipped

- A dedicated CodeMirror gutter that paints per-line git change markers (green bar for additions, blue bar for modifications, red wedge for deletions) in the regular editor view.
- A thin **overview ruler** pinned to the editor's right edge, overlaying the native scrollbar, showing every change in the file at a scaled-down position. Markers are clickable — a click jumps the viewport to that line, centred.
- Per-file `HEAD` blob cache (`WorkspaceState.headByPath`) fed by the existing `fs_git_head_content` command; lazy-seeded on first activation and re-fetched whenever `refreshGitStatus` runs, so external `git commit` / `checkout` work flows back into the gutter.
- New extension `src/lib/editor/gitChanges.ts` uses `jsdiff::diffLines` in a StateField that recomputes on every transaction, so markers stay in sync while the user types.
- New dependency: `diff` (`jsdiff`).

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a bound folder that is a git repo with at least one commit. The moon-ide repo is the easiest target.

### Happy path — live edit

1. Open any committed file (e.g. `src/styles.css`).
2. Confirm the gutter between the line numbers and the editor content is blank.
3. Add a new line somewhere in the middle: a **green bar** appears to the left of the line.
4. Modify an existing line (change a character): that line's bar flips to **blue**.
5. Delete a line: a **small red wedge** points down from the line above (or up from the line below, if the deletion was at end-of-file).
6. Undo back to the committed state. All markers disappear within the same frame — the StateField re-runs on the undo transaction.

### Multi-line edits

1. Select 5 consecutive lines. Delete them. A red wedge anchors to the line that now follows the removed block.
2. Paste 3 new lines in the same position. The 3 new lines turn **blue** (modification = added adjacent to removed) and the leading red wedge goes away.
3. Insert 2 more lines right below. The 3 originals stay blue; the 2 new ones are **green**.

### `HEAD` refresh on external git operations

1. With a modified file open and visibly coloured, `git stash` in an external terminal.
2. Trigger a refresh from the palette (**Refresh File Tree**) or let the filesystem watcher pick it up. The gutter empties — the working tree now matches `HEAD`.
3. `git stash pop`. Refresh again. Markers come back.
4. Amend a commit (`git commit --amend --no-edit`) and refresh. The gutter recomputes against the new `HEAD`; any lines that were flagged as "modified since HEAD" now resolve as clean.

### Deleted / special buffers

1. Delete a tracked file from the file tree (**Discard changes** → confirm). The tab re-opens in diff view (prior test plan `0032`). Confirm the editor gutter is **not** rendered — only the diff view is visible for deleted buffers.
2. Open an untracked file. The gutter is blank and stays blank after edits (no `HEAD` side to compare against).
3. Open an untitled buffer (`Ctrl+N`). Gutter is blank.
4. Open a file inside a folder that isn't a git repo. Gutter is blank. No errors in the dev console / Rust log.

### Overview ruler

1. With a modified file open, a column of tiny coloured ticks appears on the right edge of the editor (overlaid on the scrollbar track). Green / blue / red map to the same semantics as the gutter.
2. Hovering a tick fattens it slightly (visual cue that it's interactive).
3. Click a tick. The editor scrolls so the corresponding line is centred, and the caret lands on that line.
4. Scrollbar drag / wheel scroll still work normally — the overlay passes pointer events through except directly on the markers themselves.
5. On a file with many changes, ticks stack at the same approximate y-position; that's fine — the overview is a heat map, not a precise list.

### Visual polish

1. Dark → light theme flip: marker colours stay visible on both backgrounds (green/blue/red are semantic tokens resolved via CSS vars).
2. The gutter width is constant whether or not there are markers — the editor content doesn't shift when the first change appears.
3. Active-line highlight still reads on rows that also have a git-change marker.

## What must keep working

- LSP diagnostics lint gutter (`0024`) remains on the left; git-change gutter sits between it and the editor content.
- Inline blame on the caret's current line (`0029` / `0031`).
- Diff view toggle and deleted-buffer routing (`0032`).
- Tree markers, discard changes, refresh-on-fs-event (`0020` / `0021` / `0022`).
- Session restore, dual split, tab reordering — nothing here is tab-local beyond the extension attached to the editor view.
- `bun run check:ts`, `check:svelte`, `lint:js`, `lint:rust`, `cargo check` all pass without new warnings.

## Known limitations

- The diff is line-level only. Intra-line changes (a character swap inside an otherwise-unchanged line) paint the whole line blue; that's fine for a glance-level indicator.
- Nothing interactive from the gutter yet: no "click to revert this hunk", no hover popover showing the removed text. Revert lives on the tree row's context menu (file-level) and the diff view is the go-to for per-line detail.
- `HEAD` re-fetch piggybacks on `refreshGitStatus`. If a `git commit` runs in an external terminal and nothing invalidates the file tree (no file watcher event landing, window not refocused), the gutter stays stale until the next refresh trigger. The palette **Refresh File Tree** always recovers.
- Pure-deletion wedges only indicate _that_ lines were removed, not how many. For the count / content, the diff view is one context menu away.

## Related

- `specs/test-plans/0032-git-diff-view.md` — companion diff view that reuses the same `git_head_content` command.
- `specs/test-plans/0029-inline-git-blame.md` — same in-editor-gutter design family.
- `specs/roadmap.md` — Phase 5 Git section.
- Dependency: [`diff` (jsdiff)](https://www.npmjs.com/package/diff).
