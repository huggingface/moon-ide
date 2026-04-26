# Test plan 0001: Initial bootstrap (Phases 0, 1, and 1.5 polish)

- **Date**: 2026-04-26
- **Phase**: Phase 0 + Phase 1, plus the post-Phase 1 polish that closes the gaps surfaced while testing

## What shipped

### Phase 0 — Skeleton

- Tauri 2 shell (`src-tauri/`) + Svelte 5 frontend (`src/`).
- `WorkspaceHost` trait with a `LocalHost` implementation in `crates/moon-core/`.
- `moon-protocol` crate is the single source of truth for IPC types; TS types in `src/lib/protocol.ts` mirror it.
- IPC commands: workspace open/list/active, fs read-dir/read-file/write-file/stat, app-state load/save.
- File tree (Pierre Trees, vanilla JS) wired to `LocalHost::read_dir`.
- CodeMirror 6 editor with lazy-loaded language extensions. Open / edit / save round-trip works.

### Phase 1 — Editor + navigation

- Horizontal split (left / right pane) with focus tracking.
- Editor tabs per pane, with dirty marker.
- Custom command palette (`Ctrl+P` / `Ctrl+Shift+P`).
- File-name search and ripgrep-backed content search (Rust backend in `crates/moon-core/src/search.rs`).
- Hardcoded keybindings: `Ctrl+P`, `Ctrl+Shift+P`, `Ctrl+S`, `Ctrl+W`, `Alt+Left/Right` (planned, not yet wired).

### Phase 1.5 polish (post-test fixes)

