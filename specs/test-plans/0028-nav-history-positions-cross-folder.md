# Test plan 0028: Position-aware nav history + cross-folder navigation

- **Date**: 2026-05-04
- **Phase**: Phase 4 (LSP) — stage 2 follow-up

## What shipped

- Nav history now remembers caret position per entry, not just file path. Alt+Left restores you to the exact line and column you were reading, VS Code-style.
- Clicks inside a buffer push a new history entry; arrow keys / typing update the current entry in place. Forward stack is truncated on real navigations.
- Nav entries are folder-tagged, so Alt+Left/Right walks across bound folders and transparently switches the active folder when needed.
- Goto-definition now resolves across bound folders: if the LSP target sits inside a sibling bound folder, the editor jumps there instead of toasting the external URI.

## How to test

Prerequisites: `bun install`, `bun run tauri dev` against moon-ide itself. A second bound folder (any repo you have handy) for the cross-folder sections.

### Caret position in nav history

1. Open `src/lib/state.svelte.ts`. Scroll down and click on line 200.
2. Still in the same file, click on line 600. The caret moved from 200 to 600.
3. Press `Alt+Left`. Expected: caret lands back near line 200, **at the same column** you clicked. The file doesn't change (no tab swap); only the caret jumps.
4. Press `Alt+Right`. Expected: caret jumps to line 600.
5. Click on line 400 (with the caret at 600). Expected: new entry pushed at line 400. Forward stack is now truncated.
6. Press `Alt+Right`. Expected: nothing happens (forward truncated).
7. Press `Alt+Left`. Expected: caret back at line 600. `Alt+Left` again: line 200.

### Arrow keys update the tip, don't inflate history

1. Open `src/lib/state.svelte.ts`. Click on line 100 (tip entry = line 100).
2. Press `Down` five times without clicking. Caret is now at line 105.
3. Click on line 500 (new entry pushed at line 500).
4. Press `Alt+Left`. Expected: caret returns to **line 105**, not line 100. Arrow keys dragged the tip along.
5. Press `Alt+Right`. Expected: caret at line 500.

### No-move click doesn't push

1. Put the caret on line 50, column 10.
2. Click again at the exact same caret position (line 50, column 10).
3. Before and after, `Alt+Left` should have the same destination — a zero-delta click is treated as a refocus, not a nav. (If you don't have prior history, open two files first so there's something to go back to, then try the repeated click.)

### Click in a different file switches tab + pushes

1. Open `src/lib/state.svelte.ts`, click line 200.
2. Open `src/lib/components/Editor.svelte` from the file tree, click line 120.
3. Press `Alt+Left`. Expected: tab switches back to `state.svelte.ts`, caret at line 200.
4. Press `Alt+Right`. Expected: tab switches to `Editor.svelte`, caret at line 120.

### Cross-folder history (Alt+Left/Right)

Bind a second folder: in the sidebar, use "Add folder to workspace" to bind a second repo (e.g. any other project). Both folders appear in the folder bar.

1. With folder A active, open one of its files, click at line 50.
2. Switch to folder B (click its folder tab in the folder bar). Open one of its files, click line 30.
3. Press `Alt+Left`. Expected: active folder switches back to folder A, its tab re-opens, caret at line 50.
4. Press `Alt+Right`. Expected: active folder switches to folder B, its file re-opens, caret at line 30.
5. Repeat with deeper history: A/file1, A/file2, B/file1, B/file2 — walk back through all four with `Alt+Left`; walk forward with `Alt+Right`. Folder swaps happen at the A↔B boundary.

### Cross-folder goto-definition

Preconditions: two bound folders, at least one of them a TypeScript project that imports a module whose real definition lives inside the other folder (e.g. a monorepo where folder A depends on folder B, with a workspace symlink or `file:` path in its `package.json`).

1. Ctrl/Cmd-click on an identifier imported from folder B while viewing a file in folder A.
2. Expected: active folder switches to folder B, the target file opens, caret lands on the identifier name.
3. `Alt+Left`. Expected: active folder swaps back to A, caret at the original Ctrl/Cmd-click site (line + column preserved).
4. `Alt+Right`. Expected: back to folder B's target.

If the two folders aren't wired with cross-folder imports, skip this section — there's nothing the LSP can resolve across the boundary.

### Pure external targets still toast

1. In a TS file in any bound folder, Ctrl/Cmd-click on a built-in like `Promise` or a `@types/node` import.
2. Expected: toast reading `Definition outside workspace: file:///…/node_modules/…`. No tab opens. No folder switch.
3. This verifies the "external URI falls under no bound folder" branch still shows the existing fallback.

### Removing a bound folder prunes history

1. Build up history across folders A and B (see the cross-folder section).
2. Right-click folder B in the folder bar → "Remove folder from workspace" (or equivalent).
3. Press `Alt+Left` / `Alt+Right` repeatedly. Expected: history walks through folder A entries only; folder B's entries are gone. No flashes, no errors.
4. If a race leaves a stray folder-B entry in the forward stack, Alt+Right onto it shows a flash `Folder no longer in workspace: …` and the current view doesn't change.

## What must keep working

- Everything from test plan `0027`: Ctrl/Cmd-hover underline, Ctrl/Cmd-click jumps to in-workspace definitions, `Alt+Left`/`Alt+Right` keybindings (including word-motion fall-through on macOS when nav is empty).
- Markdown hovers (`0025`), diagnostics + completion (`0024`).
- Single-folder workspaces: nav history, goto-definition, pending-jump hand-off all work the same as before.
- Tab clicks, file-tree clicks, and split-view focus behaviour: setActive still drives nav pushes, but only via `pushFileSwitchEntry`, which coalesces re-clicks on the same tab.

## Known limitations

- Reopening a tab via a regular click (not via nav history) doesn't restore the last caret — only Alt+Left/Right does. Full per-buffer caret persistence is out of scope here; the Editor rebuilds state from `file.text` on path change, which resets CM's selection to offset 0.
- Nav history isn't persisted across app restarts. A full workspace-state reload re-initialises `navStack` to empty.
- The 6-line / symbol-boundary heuristics VS Code uses to promote "large" keyboard jumps into history entries are not implemented. Only mouse clicks and file switches push; everything else is tip-only.
- Cross-folder goto-definition depends on the LSP actually returning a target — `tsgo` running in folder A has no index of folder B, so only targets A imports that happen to resolve to files under B (via workspace symlinks, `file:` deps, or similar) will jump. A future improvement could run a broker per bound folder and dispatch requests based on the target URI; not shipped here.

## Related

- Specs: `specs/lsp.md` — "Navigation history" + "One-shot caret hand-off" sections updated for the position-aware, folder-tagged shape.
- Prior test plans: `0027-lsp-goto-definition-nav-history.md` (stage 2 baseline), `0024`/`0025` for the earlier LSP slices.
