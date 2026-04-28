# Phase 1.5 — Editor polish

A small, scoped phase that closes Phase 1's loose ends and adds
the bare minimum needed for moon-ide to feel right when opening
_itself_. Surfaced after Phase 1 closed because of the bootstrap
concern in [ADR 0005](../decisions/0005-bootstrap.md): without
this, contributors editing moon-ide-in-moon-ide diverge from
house style on every keystroke until the pre-commit hook fires.

Architectural spec this phase implements:
[`editorconfig.md`](../editorconfig.md) — `.editorconfig`
end-to-end model.

## Acceptance

### `.editorconfig` honored end-to-end

- `indent_style`, `indent_size` / `tab_width` drive CodeMirror's
  tab size and the Tab keymap, replacing the hardcoded constants
  currently in `Editor.svelte`.
- `end_of_line`, `insert_final_newline`,
  `trim_trailing_whitespace` are applied as pre-save hooks.
- `charset` is utf-8 only for v1; anything else logs a warning.
- Reload happens on `.editorconfig` save: the host clears its
  resolution cache when moon-ide writes a `.editorconfig`;
  external edits (git pull, another editor) wait for restart
  until Phase 5 ships the fs watcher.
- Precedence: `.editorconfig` over moon-ide defaults. There is
  no project-level overlay file; per
  [ADR 0006](../decisions/0006-no-settings-file.md) `settings.json`
  is gone.
- No per-language `tab_size` default — let `.editorconfig` and
  the file's language decide.

### Pre-save hook pipeline

Generic — a list of `BeforeSaveTransform`s — so Phase 8 can
drop format-on-save into the same pipeline without
re-architecting it.

### Markdown rendered preview

Opening a `.md` / `.markdown` file shows a per-tab toggle
between source ("Code") and rendered ("Preview"). Default mode
is "Preview" (the README is what we want to see when clicking
it; opening for editing is the deliberate gesture). The
Cursor-style two-state toggle lives on the tab strip, scoped to
the active tab. Renderer runs in-process (no IPC roundtrip per
render); pick a small library — `marked` or `markdown-it` — at
implementation time, not before. No syntax-highlighting inside
code fences yet, no Mermaid, no math; surface those when asked.

### Per-pane open file lists

Phase 1 ships splits with one shared `openFiles` array and two
independent active selections — both panes show the identical
tab strip, only the active tab differs. Move to one open list
per pane (VSCode/Zed convention): each split has its own tab
strip, reordering is per-pane, closing a tab on one pane leaves
it open on the other, a file can live in one pane, both, or
neither. `WorkspaceSession` grows from one `open_files` to a
per-pane pair. Drag-between-panes comes later when someone
actually asks for it.

### New untitled tab + Save As / language re-detection on rename

`Ctrl+N` opens a fresh "untitled" buffer in the focused pane
with no path on disk. The first `Ctrl+S` against an untitled
buffer opens the native save dialog (Tauri); the chosen path
becomes the tab's path and the buffer joins `openFiles` as a
normal entry. `Save File As…` does the same rebind for an
already-saved file. In both cases the chosen extension drives
the language extension (typing in an untitled buffer then
saving as `foo.svelte` switches highlighting to Svelte; saving
as `foo.ts` switches to TypeScript). Untitled buffers do
**not** survive a restart — text is not persisted in
`WorkspaceSession`; closing a dirty untitled buffer fires the
same discard prompt as any other dirty file. Test plan:
[0006-untitled-tabs.md](../test-plans/0006-untitled-tabs.md).

### Keyboard focus between regions

`F6` / `Shift+F6` cycle through the major UI regions (file tree
→ editor pane(s) → status bar) in the layout-current order;
`Ctrl+0` jumps directly to the file tree; `Esc` from the file
tree returns focus to the active editor (the search input keeps
its native Esc). All three actions are surfaced in the command
palette so the keys are discoverable. The tree's single-click /
arrow-key selection now preview-opens files **without**
stealing focus from the tree; Enter or double-click is the
explicit "take me to the editor" gesture. Test plan:
[`0005-focus-regions.md`](../test-plans/0005-focus-regions.md).

### File deletion from the tree

`Delete` / `Backspace` moves the targeted paths to the OS trash
(XDG / Finder / Recycle Bin) via the cross-platform `trash`
crate; `Shift+Delete` / `Shift+Backspace` permanently removes
them (the team's recovery story is git for tracked files). Acts
on the full multi-selection when the keyboard cursor sits on a
selected row, otherwise on just the focused row (so arrow keys
after a click hit the row the user is on). Selecting a
directory and one of its descendants collapses to a single IPC
call. Both actions show a native confirm with mode-specific
wording (single-target = filename, multi-target = "N items"),
run IPC in parallel via `Promise.allSettled` with a single
failure toast, drop every tab the operation invalidates without
firing the per-tab dirty-discard prompt, and refresh the tree.
Pierre's search/rename inputs are protected so typing Backspace
inside them never triggers a delete. Test plan:
[0007-file-deletion.md](../test-plans/0007-file-deletion.md).

## Out of scope

Keybindings remain hardcoded for now. We add user-rebindable
keymaps when there's a concrete team request for it, not before.
