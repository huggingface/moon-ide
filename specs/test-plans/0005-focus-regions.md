# Test plan 0005: keyboard focus between regions + tree preview-open

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- Keyboard focus cycles through four named regions — sidebar,
  left pane, right pane, status — via `F6` / `Shift+F6`. Order
  is computed from the live layout, so regions that aren't
  mounted (split closed, no workspace) are skipped.
- `Ctrl+0` jumps straight to a file-tree row (not the header
  button or Pierre's search input); `Esc` from anywhere in the
  sidebar yanks focus back to the active editor, unless the
  user is typing in an `<input>`.
- Tree preview-open: single-click and arrow-key selection open
  the file without stealing focus from the tree, so arrow-
  browsing previews siblings. Enter / double-click hand focus
  to the editor.
- Switching the active file scrolls the tree to the matching
  row, even when Pierre has virtualized it out.
- Dev-server hygiene: a no-store `Cache-Control` Vite plugin
  stops WebKitGTK from persisting stale dev chunks across
  launches (fixes the `failed to load virtual css module`
  warning storm).

## How to test

Prerequisites: `bun install`, host deps installed per `README.md`,
`bun run dev` running, the moon-ide repo open as the workspace.

1. Open a few files so both the sidebar and the editor have
   meaningful state. Click into the editor (caret blinks in
   CodeMirror).
2. Press `F6`. Expected: focus advances one region in
   left-to-right layout order, landing on the status bar's theme
   button (sidebar → editor → status; from the editor, the next
   region is the status bar). The button gets a focus ring;
   `Enter`/`Space` toggles the theme.
3. Press `F6` again. Expected: cycle wraps to the file tree;
   `↓`/`↑` now navigates the tree.
4. Press `F6` once more. Expected: focus returns to the editor;
   `↓`/`↑` moves the caret again.
5. Press `Shift+F6` from the editor. Expected: focus moves
   _backwards_ — straight to the file tree without going through
   the status bar. This is the fast path for "jump to tree".
6. Press `Shift+F6` again. Expected: regions in reverse order
   (sidebar → status → editor → sidebar …).
7. While focused inside the file tree, press `Esc`. Expected:
   focus returns to the active editor without changing the
   selection in the tree.
8. Click into Pierre's tree search input (the textfield at the
   top of the tree). Press `Esc`. Expected: Pierre clears or
   collapses the search as it always did — our handler stays out
   of the way for inputs.
9. Press `Ctrl+0` from the editor. Expected: focus lands on a
   row inside the file list (a `[role="treeitem"]` gets the
   focus ring); pressing `↓`/`↑` immediately moves between
   rows. Specifically, focus does **not** land on the "Open
   folder" header button or Pierre's search input — those
   would force a Tab dance the user just complained about.
   9a. **AZERTY check.** On a French keyboard, the natural form
   of the shortcut is `Ctrl+Shift+0` (Shift is required to
   type a literal `0` on the digit row); that should fire and
   land focus on a tree row. On QWERTY, `Ctrl+0` fires the
   same way. Browsers usually bind `Ctrl+0` / `Ctrl+Shift+0`
   to "reset zoom", but Tauri's webview doesn't.
10. Open the command palette (`Ctrl+Shift+P`), type "focus".
    Expected: a single result — `Focus File Tree` — with
    `Ctrl+0` shown on the right. Activating it focuses the
    file tree the same way Ctrl+0 does (lands on a tree row,
    not the header). The cycle commands are deliberately
    absent (see "What shipped" for the rationale).
