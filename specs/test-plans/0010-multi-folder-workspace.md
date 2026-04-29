# Test plan 0010: Multi-folder workspace UX

- **Date**: 2026-04-29
- **Phase**: Phase 2.5 — Multi-folder workspace UX

## What shipped

- A workspace stops being a single folder. The singleton
  `"default"` workspace now holds zero or more bound folders,
  with at most one active at a time.
- Sidebar renders a stacked list of folder bars (one row per
  bound folder) plus an inline `+ Add folder` row at the bottom.
  The active bar shows `▾` and renders that folder's file tree
  immediately underneath; inactive bars show `▸` (header-only).
- Per-folder UI state (open tabs, active tab, split layout,
  focused side, untitled-buffer counter) is preserved across
  folder switches via `WorkspaceState.folderStates`. Editor pane
  - tab strip swap when the active folder changes.
- `×` on each bar (revealed on hover, kept visible on focus)
  drops the folder with a confirm dialog. Dirty buffers in the
  removed folder are discarded after a separate warn line in the
  confirm.
- Re-picking a folder already in the workspace flips it to
  active and flashes a `Folder is already in the workspace.`
  toast — no duplicate row.
- Removing the last folder collapses to the welcome screen.
- Persisted session is per-folder: `WorkspaceSession` carries a
  list of `FolderSession` entries plus an `active_folder_path`.
  Both folder list + active pointer survive restart.
- `Ctrl+P` (file search), `Ctrl+Shift+F` (content search), and
  the file tree all scope to the active folder. Cross-folder
  search stays a Phase 7 concern.

## How to test

Prerequisites:

- `bun install`
- Tauri dev deps per `README.md`.
- Have at least three real folders to point at — e.g.
  `~/code/moon-ide`, `~/code/moon-landing` (any two
  non-trivial repos), plus a throwaway `mkdir /tmp/scratch`.

### A. First-run flow

1. Wipe persisted state so we start fresh:
   `rm -f ~/.config/moon-ide/state.json`
   (path is `app_config_dir()` — adjust for your OS if running
   elsewhere).
2. `bun run tauri dev`. Expected: welcome screen. Sidebar shows
   no folder bars — only the bottom `+ Open folder` row.
3. Click `+ Open folder`, pick `~/code/moon-ide`. Expected:
   one folder bar appears with `▾ moon-ide`, file tree renders
   underneath, welcome screen replaced by the editor pane (no
   tabs yet).
4. Open `README.md` from the tree. Expected: opens in the editor.

### B. Add a second folder, switch between them

5. Click `+ Add folder`, pick `~/code/moon-landing` (or any
   second folder). Expected: a second bar appears below the
   first; the new folder becomes active (`▾`), the previous
   bar collapses to `▸ moon-ide`. Tree + tabs swap.
6. Open `package.json` (or any file) in the second folder.
   Expected: opens in the editor; left tab strip shows only
   `package.json`, not `README.md`.
7. Click the `▸ moon-ide` bar to switch back. Expected: tabs
   swap to show `README.md` (the tab from step 4 is preserved).
   Tree swaps to moon-ide's contents.
8. Click `▾`/`▸` chevrons rapidly to swap a few times. Expected:
   no flicker, no orphaned buffers, tab state per folder always
   matches what was last open in that folder.

### C. Persistence across restart

