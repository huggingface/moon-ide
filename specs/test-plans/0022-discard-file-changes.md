# Test plan 0022: discard file changes from the file tree

- **Date**: 2026-05-03
- **Phase**: Phase 5 (Git layer)

## What shipped

- Right-click or hover an ellipsis on a changed row in the file
  tree to open a context menu with "Discard changes" (modified /
  deleted / untracked) and "Copy path".
- Folder rows get a folder-scoped discard that fans out into every
  non-ignored, non-added descendant change in a single confirm.
- Backend `git_restore_paths` runs `git restore --source=HEAD
--staged --worktree -- <paths>` in one subprocess; untracked
  files and untracked folders route to the OS trash instead.
- Always-on confirm dialog before the destructive action. Open
  tabs for restored paths reload from disk; tabs for trashed
  untracked files close.
- First use of Pierre's `composition.contextMenu` API, with a new
  reusable `ContextMenu.svelte` popover that handles anchoring,
  click-outside / Escape / arrow-key navigation.

## How to test

Prerequisites: `bun install`, `git` on PATH, a git-tracked folder
with at least one commit.

1. `bun run tauri dev`, open a git-tracked folder. Edit a tracked
   file so it's modified (do not save as dirty in moon-ide — edit
   externally or save first).
2. Hover the modified row. Expected: an ellipsis button appears at
   the right edge of the row; the rest of the tree is quiet.
3. Click the ellipsis. Expected: a popover opens with "Discard
   changes" (red) and "Copy path".
4. Hit Escape. Expected: popover closes, focus returns to the row.
5. Right-click the same row. Expected: same menu, anchored at the
   click point. Pick "Discard changes" → confirm dialog fires with
   the path in the message → OK.
6. Expected after OK: the file is restored on disk, the row's
   modified marker disappears, and if the file was open in a tab
   the editor content refreshes to the committed text (no dirty
   indicator).
7. `rm` a tracked file in the terminal (or delete it in moon-ide)
   to produce a `deleted` row. Open the context menu → Discard
   changes → confirm. Expected: the file reappears on disk and the
   ghost row disappears.
8. `touch new.txt` in the terminal. Expected: `untracked` row.
   Right-click → Discard. Confirm message reads "Move the
   untracked file ... to the trash?". Accept → file is in the OS
   trash; row disappears.
9. Multi-select two changed rows (Ctrl/Shift+click), right-click
   on one → pick "Discard changes". Expected: single confirm
   dialog summarises both paths; both are discarded atomically.
   9a. Folder discard: right-click a folder that has modified,
   deleted, _and_ untracked descendants (create this mix by
   editing one file, `rm`-ing another, and `touch`-ing a third
   inside the same folder). Expected: menu shows "Discard N
   changes in this folder". Pick it → confirm dialog reads
   "restore X tracked files to HEAD and move Y files to the
   trash". Accept → modified / deleted rows return to their
   HEAD state, untracked rows move to the OS trash.
   9b. Right-click a folder with only ignored descendants (e.g.
   `target/` on its own). Expected: no discard item in the
   menu — ignored descendants don't count as "changes".
   9c. Right-click a wholly-untracked folder (a freshly created
   directory with files inside that git reports as `?? foo/`).
   Expected: "Discard 1 change in this folder"; accept →
   folder goes to the trash.
10. Stage a new file with `git add file.txt`. Expected: `added`
    marker appears. Open the context menu → no "Discard changes"
    item (by design — unstage vs delete is ambiguous). "Copy
    path" still works.
11. Ignored rows: open the menu on a row matching `.gitignore`
    (e.g. `target/foo.rs`). Expected: no "Discard changes"
    (nothing to undo), just "Copy path".
12. Cancel the confirm dialog at step 5. Expected: file stays
    modified; no reload fires.
13. Close a folder that isn't a git repo, reopen a non-git
    folder. Right-click a file. Expected: just "Copy path" — no
    git-only actions.

## What must keep working

- Pierre row selection, keyboard navigation, search, delete flow
  (Delete / Shift+Delete), double-click-to-open.
- Ignored fade, folder-dot severity tint, deleted ghost rows
  (test plan 0021).
- Fs-watcher live refresh: the restore itself doesn't need to
  call `refreshActiveFolder` explicitly — the watcher picks it up
  — but `discardPaths` still fires a `loadPaths()` to close the
  refresh gap when the watcher is absent / exhausted.

## Known limitations

- `added` (staged-new) files have no menu action, whether
  targeted directly or reached via a folder discard. The correct
  reversal is ambiguous between "unstage" (leaves an untracked
  file) and "delete" (erases the file entirely), and the team
  hasn't asked for either yet. Unstage first via terminal, then
  use the menu on the resulting untracked row.
- Discarding an untracked _directory_ works one path at a time
  (as Pierre's context menu is per-row); the trash call is a
  recursive move for directory rows.
- No undo. "Discard" on a tracked file is irreversible — that's
  why the confirm is always-on. Untracked files go to the OS
  trash, recoverable there.
- Keyboard shortcut intentionally omitted. A palette command was
  considered and dropped — the context menu is the single
  discovery surface, and a hotkey would need a distinct "discard
  changes to active file" entry the team hasn't asked for.

## Related

- Specs: `specs/roadmap.md` (Phase 5), `specs/frontend.md`.
- Prior test plans: `0021-file-tree-full-git-status.md`,
  `0020-file-tree-gitignore-fade.md`.