- Editor indentation is hardcoded in `Editor.svelte`: `tab_size = 2`, tabs (not spaces). Phase 1.5 replaces this with per-file `.editorconfig` resolution. There is no `settings.json` — see [ADR 0006](../decisions/0006-no-settings-file.md).
- Tab rendering: tab markers are always on. Tabs show a small muted arrow at their left edge. We do **not** mark spaces — CM6's space dots were too noisy on this theme, and the team uses tabs for indentation so the spaces signal carried little value. Implemented as a custom `highlightTabs()` extension in `src/lib/editor/highlightTabs.ts` (a `MatchDecorator` over `/\t/g`), not CM6's `highlightWhitespace`. There used to be an `editor.render_tabs` knob; it had no concrete consumer at toggle-off time, so it was inlined as part of the `settings.json` deletion (ADR 0006). If the team wants it back, add a constant in `Editor.svelte` first; promote to `.editorconfig` only when a per-file flip is actually needed.
- Restore last opened workspace on launch. Workspace path is part of `AppState.last_session` and persisted to `<app_config_dir>/state.json` after every relevant mutation; restored synchronously in Tauri's `setup` so the frontend's first `workspace.active()` call already sees it. Missing folders are logged but the saved entry is **not** cleared (covers the unmount-USB-drive case). The `moon-core::app_state` module owns load / save and has its own tests. Phase 7 grows `last_session` into a recent list — see the Phase 7 entry in the roadmap.
- File tree dark theme: corrected `--trees-*-override` CSS variables in `src/styles.css`.
- Search-input top padding restored.
- Dotfiles (`.editorconfig`, `.husky/`, etc.) are visible in the tree; only `.git/` is hidden host-side. Real `.gitignore`-aware fading is deferred to Phase 5.
- `Ctrl+W` closes the active tab.
- Dirty marker now reverts when the buffer matches the saved bytes (e.g. after `Ctrl+Z` undo). Uses an FNV-1a 32-bit fingerprint + length, not a second copy of the full text — see `src/lib/util/hash.ts`.
- Rust syntax highlighting (`@codemirror/lang-rust`).
- TOML syntax highlighting via `@codemirror/legacy-modes`.
- Filename-based language map for files without useful extensions: `Cargo.lock` → TOML, `bun.lock` → JSON, `.editorconfig` / `.npmrc` → properties (INI).
- Image viewer for `png/jpg/jpeg/gif/webp/svg/bmp/ico/avif`. Uses Tauri's asset protocol via a new `WorkspaceHost::absolute_path` and `convertFileSrc` on the frontend; no image bytes flow through the IPC.
- New IPC: `fs.absolutePath(path)` exposes `WorkspaceHost::absolute_path` to the UI.
- Bundler warning fix: `@tauri-apps/plugin-dialog` is now statically imported in `commands.svelte.ts` (it was already static elsewhere — `INEFFECTIVE_DYNAMIC_IMPORT` is gone).
- `vite.config.ts` raises `chunkSizeWarningLimit` to 1500 with a comment explaining why a Tauri-served bundle is a different beast from a web app.
- Editor focus follows file activation. New `WorkspaceState.focusTick` counter that the active `Editor.svelte` reads as a reactive dependency; bumped from `setActive` (tab clicks, tree clicks, command palette) and from `closeFile` when a fallback tab takes over. Editor calls `view.focus()` via microtask so the click that triggered the bump finishes settling first. This means arrow keys / typing land in the editor immediately after opening or switching files instead of staying in the tree or on the tab strip.
- File tree mirrors the active file. The previous "deselect closed files" effect was generalised: a single `$effect` in `FileTree.svelte` keeps Pierre's selection equal to `{ activePath }` (or empty when no file is active). Tab click → row highlight follows. Closing the last tab → row clears so the next tree click is a real selection change. Early-returns when already in sync to avoid feedback loops with the tree's own `onSelectionChange`.
- Closing a dirty file shows a native confirm dialog ("Discard / Cancel"). `closeFile` is now async; it only proceeds past the dialog on Discard. Two-button rather than three because `@tauri-apps/plugin-dialog` is OK/Cancel only; the "Save" path stays at `Ctrl+S` for now (a custom 3-way modal can land later if anyone trips over the omission). Capabilities updated: `dialog:allow-confirm`.
- Tab strip is draggable. Tabs carry `draggable="true"`; an HTML5 drag exchanges a small `application/x-moon-tab` payload (path) and the strip's `dragover` decides drop position by the cursor's left/right halves of the hovered tab (or "after the last tab" when over empty strip space). A new `WorkspaceState.moveFile(fromPath, beforePath | null)` does the immutable reorder. `user-select: none` on tabs stops the click-and-drag-selects-text bug. Drop position is shown with a 2px accent stripe at the leading edge of the target (or trailing edge of the strip for "drop at end"). Out of scope for now: dragging across panes, dragging the close button, dropping outside moon-ide.
- Session persistence (open files + active tab) extended beyond just the folder path. New `WorkspaceSession` type in `crates/moon-protocol/src/session.rs` (with `SplitSide`); `AppState` now carries `last_session: Option<WorkspaceSession>` plus `theme: ThemeMode`. Frontend persists on every mutation that changes the visible state — `openLocal`, `openFile` (via `setActive`), `closeFile`, `setActive`, `splitActive`, `closeSplit`, `focusSide`, `toggleTheme` — through a microtask-coalesced `persistAppState()` that lands a single IPC roundtrip per tick. On launch, `App.svelte` calls `workspace.restoreAppState()` after the workspace is mounted; files that no longer exist are dropped silently and the cleaned-up list is saved back. Per AGENTS.md "no premature migrations" we did not add a fallback for any previous on-disk shape — corrupted/old `state.json` parses fail gracefully (warn + default).
- `Settings` (and the `<workspace>/.moon/settings.json` file) deleted entirely. Project style is `.editorconfig` (Phase 1.5); per-machine state (theme + last session) is `AppState`. See [ADR 0006](../decisions/0006-no-settings-file.md). Tauri now exposes a single `app_state_load` / `app_state_save` pair instead of separate `settings_*` and `session_*` commands.

### Process additions

- AGENTS.md gains a "no pre-existing warnings" rule and a bootstrap exception clause in "Scope discipline".
- AGENTS.md gains a "no premature migrations" rule: until the roadmap's last phase ships, schemas (settings, persisted app state, JSON-RPC) can be renamed/restructured/deleted freely without compat shims. A best-effort warn + default fallback is fine; crashing on startup is not.
- `specs/test-plans/` introduced. This file is the first entry. README explains when to write one and when to skip.

## How to test

Prerequisites:

- Linux dev libs for Tauri 2 (see `README.md` "Tauri prerequisites").
- `bun install` at repo root.
- A real codebase to open — moon-ide itself is fine (it bootstraps).

### Quality gates (must pass before opening the app)

```bash
bun run fmt:check
bun run lint
bun run check
bun run test
bun run build:vite
```

