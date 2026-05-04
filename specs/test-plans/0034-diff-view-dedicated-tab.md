# Test plan 0034: Diff view in a dedicated tab

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git)

## What shipped

- Diff view for a modified file now opens a dedicated tab at synthetic path `moon-diff:<realPath>`, living alongside any existing editor tab. `Alt+Left` navigates back to the editor view.
- `OpenFile` gains `isDiffTab: boolean` + `realPath: string`; `workspace.openDiffTab(realPath)` is the one entry point (idempotent — focuses an existing diff tab rather than duplicating).
- Deleted files keep their prior shape (single tab held in diff mode by `isDeleted: true`).
- The old per-path `diffModes` toggle / `toggleDiffMode` / `setDiffMode` / `diffModeFor` helpers are gone. Command palette entry is renamed **Git: View Diff** (opens the tab) and is hidden when the active buffer is already a diff tab or a deleted file.
- Diff tabs skip save, LSP, blame, HEAD-gutter, and editorconfig seeding. `updateText` on a synthetic diff path is a no-op guard.
- `Alt+Left` / `Alt+Right` promoted to a window-level handler (was a CodeMirror keymap binding): now fires from diff tabs, image tabs, and anywhere else that isn't an `<input>` / `<textarea>`. Always swallows the event — a stale press with no history still eats the keystroke instead of leaking through to the browser / CM defaults.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a bound git-repo folder with at least one commit.

### Happy path — modified file

1. Open a tracked file and edit a few lines.
2. Right-click the row in the file tree → **View diff**. A new tab appears titled `<filename> (diff)`, focused, showing the split diff.
3. `Alt+Left`. Focus swaps to the editor tab (if it's the previously visited one) or the previous tab in history.
4. `Alt+Right`. Back to the diff tab.
5. Type more edits in the editor tab. Switch to the diff tab: the right side reflects the live buffer, not just a snapshot from when the diff was opened.
6. Close the diff tab — editor tab is unaffected.
7. Close the editor tab while the diff tab stays open. Switch to the diff tab: the diff still renders (the diff tab's `$effect` falls back to `readFile` for the "after" side).

### Alt+Arrow across tab kinds

1. From a diff tab, press `Alt+Left`: navigates back. From an image tab (`*.png`), `Alt+Left`: navigates back. Both used to no-op because the keymap was CM-scoped.
2. On a fresh session with no history, press `Alt+Left` inside an editor: nothing happens, _and_ the caret doesn't word-jump. The event is swallowed.
3. Open the command palette (`Ctrl+P`), type into the search box. `Alt+Left` moves the caret one word left — text inputs are exempt from the global swallow.

### Happy path — deleted file (regression check for plan 0032)

1. Delete a tracked file via the tree's **Discard changes** / external `rm`.
2. Click the deleted row. A _single_ tab opens, showing the diff with the whole file as a deletion (left side = HEAD, right side = empty).
3. No "(diff)"-suffixed second tab appears — deleted files don't get both shapes. `Git: View Diff` is hidden from the palette for this tab.

### Palette & context menu

1. On a modified file's active editor tab: `Git: View Diff` is visible. Run it → a new diff tab opens. Running it again: the existing diff tab is focused (no duplicate).
2. On the diff tab itself: `Git: View Diff` is hidden (it's already what the tab is).
3. On a clean / added / untracked / ignored file: `Git: View Diff` is hidden.

### Session restore

1. Open a diff tab. Quit the IDE.
2. Relaunch. The diff tab comes back in the same position; right-side text re-fetches on mount (live buffer if the editor tab was also restored, else disk).
3. If the real path has been deleted externally since quitting, the diff tab opens with an empty "after" side — no crash, no toast spam. Closing the diff tab recovers.

### Save / LSP / blame isolation

1. In a diff tab, press `Ctrl+S`. Nothing happens (no toast, no error). Keep typing in the editor tab; saves behave normally there.
2. LSP diagnostics in the editor tab remain normal (squigglies, hover, completion) — the diff tab doesn't open a second LSP session for the same path.
3. Inline git blame on the editor tab's current line keeps working. Diff tab has no blame overlay by design.
4. The editor tab's git-change gutter + overview ruler (plan 0033) keep updating with every edit.

## What must keep working

- Every prior Phase-5 gesture: tree markers, discard changes, refresh on fs events, inline blame + PR linking, diff view basics from plan 0032, git-change gutter + overview ruler (0033).
- Dual-split: opening a diff tab into the right pane via drag / context-menu-on-right-pane stays independent from the left pane.
- Tab close, reorder, drag-between-panes behave identically for diff tabs and editor tabs.
- `Ctrl+N` untitled flow, save-as, rename — diff tabs stay out of these code paths entirely.
- `bun run check:ts`, `check:svelte`, `lint:js`, `lint:rust`, `cargo check` all pass without new warnings.

## Known limitations

- The diff tab's "after" side is live only when the editor tab is open in the **same folder**. A diff tab open across a folder swap falls back to its on-disk snapshot until the editor tab is reopened (untested niche; documented rather than fixed).
- No "jump to editor" affordance inside the diff tab yet — users reach it via `Alt+Left` or by clicking the editor tab in the strip.
- Diff tabs aren't split-aware in a clever way: dragging a diff tab to the other pane works, but there's no "open diff in opposite pane" shortcut. Matches Phase 5's scope.
- The diff tab's title carries a `(diff)` suffix rather than a dedicated icon. Icons come with a broader tab-chrome pass later.
- External deletion of the real path while the diff tab is open leaves the "after" side empty without re-prompting the user. A retry affordance would be nice but isn't wired up.

## Related

- `specs/test-plans/0032-git-diff-view.md` — the original diff view (in-place toggle). Superseded by this plan.
- `specs/test-plans/0033-git-change-gutter.md` — the in-editor gutter + overview ruler pair.
- `specs/roadmap.md` — Phase 5 bullet updated to reflect the dedicated-tab model.
