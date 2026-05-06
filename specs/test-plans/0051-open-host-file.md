# Test plan 0051: Open File… (Ctrl+O) for in-folder and host-external files

- **Date**: 2026-05-06
- **Phase**: 1.5 (editor polish) — small UX add, but crosses the IPC boundary so it gets its own plan.

## What shipped

- `Ctrl+O` (and palette `Open File…`) now opens the native file picker. Picking a file inside the active folder routes through the regular `openFile` flow (LSP, editorconfig, git, persistence). Picking a file anywhere else opens it as an `isExternal` buffer that reads/writes via host-direct IPC and skips per-folder wiring.
- New Tauri commands `fs_read_file_host` / `fs_write_file_host` and free functions `moon_core::read_host_file` / `write_host_file` that bypass every `WorkspaceHost` and use `tokio::fs` directly. Phase 2's container `WorkspaceHost` would otherwise refuse paths outside its bind mount; this pair keeps "open a host file" honest under either host.
- New `OpenFile.isExternal` flag drives the per-buffer skips: no LSP `didOpen` / `didChange` / `didClose`, no editorconfig fetch, no git blame / HEAD seed / `refreshGitStatus` reload, and no inclusion in the persisted session.
- `Ctrl+O` requires an active folder (the open-files list is per-folder); without one the user sees the same "open a folder first" toast `Ctrl+N` already uses.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a folder open as the active workspace, plus a couple of files outside that folder on the host (e.g. `~/.bashrc`, a sibling repo's `README.md`).

### Inside the active folder

1. Active folder = a real repo (e.g. moon-ide). Press `Ctrl+O`. In the picker, navigate to a file **inside** the active folder (`src/main.ts`).
2. Expected: the file opens like a regular tab. Confirm by:
   - LSP diagnostics appear (squigglies for an intentional typo).
   - `Ctrl+S` runs the format-on-save pipeline (oxfmt / prettier touch the buffer if applicable).
   - Editor gutter shows git-change markers if the file has working-tree edits.
   - Quitting and reopening the IDE restores the tab.

### Outside every bound folder

3. Press `Ctrl+O`. Navigate to a file **outside** the active folder (e.g. `~/.bashrc`).
4. Expected: the file opens in a new tab; the tab label is the basename, the buffer renders the file's text.
5. With the external buffer focused:
   - No LSP squigglies should appear (the buffer never registered with the broker).
   - The git-change gutter / inline blame stays empty (external file isn't in any tracked repo from this folder's POV).
   - The editor uses default editorconfig (tabs, indent 4 — whatever `defaultEditorConfig` is) regardless of any `.editorconfig` near the external file.
6. Edit the buffer, press `Ctrl+S`. Expected: bytes land at the absolute host path (verify with `cat` in a host terminal). The dirty marker clears. No format-on-save / lint-staged side effects.
7. Quit moon-ide, reopen. Expected: the external tab does **not** come back; only files inside bound folders persist. Re-press `Ctrl+O` to reopen.
8. Close the external tab while at least one regular tab is also open. Expected: tab disappears, no `lspClose` errors in the dev console.

### No-folder guard

9. Remove every folder from the workspace so the welcome screen is showing. Press `Ctrl+O`.
10. Expected: a toast "Open a folder before opening a file." No file picker.

### Container / Phase 2 sanity (when the container path lights up)

11. With the active folder running in a container (Phase 2), press `Ctrl+O` and pick a file outside the bind mount (e.g. `~/.bashrc` on the host).
12. Expected: the file opens. The read goes through `fs_read_file_host`, which uses `tokio::fs` directly on the host — no `docker exec` round-trip, no path translation. Save also lands on the host's filesystem.

### Binary refusal

13. Press `Ctrl+O` and pick an image / PDF / other binary file.
14. Expected: a flash toast "Cannot open binary file: …". No tab opens.

## What must keep working

- `Ctrl+N`, `Ctrl+S`, `Ctrl+W`, `Ctrl+P`, `Ctrl+Shift+F`, `Ctrl+0`, `Ctrl+L`, `Ctrl+J`, `Ctrl+\` — every other shortcut wired in `App.svelte` keeps its previous behaviour.
- Session restore for in-folder files (the existing per-folder persistence path was only narrowed to skip external buffers — every other tab still restores).
- LSP open / close lifecycle for in-folder buffers (the `wasExternal` guard in `closeFile` only skips LSP teardown when the buffer was external; regular files still call `lspClose`).
- Format-on-save pipeline (`save_file` with editorconfig + lint-staged) for in-folder saves.
- `fs_read_file` / `fs_write_file` continue to enforce the workspace-root boundary — only the new `_host` pair bypasses it.

## Known limitations

- External buffers are intentionally transient: not persisted across restarts. The per-folder session model has nowhere clean to put them, and re-typing `Ctrl+O` is cheap.
- No "external" badge or italic label on the tab. Add later if anyone confuses an external tab for an in-folder one.
- Binary / image external files refuse with a toast — no read-only image preview for host paths yet (the existing `loadImageFile` path requires `fs.absolutePath` which itself goes through the active host).
- `Save As…` on an external buffer still routes through `saveActiveAs`, which insists the target lands inside the active folder. Saving an external file to a brand-new external location isn't wired.
- `oxlint` / `tsgo` / `svelte-check` aren't run on external buffers — the tools are scoped to the active folder.

## Related

- Specs: [architecture.md](../architecture.md) — the `WorkspaceHost` invariant and why the host-direct pair is a deliberate exception.
- Prior test plans: [0001-skeleton.md](0001-skeleton.md) (the original `fs_read_file` + `fs_write_file` flow we're paralleling).
