# Test plan 0007: File deletion (trash + permanent)

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- `WorkspaceHost::trash_path` (move to OS trash) and `WorkspaceHost::delete_path`
  (permanent removal). `LocalHost` implements both:
  - `trash_path` calls the cross-platform [`trash`](https://crates.io/crates/trash)
    crate via `tokio::task::spawn_blocking` (it's a sync API and some backends
    — XDG trash on a network mount, Finder calls — can stall). On Linux this
    uses the FreeDesktop Trash spec v1.0; macOS uses Finder Trash; Windows uses
    the Recycle Bin.
  - `delete_path` uses `tokio::fs::remove_file` / `remove_dir_all`.
  - Both refuse to act on the workspace root and rely on `resolve()` to block
    `..` traversal.
  - Both clear the editorconfig cache after a successful op.
- Tauri commands `fs_trash` and `fs_delete` and matching IPC wrappers
  (`ipc.fs.trash`, `ipc.fs.delete`).
- `WorkspaceState.trashPaths(paths)` and `WorkspaceState.deletePaths(paths)`
  share a private `removePaths(paths, mode)` that:
  - Drops descendants of selected ancestors (selecting `src/` plus
    `src/foo.ts` collapses to one IPC call against `src/`) via the
    file-local `dropDescendantPaths` helper — the directory removal subsumes
    the children, and removing the children first risks "no such file"
    errors when the parent removal cleans them up.
  - Confirms via the native dialog with mode-specific wording. Single-target
    selections keep the precise filename / "the folder X" wording; multi
    falls back to "Move N items to the trash?" / "Permanently delete N
    items? This cannot be undone (recover via git if any of them were
    tracked)." (no enumeration — native dialogs don't render long lists
    cleanly and the tree highlight already shows the targets).
  - Issues IPC calls in parallel via `Promise.allSettled` so one bad path
    (locked, perms) doesn't abort the rest. Failures are summarised in a
    single toast (count + first error) — the problems-panel home for
    multi-line errors arrives in Phase 8.
  - Drops every open buffer the operation invalidates (the paths themselves,
    plus descendants for directories) from `openFiles`, both panes' tab
    arrays, `previewModes`, and `editorConfigs`. Active paths fall back to
    the last surviving tab on the same side.
  - Refreshes the file tree via `loadPaths()` and persists the session.
  - Skips the dirty-discard prompt — the user just confirmed they want the
    paths gone, asking again per-tab would be noise.
  - Closes any synthetic `untitled:N` paths in the input as plain tab
    closes (no IPC call) so multi-selecting an untitled buffer alongside
    real files still does the obvious thing.
- `FileTree.svelte` keybindings:
  - `Delete` / `Backspace` → trash.
  - `Shift+Delete` / `Shift+Backspace` → permanent delete.
  - Targeting via `collectRemovalTargets`: when the keyboard cursor sits
    on a selected row the entire selection is the target (multi-delete);
    otherwise just the focused row is, so arrow keys after a click act
    where the user expects rather than on the originally-clicked file.
  - All variants are suppressed if focus is inside an `<input>` or
    `<textarea>` in the tree's shadow DOM (Pierre's search box, future
    rename input), so typing inside those fields can never trigger a
    delete.

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