11. Split the editor (`Ctrl+\`). Click into the left pane,
    press `F6`. Expected: focus moves to the right pane (the
    full order is sidebar → editor-left → editor-right →
    status → sidebar). Verify both directions.
12. Close the split (`Ctrl+\` again). Expected: F6 cycle drops
    `editor-right` from the order; pressing it from the
    remaining editor goes straight to status.
13. With **no workspace** open (welcome screen), press `F6`.
    Expected: the cycle is sidebar ↔ status (no editor regions
    exist in the DOM); F6 never gets stuck.
14. Open the command palette and press `F6`. Expected: focus
    leaves the palette and enters the cycle at `sidebar` (palette
    is off-region; F6 enters at the start). `Shift+F6` from an
    off-region state enters at `status`.

### Tree → editor focus

15. Click a file in the tree. Expected: the file opens in the
    editor **and** the caret appears in CodeMirror — typing
    immediately edits the file. The tree row is still
    selected (highlighted) but no longer holds DOM focus.
    Specifically, the first keystroke after the click does
    **not** seed Pierre's tree search (the old preview-open
    behaviour did, and that's the bug this change fixes).
16. Re-focus the tree (`Ctrl+0` or `F6`). Press `↓`/`↑`.
    Expected: the tree's focused row moves; Pierre only
    fires `onSelectionChange` on click, so arrow keys don't
    open files in the editor — they just move Pierre's row
    cursor. Press `Enter` on a focused file row: the file
    opens and the caret blinks in CodeMirror.
17. Re-focus the tree, double-click a file. Expected: the
    file opens AND focus moves to the editor (single click
    already does both — double-click is just the
    follow-through gesture and shouldn't break).
18. Single-click a directory. Expected: Pierre toggles its
    expansion as before; nothing opens in the editor; focus
    stays on the directory row. Pressing `Enter` on a
    focused directory row is a no-op (Pierre's `→`/`←` still
    expand and collapse as before).
19. Open the command palette (`Ctrl+P`), pick a file. Expected:
    the file opens AND the caret lands in CodeMirror ready
    to type. Verify on the **very first file open after a
    fresh launch** too — the `let view` reactivity fix in
    `Editor.svelte` makes the focus retry once `onMount`
    finishes building the view.
20. Click an already-open tab in the tab strip. Expected: the
    tab becomes active and focus moves to the editor.

### Tab → tree scroll

21. Open enough files that the file tree has visible scroll
    (resize the window if needed, or open a deep folder).
    Manually scroll the tree so the active file's row is
    nowhere on screen.
22. Click a different tab in the tab strip. Expected: the file
    tree scrolls so the newly active file's row is visible and
    selected, even if it was virtualized out of the rendered
    window. Focus stays in the editor (the slow-path focus
    park is restored before the call returns).
23. Repeat with palette quick-open: open the palette, type the
    name of a file far away in the tree, accept. Expected: the
    file opens, the editor has focus, and the tree has
    scrolled to the row.

### Dev-server cache (warning-free launches)

24. Stop the dev server (`Ctrl+C` in the terminal running
    `bun run dev`). One-time hygiene: delete the WebKitGTK
    cache for moon-ide so any stale entries from before this
    change are flushed —
    `rm -rf ~/.local/share/moon-ide/WebKitCache`.
25. Restart `bun run dev`. Expected: the terminal logs no
    `[vite-plugin-svelte:load] failed to load virtual css
module` warnings during startup, and none on subsequent
    cold relaunches. (Some warnings can still appear during
    HMR if you edit a Svelte component while the page is in a
    partial state — those are dev-only and harmless.)

## What must keep working

- All Phase 1 / Phase 1.5 invariants from test plans 0001-0004.
- Clicking a tab still focuses its pane (existing behaviour;
  unrelated code path, but verify F6 cycle and click-to-focus
  agree on which pane is "current").
- `Ctrl+R` reload still prompts for dirty buffers (no
  interaction with the new keys).
- The theme toggle still works from the status bar — the
  status focus ticker just adds a focus path; clicking is
  unchanged.

## Known limitations

- **No focus ring on Pierre rows.** Pierre Trees handles its own
  selection styling; our F6 jump focuses the first focusable
  element inside but doesn't paint an extra outline. Keyboard
  navigation works regardless. We'll reassess once we replace
  or restyle the tree.
- **`Ctrl+0` clashes with browser "reset zoom" if the webview
  ever respects it.** Tauri's webview doesn't, so we get the
  shortcut for free; if this changes we'll move to `Ctrl+B` or
  similar.
- **No region for the command palette itself.** The palette is
  modal and has its own focus trap; F6 leaves it alone (and
  treats it as off-region for cycle entry).
- **Welcome screen's "Open folder" button isn't a focus target.**
  When no workspace is open, F6 only cycles sidebar ↔ status.
  The Welcome button is reachable via Tab from the sidebar.
  Adding a `data-region="welcome"` is straightforward if it
  comes up.
- **Enter on a focused directory does nothing.** Pierre's
  `→`/`←` still expand and collapse, so the workflow is
  unchanged; we just don't bind Enter for directories yet.
  Cheap follow-up if anyone misses it.

## Related

- Specs: `specs/roadmap.md` (Phase 1.5 — accessibility / focus).
- Prior test plans: `0003-per-pane-tabs.md` (introduced
  per-pane focus state that this plan builds on).
- ADRs: none directly.