Expected: all five exit 0, **and** `bun run build:vite` is silent (no Vite warnings, no `INEFFECTIVE_DYNAMIC_IMPORT`, no chunk-size warning). (The full `bun run build` runs `tauri build`, which bundles the Rust shell — slow and not what we want for a quick gate.)

### App smoke test

```bash
bun run dev
```

1. **Open folder.** Either `Ctrl+P` → "Open Folder…" or click the welcome-screen button. Pick `~/code/moon-ide` itself.
2. **File tree.**
   - Tree renders against the dark background — no white flash anywhere on it. The search input has visible breathing room above it.
   - Dotfiles `.editorconfig`, `.husky`, `.lintstagedrc.json`, `.oxlintrc.json` are listed. `.git/` is **not** listed.
3. **Open a Rust file.** `crates/moon-core/src/host.rs`. Syntax colors apply: keywords, strings, types are differentiated.
4. **Open `Cargo.toml`.** TOML highlighting (sections, keys, strings).
5. **Open `Cargo.lock`.** Same TOML highlighting as `.toml` files — the filename-based fallback fires.
6. **Open `bun.lock`.** JSON highlighting.
7. **Open `.editorconfig`.** Properties (INI) highlighting — section headers in brackets, `key = value` pairs.
8. **Open an image.** `src-tauri/icons/128x128.png`. Image viewer shows the icon on a checker-board, footer reports `128 × 128`. The same content area shows nothing else (no editor below).
9. **Edit a text file.**
   - Type a character. Title bar / tab gets a dirty dot.
   - `Ctrl+Z` until the buffer is back to disk content. The dot disappears immediately. (Smoke test on the fingerprint comparison.)
   - Type one character of identical content (e.g. delete a `f`, retype `f` at a different point). Dot stays — different content of identical length is still dirty.
10. **Save.** `Ctrl+S`. Dot disappears, mtime in tooltip updates.
11. **Tabs.**
    - `Ctrl+W` closes the active tab.
    - Multiple tabs in one pane: clicking a tab focuses it; the focused pane shows the accent bar.
    - Click any tab → the editor area itself is focused (cursor visible, arrow keys move the caret, no need to click again into the editor body).
    - **Drag a tab's body**: it moves left/right in the strip. Drop position previewed as a 2px vertical accent stripe at the leading edge of the target tab; dropping past the last tab snaps to the trailing end. The dragged tab fades to 50% while in flight. Holding a tab and slowly moving the mouse across its label does **not** select the text — that's the `user-select: none` fix; before it, dragging selected the labels of every tab the cursor passed over.
    - Open the same file in both panes, drag a tab on one side; the other side's strip reorders identically (tab order is shared).
12. **Tree click behavior.**
    - Click a file in the tree → it opens **and** the editor takes focus. Press an arrow key immediately; the caret moves in the editor, not the tree.
    - Click the same already-active file in the tree → focus snaps back into the editor.
    - Open a file, close its tab with the × button, then click the same row in the tree again → it reopens. (Before this fix, the second click was silent because the row was still selected.)
13. **Dirty close confirm.**
    - Edit a file so it's dirty. Click the tab × → native dialog "{filename} has unsaved changes. Discard them?" with `Discard` / `Cancel`.
    - `Cancel` → tab stays open, edits intact, dirty marker still showing.
    - `Discard` → tab closes, edits lost (intentional).
    - Same flow via `Ctrl+W` on a dirty file. Same flow on the close button of a non-active dirty tab.
    - Closing a clean tab does **not** prompt — instant close.
14. **Split.**
    - Open one file in the left pane. Drag a file from the tree into the right pane (or use the command palette).
    - Each pane has its own active tab; closing a tab on one side does not affect the other.
15. **Command palette.**
    - `Ctrl+P` lists files. `Ctrl+Shift+P` lists commands. Both are keyboard-navigable.
16. **Search.**
    - File search returns results within ~50 ms on the moon-ide repo.
    - Content search via ripgrep backend returns hits with surrounding context.

### Indentation behavior

- Hardcoded in `Editor.svelte`: `tab_size = 2`, tabs not spaces. Typing Tab in a text file inserts a literal `\t`; column width displays as 2.
- There is no `settings.json` to flip these. Phase 1.5 wires `.editorconfig` and these constants get replaced by per-file resolution.

### Whitespace rendering

