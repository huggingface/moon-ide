# Test plan 0005: keyboard focus between regions + tree preview-open

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- New `src/lib/focus.ts`: pure module that owns the region cycle.
  Exports `regionOrder()`, `currentRegion()`, `focusRegion(id)`, and
  `cycleFocus(forward)`. Cycle order is computed from current
  layout (sidebar always; left pane only when a workspace is open;
  right pane only when split; status always) so F6 never lands on
  a region that doesn't exist.
- Region marker convention: each top-level region root carries a
  `data-region` attribute (`sidebar`, `editor-left`, `editor-right`,
  `status`). `currentRegion()` reads it from `document.activeElement`'s
  closest ancestor.
- `WorkspaceState` gains `sidebarFocusTick` and `statusFocusTick`
  alongside the existing `focusTick`. `requestSidebarFocus()` /
  `requestStatusFocus()` bump them; the matching component pulls
  focus in via a `$effect`. Same pattern as the editor's existing
  focus ticker, kept symmetrical for the next region we add.
- `Sidebar.svelte`:
  - Roots a `data-region="sidebar"` wrapper with `tabindex="-1"`.
  - Watches `workspace.sidebarFocusTick` and focuses **a tree
    row directly** (the active `[role="treeitem"][tabindex="0"]`
    when one exists, otherwise the first row). Earlier versions
    landed on the "Open folder" header button or Pierre's
    search input, forcing a Tab dance to reach the file list;
    we now skip straight to the rows. The header button is the
    fallback only when there are no rows at all (no workspace).
  - `Esc` while focused inside the sidebar yanks focus back to
    the active editor — but only when the user isn't typing in
    an `<input>`/`<textarea>`, so Pierre's search input keeps its
    native Esc-to-clear behaviour.
- `StatusBar.svelte`: `data-region="status"` + a tick-driven
  `$effect` that focuses the theme toggle (the only interactive
  control on the bar today).
- `EditorPane.svelte`: `data-region="editor-left"` /
  `data-region="editor-right"` so the cycle can locate the focused
  pane. The existing `focusSide(side)` + editor focus ticker is
  reused; `focusRegion('editor-left')` calls both.
- `App.svelte` keybindings:
  - `F6` → `cycleFocus(true)`.
  - `Shift+F6` → `cycleFocus(false)`.
  - `Ctrl+0` → `workspace.requestSidebarFocus()`. We don't
    filter by Shift state for this one: French AZERTY needs
    Shift to type a literal `0`, so AZERTY users hit
    `Ctrl+Shift+0` and QWERTY users hit `Ctrl+0` — both
    produce `event.key === '0'`, so the same handler fires on
    either layout without caring about Shift.
- Palette command for discoverability: `Focus File Tree`
  (Ctrl+0). The cycle commands (F6 / Shift+F6) are
  deliberately **not** in the palette — F6 is relative to the
  current region, and the palette is "off-region", so a palette
  entry would always re-enter the cycle at the same edge
  instead of advancing. The keys themselves work fine; the
  palette button would have been a misleading ghost shortcut.
- **Tree preview-open without focus steal.** Single-click and
  arrow-key selection in the tree now open the file in the
  editor _without_ moving DOM focus out of the tree, so the
  user can keep arrow-browsing through siblings. `openFile()`
  / `setActive()` learned an `{ focus?: boolean }` option;
  callsites that drive a deliberate jump (tab clicks, palette
  pick, session restore) keep the default `focus: true`, the
  tree opts out with `focus: false`. **Enter** on the focused
  row, and **double-click** anywhere in the tree, hand focus
  to the editor (Enter also opens the row if it wasn't already
  open — useful after pure arrow-key navigation, since Pierre
  only updates selection on click).
- **Tab → tree scroll.** Switching the active file (clicking a
  tab, palette pick, splitting) now scrolls the tree to the
  matching row even when Pierre has virtualized it out of the
  rendered window. Implementation: a fast path that calls
  `scrollIntoView` on the row when it's already mounted, and a
  slow path that briefly parks DOM focus on Pierre's scroll
  container so the controller's layout effect runs
  `scrollFocusedRowIntoView` (Pierre gates that effect on
  "focus is inside the tree" — without the focus park, a
  programmatic `focusNearestPath` on its own does nothing).
  Original focus is restored once the scroll commits.
- **Vite dev-server cache headers.** A small dev-only plugin
  rewrites `Cache-Control` to `no-store` on every response so
  WebKitGTK doesn't persist dev artifacts. Without it, a
  config change that alters which modules a Svelte component
  imports leaves stale JS in
  `~/.local/share/dev.moon-ide.desktop/WebKitCache` that
  resurrects requests for modules Vite no longer produces,
  surfacing as `failed to load virtual css module` warnings on
  every launch. `Cache-Control: no-store` keeps every dev
  fetch fresh, no manual cache wipes needed.

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

### Tree preview-open

15. Click a file in the tree. Expected: the file opens in the
    editor, but the focused row indicator stays in the tree
    (caret does **not** appear in CodeMirror; the tree's row
    keeps the focus ring). The matching tab is shown as
    active.
16. Press `↓`/`↑`. Expected: the tree's focused row moves;
    when it lands on a different file the editor preview
    swaps to that file (selection and tab follow). Focus
    remains in the tree the entire time. Skipping over
    directories is fine — they're not opened in the editor,
    just focused in the tree.
17. Press `Enter` on a focused file row. Expected: focus jumps
    to the editor and the caret blinks in CodeMirror. The
    file is the one that was focused in the tree (handles the
    case where Pierre's "focused" row is ahead of the
    "selected" row after pure arrow-key navigation).
18. Re-focus the tree (`Ctrl+0` or `F6`). Double-click another
    file. Expected: the file opens AND focus moves to the
    editor in one gesture.
19. Single-click a directory. Expected: Pierre toggles its
    expansion as before; nothing opens in the editor;
    focus stays in the tree. Pressing `Enter` on a focused
    directory row is a no-op (Pierre's `→`/`←` still expand
    and collapse as before).
20. Open the command palette (`Ctrl+P`), pick a file. Expected:
    same as before — the file opens AND focus jumps into the
    editor (the palette path keeps the default `focus: true`).
21. Click an already-open tab in the tab strip. Expected: the
    tab becomes active and focus moves to the editor (tab
    clicks keep the default `focus: true`).

### Tab → tree scroll

22. Open enough files that the file tree has visible scroll
    (resize the window if needed, or open a deep folder).
    Manually scroll the tree so the active file's row is
    nowhere on screen.
23. Click a different tab in the tab strip. Expected: the file
    tree scrolls so the newly active file's row is visible and
    selected, even if it was virtualized out of the rendered
    window. Focus stays in the editor (the slow-path focus
    park is restored before the call returns).
24. Repeat with palette quick-open: open the palette, type the
    name of a file far away in the tree, accept. Expected: the
    file opens, the editor has focus, and the tree has
    scrolled to the row.

### Dev-server cache (warning-free launches)

25. Stop the dev server (`Ctrl+C` in the terminal running
    `bun run dev`). One-time hygiene: delete the WebKitGTK
    cache for moon-ide so any stale entries from before this
    change are flushed —
    `rm -rf ~/.local/share/dev.moon-ide.desktop/WebKitCache`.
26. Restart `bun run dev`. Expected: the terminal logs no
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
