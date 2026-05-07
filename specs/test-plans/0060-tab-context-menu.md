# Test plan 0060: Right-click menu on editor tabs

- **Date**: 2026-05-07
- **Phase**: 6.x (editor polish) — small UX add but it touches the open-files surface, the host-direct file flag, and the per-pane close flow, so it gets its own plan.

## What shipped

- Right-clicking a tab in either pane (`EditorTabs.svelte`) now opens a small action menu, mounted via the existing `ContextMenu.svelte` (the same component the file tree uses for its row menus). The menu portals onto `document.body`, so it isn't clipped by the tab strip's horizontal scroll.
- Menu items, in order:
  - **Copy path** — copies the absolute host path. For in-folder buffers it joins `workspace.activeFolderPath` with the buffer's relative `path`. For `isExternal` buffers (opened via `Ctrl+O` on a path outside every bound folder) `path` already is the absolute host path, so we copy it verbatim. Disabled for untitled buffers.
  - **Copy relative path** — copies the workspace-relative `path`. Hidden for `isExternal` buffers (the relative entry would just duplicate "Copy path") and for untitled buffers.
  - **Close** — closes only the right-clicked tab in this pane.
  - **Close others** — closes every other tab in this pane. Disabled when the right-clicked tab is the only one. Each close goes through `workspace.closeFile`, so a dirty buffer still triggers the discard confirmation.
  - **Close all** — closes every tab in this pane. Disabled when the strip is already empty (which can't happen if you right-clicked a tab, but the rule keeps the menu honest).
- Both copy actions surface a `workspace.flash` confirmation (`Copied path` / `Copied relative path`); the clipboard write goes through `navigator.clipboard.writeText`. A clipboard rejection (e.g. window-not-focused policy) flashes a `Could not copy …` instead of failing silently.
- Tab unmount disposes the menu via `$effect` cleanup, so the popover can't outlive the strip when the user collapses a split or closes the folder.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a folder open as the active workspace.

### In-folder file

1. Open any in-folder file (e.g. `src/App.svelte`).
2. Right-click its tab.
3. Expected: menu shows `Copy path`, `Copy relative path`, divider, `Close`, `Close others` (disabled if it's the only tab), `Close all`.
4. Click `Copy path`. Expected: a `Copied path` flash; clipboard now holds the absolute path (e.g. `/home/<user>/code/moon-ide/src/App.svelte`). Paste in a terminal to confirm.
5. Right-click again, click `Copy relative path`. Expected: clipboard holds `src/App.svelte` (no leading slash, no folder prefix).

### External file (Ctrl+O on something outside the active folder)

6. Hit `Ctrl+O` and pick a file outside the current folder (e.g. `/etc/hostname`, or any `~/Documents/...` file).
7. The new tab opens. Right-click it.
8. Expected: menu shows `Copy path`, divider, `Close` / `Close others` / `Close all`. **No** `Copy relative path` row — external buffers don't have a meaningful relative form.
9. `Copy path` flashes `Copied path`; clipboard holds the absolute host path you opened.

### Untitled buffer

10. `Ctrl+N` to create an untitled buffer.
11. Right-click its tab.
12. Expected: menu shows `Copy path` (disabled, greyed out), divider, `Close` / `Close others` / `Close all`. No relative-path row.
13. The disabled `Copy path` row should not respond to click; arrow-key navigation skips it (the menu's internal `enabledItems` list filters disabled entries).

### Close actions

14. Open three or more tabs in the same pane.
15. Right-click the middle tab → `Close others`. Expected: only that tab survives in the pane. The clicked tab stays active.
16. Open a few more tabs, dirty one of them (type a stray char), then right-click any tab → `Close all`.
17. Expected: the dirty buffer triggers the standard "Unsaved changes" confirm; cancelling leaves that tab open while the rest close. Accepting closes everything.
18. With a split open, right-click a tab in the **right** pane → `Close all`. Expected: only the right pane's tabs close. The left pane is untouched.

### Path correctness with multiple bound folders

19. Bind a second folder. Open a file in each, switch the active folder so the second folder's tabs are visible.
20. Right-click a tab → `Copy path`. Expected: the absolute path matches the **second** folder's host path (i.e. `Copy path` reads `workspace.activeFolderPath` at click time, not whatever the first folder was).
21. (Reads from the active folder's slot — see `state.svelte.ts::activeFolderPath`. If you flip back to the first folder before clicking, you get the first folder's prefix; this is by design.)

### Container parity

22. With the active folder running inside a container, right-click an in-folder tab → `Copy path`.
23. Expected: the copied path is the **host** path (the bind-mount source, e.g. `/home/<user>/code/<folder>/<file>`), not the in-container path. The clipboard is most useful for paste-into-host-terminal, so the host path wins. (If you ever need the container path you can derive it from the bind-mount config in `compose.yaml`.)

### Menu dismissal & a11y

24. Open the menu, press `Escape`. Menu closes without action.
25. Open the menu, click anywhere outside it. Menu closes.
26. Open the menu, use `ArrowDown`/`ArrowUp` to move focus. Disabled rows (e.g. `Copy path` on an untitled buffer) are skipped. `Enter` activates the focused row.
27. Open the menu near the bottom of the screen — it should flip to anchor above the cursor instead of clipping (this is `ContextMenu.svelte`'s built-in viewport flip).

### Cleanup

28. With the menu open, close the folder via the folder bar. Expected: the menu disappears (the strip's `$effect` cleanup tears down the portaled host).
29. With the menu open, collapse the right split (`Ctrl+\`). Expected: if the menu was attached to a tab in the disappearing pane, it tears down cleanly — no orphaned host on `document.body`.

## What must keep working

- Left-click on a tab still activates it; middle-click still drags; the existing close button (`×`) still closes the tab without surfacing the menu.
- Drag-and-drop between panes (the `ondragstart` / `ondragover` / `ondrop` set) is unaffected. Right-click does not initiate a drag.
- The close-button `×` calls `workspace.closeFile` directly; it does **not** route through the new `Close` menu entry, so its dirty-prompt behaviour is unchanged.
- All other consumers of `ContextMenu.svelte` (the file tree's row menus). The component itself wasn't modified.
- `Ctrl+O` flow and the `isExternal` flag: the menu reads `file.isExternal` but doesn't mutate any open-file shape.

## Known limitations

- Bulk-close actions (`Close others`, `Close all`) walk the tabs sequentially and call `closeFile` on each, so dirty buffers prompt one-by-one. We can batch-confirm later if it becomes annoying; for now there's no concrete report.
- `Copy path` for in-folder files joins with `/` directly — fine on Linux/macOS, but Windows hosts (which we don't ship to today) would want a backslash. Revisit if/when the project supports Windows.
- The clipboard write uses `navigator.clipboard` directly (same pattern as the file-tree menu's `Copy path`). Tauri also has `@tauri-apps/plugin-clipboard-manager`; switching to it would only matter if `navigator.clipboard` ever stopped working in the webview.

## Related

- Specs: [frontend.md](../frontend.md) — open-files / per-pane tab strip.
- Prior test plans: [0051-open-host-file.md](0051-open-host-file.md) (the `isExternal` flag this menu reads), [0055-open-session-trace.md](0055-open-session-trace.md) (another consumer of the host-direct file mechanism).