- Open any file with mixed indentation. Each tab shows a faint left-anchored arrow; spaces stay invisible (no dots).
- Tab markers are always on. There is no setting to hide them — see [ADR 0006](../decisions/0006-no-settings-file.md).

### Theme

- Open command palette → "Toggle Theme (light/dark)". UI repaints immediately. Quit and relaunch — the chosen theme survives the restart (it's persisted in `AppState.theme`, not in any project file).

### Session restore (folder + tabs + active)

1. Open a folder. Open three files; switch to the second so it's active. Quit the app.
2. Relaunch via `bun run dev`. Same folder reopens, same three tabs in the same order, the second one is active. Caret is in the editor (arrow keys move the caret immediately, no extra click).
3. The matching tree row is highlighted as the active file (you should not have to scroll-and-click in the tree to confirm which file is which).
4. With multiple tabs open, click each one in turn. The tree-row highlight follows the active tab.
5. Close all tabs. Tree selection clears. Click any file in the tree → it opens (selection-change event fires correctly).
6. Move one of the open files away externally between launches (e.g. `mv ~/code/example/foo.txt /tmp/`). Relaunch. The other tabs restore; the missing one is silently dropped; no error toast. Stop the app and inspect `<app_config_dir>/dev.moon-ide.desktop/state.json` — `open_files` no longer contains the dropped path.
7. Move the entire workspace folder away. Relaunch. Welcome screen appears (no crash, no toast). Stderr shows a `failed to restore last workspace` warning. Move it back, relaunch — full session (folder + tabs + active) restores.
8. On a fresh OS user (or after deleting `<app_config_dir>/dev.moon-ide.desktop/state.json`), launch — welcome screen appears, no error.
9. With a state file from a previous schema (e.g. one that still has the old `last_workspace_path` field, or anything else `serde` rejects), launch — stderr shows `app state parse failed; ignoring`, app starts on the welcome screen, no crash. (This is the "no premature migrations" path.)

## What must keep working

- The UI must not call `tauri.invoke` for anything outside `src/lib/ipc.ts`. (Grep for `invoke<` outside that file — should return zero hits.)
- The dirty marker is correct in three cases: (a) edit then revert via undo → not dirty; (b) edit then revert by retyping the original → not dirty; (c) two edits that each toggle a character but result in same length / different bytes → dirty.
- Image opens never call `fs.readFile`. Verify by adding a `console.log` in the IPC layer if regressions are suspected.
- `bun run build:vite` stays silent; new warnings are bugs.
- Workspace-relative paths are the only thing the UI sees. Absolute paths only enter the UI via `ipc.fs.absolutePath()` and only for the asset-protocol case.
- `.git/` stays hidden in the tree. No other directories are filtered host-side.
- File-name language matches happen **before** extension matches, so `Cargo.lock` is TOML, not no-language.

## Known limitations

- Image viewer is read-only and has no zoom/pan. Out of scope until requested.
- SVG files open as images. To edit one as text, you'd currently rename to `.svg.txt`. We add an "Open as Text" command if the team needs it.
- `properties` mode is a reasonable approximation for `.editorconfig` / `.npmrc` — it's not a strict EditorConfig grammar.
- `bun.lock` highlighting routes through plain JSON. Bun's lockfile is JSONC-tolerant in theory; we don't currently allow comments. If `bun` ever writes them, we route to a JSONC mode.
- Asset-protocol scope is `["**"]`. Acceptable because the IDE has full FS access by design; revisit if we ship to non-developer users.
- `WorkspaceHost::absolute_path` returns a host-side path. For Phase 2 remote hosts this can't be fed to `convertFileSrc` directly; the remote impl will need a different image strategy (data-URL over JSON-RPC). Trait doc-comment notes this.
- `Alt+Left` / `Alt+Right` navigation history is in the keybinding table but not wired yet (LSP-driven, lands with Phase 4).

## Related

- ADRs: [0001 — Tauri](../decisions/0001-tauri.md), [0002 — workspace host](../decisions/0002-workspace-host.md), [0003 — adapters](../decisions/0003-adapters.md), [0004 — code style](../decisions/0004-code-style.md), [0005 — bootstrap](../decisions/0005-bootstrap.md).
- Specs: [architecture.md](../architecture.md), [protocol.md](../protocol.md), [editorconfig.md](../editorconfig.md), [roadmap.md](../roadmap.md).
- Prior test plans: none — this is the first.
