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

Saves go through `ipc.fs.writeFile`. The editor only tracks dirty state; persistence is owned by the workspace state module.

## File tree

`@pierre/trees` `FileTree` class is instantiated in `FileTree.svelte`'s `onMount`. Mounted into a div via `tree.render({ containerWrapper })`. Selection and double-click are wired to `workspace.openFile(path)`. The component cleans up via `tree.cleanUp()` in `onDestroy`.

We don't wrap Pierre Trees behind an adapter — we use it directly. If we need to swap implementations later, this single component is the only place that changes.

## Theming

CSS custom properties only. The only stylesheet-global signal is the `.light` class on `:root`: present → light palette, absent → dark palette. Pierre Trees is themed via the same custom properties (it reads CSS vars from the host).

The user picks one of three modes in the status-bar theme picker — **System**, **Dark**, **Light** — persisted in `AppState.theme` (see `crates/moon-protocol/src/theme.rs`). System is the default for fresh installs.

- The picked mode is stored verbatim. The painted palette is the _resolved_ `WorkspaceState.effectiveTheme`, which is `dark` / `light` when the user picked one of those and the OS preference when they picked `System`.
- The OS preference is resolved by the desktop shell via the `system_theme` Tauri command. On Linux / BSD the command talks to the XDG Desktop Portal (`org.freedesktop.appearance color-scheme`) through `ashpd`, which is the same channel Firefox and Chromium listen on; on macOS / Windows it forwards `tauri::WebviewWindow::theme()`. The detour matters because in a WebKitGTK webview both `matchMedia('(prefers-color-scheme: dark)')` and Tauri's own `getCurrentWindow().theme()` ignore the GTK / GNOME / KDE preference and default to light, so System mode flipped to light on every launch for users on a dark desktop. `restoreAppState` awaits the probe before the first `applyTheme` call so the UI doesn't flash.
- Live OS-theme flips go through the same platform split. On Linux a tokio task in the desktop shell subscribes to `Settings.receive_color_scheme_changed` via ashpd and re-broadcasts each change as the `system:theme-changed` Tauri event, which the frontend `listen`s for. On macOS / Windows `getCurrentWindow().onThemeChanged` fires directly on the webview, so the Linux watcher compiles to a no-op there. `matchMedia` is left wired as a last-resort fallback for non-Tauri dev shells (vite-only).
- Surfaces that can't read CSS variables (CodeMirror's `dark: boolean` build-time flag, xterm.js's option-bag palette) have their own `$effect` blocks keyed on `effectiveTheme` and reconfigure when it flips.

## Keymap

Cm6 default keymap + a small layer of app-level shortcuts wired in `App.svelte`:

- `Cmd/Ctrl+S` — save active file
- `Cmd/Ctrl+P` — quick open (Phase 1)
- `Cmd/Ctrl+Shift+P` — command palette (Phase 1)

Linux uses `Ctrl`; macOS `Cmd`. We test for `navigator.platform`.