9. With both folders bound and `moon-ide` as the active folder
   showing `README.md`, kill the dev server (`Ctrl+C` in the
   terminal that's running `tauri dev`) and relaunch.
10. Expected: both folder bars are present, the same one is
    active, and the same tab is open. Re-saving `state.json`
    after a quick edit and re-launching: still consistent.

### D. Remove a folder

11. Hover over the inactive `▸ moon-landing` bar. Expected:
    `×` button fades in on the right edge.
12. Click `×`. Expected: confirm dialog reading
    `Remove moon-landing from the workspace?`. Click cancel —
    nothing changes.
13. Open an untitled tab in `moon-landing` (`Ctrl+L`-aware:
    type `Ctrl+N`, type some text — buffer is dirty), switch to
    `moon-ide`, then click `×` on the `moon-landing` bar.
    Expected: confirm warns about the unsaved buffer count.
    Confirming drops the bar, the dirty untitled is discarded
    (silently), and the active folder stays on `moon-ide`.
14. Click `×` on the active `▾ moon-ide` bar. Confirm.
    Expected: bar disappears, welcome screen returns,
    `+ Open folder` is the only row left in the sidebar.

### E. Duplicate-add detection

15. From welcome, open `~/code/moon-ide`. Then click
    `+ Add folder` and pick the _same path_ again.
    Expected: bar count stays at 1, toast reads
    `Folder is already in the workspace.`, the existing bar
    flips to active.
16. Add a second folder. Click `+ Add folder` and pick the
    second one again (the inactive bar's folder).
    Expected: same toast; that folder becomes active without a
    second bar appearing.

### F. Edge cases

17. With 3+ folders bound, remove the active one. Expected:
    active hands off to the _previous_ in insertion order
    (the bar immediately above the removed one). Bar focus +
    tree re-mount accordingly.
18. With 3+ folders bound, remove the _first_ (top) folder
    while it is active. Expected: active hands off to the new
    first folder (the one that was second).
19. Switch the active folder while a save is in flight in the
    previous active. Expected: the save completes against the
    previous folder's host (the IPC was already routed when we
    issued it), but no UI confusion — tabs swap correctly when
    the switch completes.
20. Close the IDE with N folders bound, delete one of those
    folders from disk (`rm -rf /tmp/scratch`), relaunch.
    Expected: the deleted folder is dropped silently from the
    folder list during launch hydrate; remaining folders bind
    normally; saved session is rewritten without it. Console
    log includes a `failed to restore folder` warning.

## What must keep working

Regression checks. If any of these break, the commit needs a
follow-up.

- Single-folder UX (one folder bound) is visually and
  functionally identical to pre-2.5: sidebar shows one bar, tree
  underneath, tabs work, split works, save / save-as work.
- All existing keyboard shortcuts still target the active
  folder: `Ctrl+P` (file open), `Ctrl+Shift+F` (search),
  `Ctrl+W` (close tab), `Ctrl+S` (save), `Ctrl+N` (new untitled),
  `Ctrl+\` (split), `Ctrl+0` (sidebar focus — now lands on the
  active folder's tree, falling back to the `+ Add folder`
  button when no folder is active), `F6` cycle.
- Per-pane tab state (left vs. right) is independent within a
  folder — same as before, just now also independent across
  folders.
- The container status pip in the status bar still tracks the
  active folder's compose project (Phase 2.0 bridge
  implementation). Switching folders refreshes the pip; the
  Phase 2 redesign that decouples this from the active folder
  ships in a later commit.
- `app_state.json` corruption falls back to defaults — same
  contract as Phase 0–2; covered by
  `crates/moon-core/src/app_state.rs` unit tests.

## Known limitations

Things we deliberately did not do — see
[`specs/roadmaps/phase-02.5-multi-folder.md`](../roadmaps/phase-02.5-multi-folder.md)
§ "What deliberately doesn't ship" for the full list.

- Tree state (expansion, scroll position) is not preserved
  across folder switches. The tree is re-mounted from scratch
  every time the active bar changes. Phase 7 follow-up if it
  becomes annoying.
- No drag-to-reorder folder bars. Insertion order only.
- No folder rename. Path basename is the label.
- Compose status indicators on the bars: empty `.indicator`
  slot is reserved on each row; the Phase 2 container redesign
  will fill it.
- Cross-folder file or content search. Both palettes scope to
  the active folder.
- Multiple workspaces. There is one workspace (`"default"`)
  with N folders.

## Related

- Specs:
  [`roadmap.md`](../roadmap.md) (Phase 2.5 entry),
  [`roadmaps/phase-02.5-multi-folder.md`](../roadmaps/phase-02.5-multi-folder.md),
  [`containers.md` § Multi-folder workspace](../containers.md#multi-folder-workspace-the-command-centre-ux).
- Prior test plans:
  [0003 — Per-pane tabs](0003-per-pane-tabs.md) (single-folder
  per-pane invariants this plan should preserve),
  [0006 — Untitled tabs](0006-untitled-tabs.md) (untitled
  buffer behaviour, now per-folder).
