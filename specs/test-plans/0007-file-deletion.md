# Test plan 0007: File deletion (trash + permanent)

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- Two new `WorkspaceHost` operations: `trash_path` (OS trash via
  the `trash` crate; FreeDesktop / Finder / Recycle Bin) and
  `delete_path` (permanent). Both refuse the workspace root,
  block `..` traversal, and clear the editorconfig cache.
- File-tree keybindings: `Delete` / `Backspace` trashes;
  `Shift+Delete` / `Shift+Backspace` deletes permanently.
  Targeting follows the keyboard cursor, so arrow-after-click
  acts where the user is looking. Suppressed while focus is in
  Pierre's search/rename input.
- Multi-select does one pass: descendants of a selected
  directory are dropped before IPC (directory removal subsumes
  them), calls run in parallel via `Promise.allSettled` so one
  bad path doesn't abort the rest, and partial failures
  surface as a single toast.
- Removal cleans up every in-memory buffer the paths or their
  descendants touch (open files, both panes' tab arrays,
  preview-mode, editorconfig). Untitled tabs in a multi-select
  close without IPC. No per-tab dirty-discard prompt — the
  confirm already covers intent.

## How to test

Prerequisites: `bun install`, host deps installed per `README.md`.

1. `bun run dev` and open a workspace with at least:
   - A file you can afford to lose (e.g. a scratch `notes.txt`).
   - A folder with multiple files.
   - A file that's open in **both** panes (split view).

2. **Trash a file**.
   1. Click `notes.txt` in the tree to focus the row.
   2. Press `Delete`.
   3. Expected: confirm dialog titled "Move to trash" with body
      "Move notes.txt to the trash?" and buttons "Move to trash" / "Cancel".
   4. Confirm. Expected: row disappears from the tree, any open tab for it
      closes without a dirty-discard prompt, file is in your OS trash
      (Linux: `~/.local/share/Trash/files/`).

3. **Cancel a trash**.
   1. Repeat with another file but click "Cancel".
   2. Expected: nothing changes, no IPC call, file stays.

4. **Hard delete a file**.
   1. Focus another scratch file.
   2. Press `Shift+Delete`.
   3. Expected: confirm dialog titled "Permanently delete" with body
      "Permanently delete <path>? This cannot be undone (recover via git if
      it was tracked)." and buttons "Delete" / "Cancel".
   4. Confirm. Expected: file is gone (not in trash), tree refreshes, any
      open tabs close.

5. **Delete a directory recursively**.
   1. Make a temp folder with two files, open one of them so it has a tab.
   2. Focus the folder row, press `Delete` (trash) or `Shift+Delete`.
   3. Expected: prompt mentions "the folder X (and everything inside it)"
      (trash) or "the folder X and everything inside it. This cannot be
      undone…" (permanent). Confirm. The folder and its open tab both go
      away. The other (formerly nested) file's editorconfig entry is no
      longer in `editorConfigs` (verify by re-creating the same path and
      seeing it re-resolves from disk).

6. **Backspace works on macOS-style hardware**.
   1. Focus a row, press `Backspace`. Expected: trash confirm. With Shift:
      permanent delete confirm.

7. **Search/rename inputs don't trigger delete**.
   1. Click into Pierre's filter input at the top of the tree.
   2. Type something then hit `Backspace`. Expected: backspace deletes a
      character in the input — no confirm dialog appears.

8. **Workspace root is protected**.
   1. There's no UI way to focus the root row, but if it ever happens, the
      backend must refuse. (Verified by `host.rs` tests — covered by
      `delete_refuses_workspace_root`.)

9. **Both panes recover from a delete**.
   1. Open the same file in both panes via "Open to side".
   2. Trash it. Expected: tab closes in both panes; if it was active, each
      pane falls back to the previous tab (or the welcome view).

10. **Multi-select trash**.
    1. Click a file. Ctrl+click two more files. Expected: three rows
       highlighted; the keyboard cursor is on the last one clicked.
    2. Press `Delete`. Expected: confirm reads "Move 3 items to the
       trash?" with the same buttons as the single-file case. Confirm.
    3. All three rows disappear, all three open tabs (if any) close,
       three entries land in the OS trash.

11. **Multi-select with directory subsumption**.
    1. Multi-select a folder `src/` plus a file `src/foo.ts` inside it.
    2. Press `Shift+Delete`. Expected: confirm reads "Permanently delete
       2 items? …" _but_ only one IPC call is issued (against `src/`);
       the directory delete subsumes the file. Tree refreshes and both
       are gone; no "no such file" toast appears.

12. **Arrow off the selection, then Delete**.
    1. Click `a.txt` to select + focus it.
    2. Arrow down to `b.txt`. Pierre updates focus but not selection —
       `a.txt` is still highlighted, `b.txt` is the keyboard cursor.
    3. Press `Delete`. Expected: confirm targets `b.txt`, not `a.txt`.
       The "Delete acts where the cursor is" rule beats the stale
       click-selection.

13. **Partial failure surfaces a single toast**.
    1. (Manual setup) Make a file `locked` read-only-by-parent (e.g.
       `chmod -w` the parent dir on Linux), select it alongside a
       writable file, press `Shift+Delete`, confirm.
    2. Expected: writable file is gone; toast reads
       "Delete failed for 1 of 2: …". The successful removal is still
       reflected in the tree and in any open tabs.

## What must keep working

- Untitled tabs: `Delete` on a `untitled:N` path (only reachable defensively)
  closes the tab and never hits IPC.
- Dirty discard prompt still fires when _closing_ a dirty file via the tab
  close button or `Ctrl+W` — only `removePaths` skips it.
- Editorconfig resolution after a delete picks up new files at the same path
  (cache was cleared).
- Pierre's search/filter input still accepts Backspace and Delete inside it.

## Known limitations

- No undo for permanent deletes; the team's recovery story is git for tracked
  files, no recovery for untracked files. Trash is recoverable via the OS UI.
- Linux: relies on each distro implementing FreeDesktop Trash v1.0. The
  `trash` crate documents this assumption; non-DE-shipping minimal systems
  (e.g. some headless containers) may not have a trash dir, in which case
  the crate errors and we surface "Move to trash failed: …" via the toast.
- No "Don't ask me again" toggle on the trash confirm. Adding one is cheap
  but speculative; gate on team feedback.
- Multi-select dialog body is "N items" — no per-path enumeration. Native
  dialogs don't render lists cleanly; the tree highlight already shows the
  targets. Revisit when we have a real problems / dialog surface.
- Partial-failure toast surfaces only the _first_ error reason. Acceptable
  while there's no problems panel; revisit in Phase 8.

## Related

- Specs: `specs/architecture.md` (workspace host), `specs/roadmap.md`.
- Prior test plans: `0001-initial-bootstrap.md` (workspace host, IPC plumbing).
