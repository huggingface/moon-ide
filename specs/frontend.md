# Frontend

STATUS: partial — Phase 0 ships file tree + single editor + tabs scaffolding.

## Stack

- Svelte 5 (runes) + TypeScript
- Vite (Tauri's recommended bundler)
- CodeMirror 6 for the editor
- `@pierre/trees` (vanilla mode) for the file tree
- CSS variables for theming, no CSS framework
- No global CSS reset beyond a tiny `:root` defaults block

## Module layout

```
src/
├── App.svelte                  Root layout: sidebar | editor area | status bar
├── main.ts                     Mount + global setup
├── styles.css                  CSS variables (theme tokens) + a small reset
├── lib/
│   ├── ipc.ts                  Typed wrappers around Tauri commands
│   ├── protocol.ts             TS types mirroring crates/moon-protocol
│   ├── state.svelte.ts         Workspace/editor state ($state runes)
│   ├── components/
│   │   ├── FileTree.svelte     Wraps @pierre/trees vanilla
│   │   ├── Editor.svelte       Wraps CodeMirror 6
│   │   ├── EditorTabs.svelte   Tab strip
│   │   ├── StatusBar.svelte
│   │   └── Sidebar.svelte
│   └── editor/
│       └── language.ts         Lezer/CM6 language extension picker per file
```

## State

Workspace and editor state lives in `state.svelte.ts` using Svelte 5 `$state`. Single shared store; components read it directly via `import { workspace } from '$lib/state.svelte'`.

```ts
type WorkspaceState = {
	rootPath: string | null;
	paths: string[]; // flat path list for Pierre Trees
	openFiles: OpenFile[];
	activeFileId: string | null;
};
```

Avoid Svelte stores (`writable`) — runes are simpler for app-level state. Use stores only for cross-cutting reactive primitives if we need them.

## IPC convention

Every Tauri command is wrapped in `lib/ipc.ts` with a typed function:

```ts
export const ipc = {
	fs: {
		readDir: (path: string) => invoke<DirEntry[]>('fs_read_dir', { path }),
		readFile: (path: string) => invoke<ReadFileResult>('fs_read_file', { path }),
		// ...
	},
};
```

Components never call `invoke` directly. This keeps the Rust/TS surface auditable in one file.

## Editor

CodeMirror 6 is configured in `Editor.svelte`. Language extensions are loaded per file (lazy via dynamic import). Phase 0 includes TS/JS, JSON, CSS, MD, HTML.

State per editor:

- doc + selection + scroll
- dirty flag
- file mtime at load (to detect external changes later)
- per-tab view-state snapshot (caret + anchor + `scrollTop` + serialized CM history) cached on `WorkspaceState.viewStateByKey`, captured right before a tab swap and replayed on return. Includes the undo history (`EditorState.toJSON({ history: historyField })`) so `Ctrl+Z` after `Alt+Left`-ing back to an unsaved buffer still walks through edits made before the user clicked away. Dropped when the buffer falls out of every pane (`closeFile`'s GC block), so a fresh open from the tree starts with an empty undo stack.

Saves go through `ipc.fs.writeFile`. The editor only tracks dirty state; persistence is owned by the workspace state module.

### Extensions baseline

`baseExtensions()` in `Editor.svelte` is the single source of truth for what every editor instance gets. It's deliberately explicit rather than pulled from `codemirror`'s kitchen-sink `basicSetup` — we want to know exactly which behaviours are on:

- **Line numbers + active line/gutter** — structural.
- **`bracketMatching` + `closeBrackets`** — matches the paren/bracket/brace under the caret, and auto-inserts the matching close glyph on opener input. `closeBracketsKeymap` is merged into the keymap so backspace on an empty pair deletes both sides.
- **`indentOnInput` + `history`** — language-aware reindent after tokens and full undo/redo.
- **`highlightSelectionMatches`** — highlights other occurrences of the current selection in the buffer (matches VSCode's "selection highlights").
- **`autocompletion({ activateOnTyping: false })`** — completion surface only; no source is registered yet. Opens explicitly on Ctrl-Space or when a future LSP source dispatches one. We keep the popover off the typing path because the built-in identifier-from-document source is too noisy to show unprompted, and the real sources (LSP, snippets) arrive in a later phase.
- **`foldGutter`** — code folding driven by each language's Lezer grammar (`languageData.foldNodeProp`). Gutter sits immediately right of the line numbers; click the marker to fold/unfold. Fold state is in-memory per editor instance — we don't persist folds across file reopens. Wired into `DiffView` on both sides too: `@codemirror/merge` re-measures and rebalances its block spacers when a side's height changes (its update listener calls `updateSpacers` on `heightChanged`), so folds on one pane don't desynchronise the alignment — the other pane gains spacers at the next unchanged-region boundary. We deliberately skip CM's `foldKeymap` because `Ctrl-Alt-[` / `Ctrl-Alt-]` (foldAll / unfoldAll) shadow the AltGr-`[` / AltGr-`]` glyphs on French AZERTY and other AltGr-using layouts (Linux browsers report AltGr as `ctrlKey + altKey`, and CM's keyName-based match fires before any layout heuristic) — installing the keymap would mean a French-keyboard user couldn't type `[` without folding the entire file. Add a layout-stable keyboard binding (F-key, or a `Ctrl+K` leader sequence) if one becomes a real ask.
- **Tab / indent** — `indentWithTab` keyed, indent unit and tab size driven by the `editorConfigCompartment`.

Anything not in that list is intentionally off. Notably **no** rectangular selection, **no** rainbow brackets (deferred — wants a Lezer-aware scan to skip strings/comments; will land as a small standalone extension when it's someone's actual itch).

### Diff and conflict surfaces

Different jobs, different tools:

- **Working-tree diff** (modified-file `View diff`, deleted files): `@codemirror/merge`'s `MergeView` — two CodeMirror editors side by side, sharing the regular editor's language / theme / editorconfig stack. Left side (`HEAD`) is read-only, right side (working tree) is editable so the user can fix things up directly inside the diff. `Ctrl+S` writes the right-hand buffer back through the normal `saveActive` path.
- **Read-only diffs** (commit detail, SCM review panel, PR hover): same `@codemirror/merge` engine, configured with both sides read-only. Avoids carrying two diff renderers around.
- **Review changes pseudo-tab** (`review://default-branch`, see Phase 5 §5.4): same `@codemirror/merge` engine, stacked one `MergeView` per changed file inside a single scrollable container; each section lazy-mounts on `IntersectionObserver` hit so opening a 100-file branch stays snappy. **Left side (base / `HEAD`) is read-only; right side (working tree) is editable** for everything except `deleted` rows, matching the working-tree-diff surface above. Edits route through `workspace.updateText` against a lazily-attached `OpenFile`; `Ctrl+S` from inside a section saves the underlying file via `saveReviewSection` (the global Ctrl+S handler delegates when the active tab is the synthetic `review://` buffer and a section has focus). Ctrl/Cmd-click goto-definition is wired on the right side with a lazy first-modifier-hover attach (the section calls `ensureBackingBuffer` only when the user is actually probing it, so broker traffic stays proportional to user intent); the jump itself routes through `workspace.jumpTo`, which opens the target as a regular editor tab in the same pane — exactly the "leave review mode and land on the function" behaviour. Hover / completion / diagnostics still aren't wired (they'd require eager `didOpen` on every visible section); click the section header's path to open the file as a regular tab when those are wanted.
- **Editable merge conflicts**: handled in the regular CM editor (not a MergeView) by the `editor/conflictMarkers.ts` extension. It decorates the `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` block in place and overlays an inline `Accept current` / `Accept incoming` / `Accept both` widget on the opening marker line. Same keybindings, theme, selection, undo stack as any other buffer — the conflict surface is just the regular editor with extra decoration. Gated on `gitStatusEntries[path].status === 'conflicted'` so files that happen to contain marker syntax (this very document, test fixtures) stay inert. The full conflict-resolution flow (SCM panel reshape, abort, auto-stage on save, commit-merge with soft-warn on residual markers) lives in Phase 5 §5.6.
- **Main editor buffer**: CodeMirror 6, as above.

The split matters because Pierre Diffs is display-only — forcing it to also edit would either duplicate CM's work or give us a second inferior editor. Keeping CM for every editable surface keeps "what works in the main editor" identical to "what works in the conflict editor".

### LSP hook

Planned shape (not yet built): a per-file `ViewPlugin` subscribes to moon-core's LSP broker (one adapter per capability — hover, go-to-def, diagnostics, completions). The adapter translates LSP events into CM's `hoverTooltip`, `gotoDefinition` command, `setDiagnostics` effect, and `autocompletion` source respectively. Nothing goes to the UI that didn't come through moon-core — no direct LSP process ownership on the frontend.

Git blame is shaped the same way: a `ViewPlugin` that reads per-line blame from moon-core and decorates the gutter with a soft right-aligned annotation. Both ride the same infra that discard-changes already uses (workspace state → IPC → rust core → surface).

## File tree

`@pierre/trees` `FileTree` class is instantiated in `FileTree.svelte`'s `onMount`. Mounted into a div via `tree.render({ containerWrapper })`. Selection and double-click are wired to `workspace.openFile(path)`. The component cleans up via `tree.cleanUp()` in `onDestroy`.

We don't wrap Pierre Trees behind an adapter — we use it directly. If we need to swap implementations later, this single component is the only place that changes.

The path list fed to Pierre comes from `WorkspaceHost::collect_paths` (one IPC, full recursive walk on the blocking pool). Two performance choices matter:

- **Backend prunes gitignored directories.** Before walking, `collect_paths` runs `git status --porcelain=v1 -z --ignored=matching` to learn which directories git would collapse to a single `!! dir/` row. The walk emits each such directory as a single collapsed entry and **does not enumerate its descendants**. On a moon-ide-sized repo this drops the path count from ~127k to ~25k — the bulk of the savings is `node_modules/` and `target/`. Non-repo folders see an empty skip set and walk everything (no authoritative ignore source to consult).
- **Backend depth cap with lazy frontier.** `collect_paths` recurses to `MAX_TREE_DEPTH=16` (frontend constant). Directories whose recursion the cap stops short of return alongside the path set in `CollectPathsResult.depth_capped`; the frontend feeds them into the same lazy-load bucket as gitignored directories. 16 covers every realistic project (deepest SvelteKit `[param]/` route stacks, monorepo `packages/<scope>/<name>/src/**`); the lazy frontier is defence-in-depth for genuinely deeper trees we haven't seen yet. Empty leaf directories at the cap aren't marked lazy — there's nothing to fetch on expansion.
- **Lazy descendant fetch.** When the user expands a collapsed-ignored or depth-capped directory in the file tree, `FileTree.svelte` issues `fs_collect_paths_under(rel, max_depth=0)` to fetch its direct children and batch-adds them to Pierre. The same response carries a `depth_capped` list: only those direct children get re-flagged as lazy, so drilling deeper into `node_modules/foo/bar/` re-issues the command at the next level while empty leaves stay non-lazy. Loaded paths are tracked separately from `workspace.paths` and re-unioned into the merged path set on every refresh so the path-set effect's `applyPathsDiff` doesn't churn them. Folder switches reset the lazy bucket; same-folder watcher refreshes preserve it.
- **`resetPaths` over `applyPathsDiff` for wholesale changes.** Pierre's `tree.batch([{type:'add', path}, …])` runs an event-per-op emit + child-index update; on 80k+ paths that's seconds of main-thread time. `resetPaths` rebuilds the path store in one shot. `FileTree.svelte` picks `resetPaths` whenever the active folder changes, when the previous snapshot was empty and the next isn't, and on first mount; otherwise small steady-state diffs (save, delete, watcher event) use the cheaper `applyPathsDiff`. See [test plan 0076](test-plans/0076-folder-switch-perf.md).

## Theming

CSS custom properties only. The only stylesheet-global signal is the `.light` class on `:root`: present → light palette, absent → dark palette. Pierre Trees is themed via the same custom properties (it reads CSS vars from the host).

The user picks one of three modes in the status-bar theme picker — **System**, **Dark**, **Light** — persisted in `AppState.theme` (see `crates/moon-protocol/src/theme.rs`). System is the default for fresh installs.

- The picked mode is stored verbatim. The painted palette is the _resolved_ `WorkspaceState.effectiveTheme`, which is `dark` / `light` when the user picked one of those and the OS preference when they picked `System`.
- The OS preference is resolved by the desktop shell via the `system_theme` Tauri command. On Linux / BSD the command talks to the XDG Desktop Portal (`org.freedesktop.appearance color-scheme`) through `ashpd`, which is the same channel Firefox and Chromium listen on; on macOS / Windows it forwards `tauri::WebviewWindow::theme()`. The detour matters because in a WebKitGTK webview both `matchMedia('(prefers-color-scheme: dark)')` and Tauri's own `getCurrentWindow().theme()` ignore the GTK / GNOME / KDE preference and default to light, so System mode flipped to light on every launch for users on a dark desktop. `restoreAppState` awaits the probe before the first `applyTheme` call so the UI doesn't flash.
- Live OS-theme flips go through the same platform split. On Linux a tokio task in the desktop shell subscribes to `Settings.receive_color_scheme_changed` via ashpd and re-broadcasts each change as the `system:theme-changed` Tauri event, which the frontend `listen`s for. On macOS / Windows `getCurrentWindow().onThemeChanged` fires directly on the webview, so the Linux watcher compiles to a no-op there. `matchMedia` is left wired as a last-resort fallback for non-Tauri dev shells (vite-only).
- Surfaces that can't read CSS variables (CodeMirror's `dark: boolean` build-time flag, xterm.js's option-bag palette) have their own `$effect` blocks keyed on `effectiveTheme` and reconfigure when it flips.

## Window state

Window size, position, maximized, and fullscreen state are persisted by the official `tauri-plugin-window-state` plugin. It writes its own JSON next to `state.json` and hooks window creation / close automatically — we don't roll our own because it handles monitor placement and DPI edge cases we'd rather not reimplement. That means this slice of UI state lives outside `AppState`; everything else (theme, last session, bottom panel) stays in `state.json`.

## Keymap

Cm6 default keymap + a small layer of app-level shortcuts wired in `App.svelte`:

- `Cmd/Ctrl+S` — save active file
- `Cmd/Ctrl+P` — quick open (Phase 1)
- `Cmd/Ctrl+Shift+P` — command palette (Phase 1)

Linux uses `Ctrl`; macOS `Cmd`. We test for `navigator.platform`.
