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

CSS custom properties only. Default dark theme; light theme is a class on `:root`. Pierre Trees is themed via the same custom properties (it reads CSS vars from the host).

## Keymap

Cm6 default keymap + a small layer of app-level shortcuts wired in `App.svelte`:

- `Cmd/Ctrl+S` — save active file
- `Cmd/Ctrl+P` — quick open (Phase 1)
- `Cmd/Ctrl+Shift+P` — command palette (Phase 1)

Linux uses `Ctrl`; macOS `Cmd`. We test for `navigator.platform`.
