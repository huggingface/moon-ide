# Test plan 0075: F2 rename via LSP

- **Date**: 2026-05-15
- **Phase**: Phase 4 (LSP) — feature follow-up to goto-definition

## What shipped

- F2 on an identifier opens a docked panel at the top of the editor, prefilled with the existing name. Enter applies a server-computed rename across every reference; Escape (or the Cancel button) dismisses without writing.
- The applier handles two file populations side-by-side: open buffers update through `workspace.updateText` and stay dirty (Ctrl+S to commit); closed files are read → modified → written via the active folder's `WorkspaceHost`, followed by `lsp_notify_files_changed` so the server can resync.
- Backend additions: `textDocument/prepareRename` + `textDocument/rename` wired through `LspServer` / `LspBroker` / Tauri (`lsp_prepare_rename`, `lsp_rename`), client capability `rename: { prepareSupport: true }`.
- Protocol additions: `LspTextEdit`, `LspDocumentEdit`, `LspWorkspaceEdit`, `LspPrepareRename`. The translator flattens both `WorkspaceEdit.changes` and `WorkspaceEdit.documentChanges` and drops cross-folder URIs (not supported today).
- Documentation: `specs/lsp.md` gains an F2 rename section under goto-definition; capability list updated.

## How to test

Prerequisites: `bun run tauri dev`, a bound folder with a TS/Rust/Python/Go project the user has open buffers in. The capfi-international workspace (TypeScript) and moon-ide itself (Rust + TypeScript) are both good fits.

### Happy path — same-file rename

1. Open `src/lib/editor/lspRename.ts` or a similar TS file with a private helper used 2–3 times in the same module.
2. Park the caret on the helper's name. Press **F2**.
3. The docked panel appears at the top of the editor with the input prefilled and selected ("Rename '\<name\>' to:"). The input is focused — start typing to overwrite.
4. Type a new name, press **Enter**.
5. Expected: the panel closes, a status flash reports `Renamed 'old' → 'new' in 1 file (unsaved — Ctrl+S to commit)`, and every reference in the open buffer is rewritten. The tab is marked dirty.
6. Press **Ctrl+S**. Diagnostics refresh; nothing red.

### Cross-file rename — mix of open + closed files

1. Open one consumer of a symbol that's imported from another module (e.g. a function exported from one file and consumed from two others). Don't open the other consumers.
2. F2 on the export's name. Type a new name. Enter.
3. Expected status flash: `Renamed 'old' → 'new' in N files (unsaved — Ctrl+S to commit)` where N counts both the open buffer and the closed consumers.
4. The open buffer is dirty. Open one of the closed consumers (FileTree → click). Its on-disk bytes already carry the rename — no dirty indicator, no panel artifacts, no extra newlines.
5. Hover an identifier in the rewritten consumer → LSP still answers; diagnostics for the file are clean. (If the server is push-mode, the `lsp_notify_files_changed` fanout invalidated the cache; if it's pull-mode, the next `didChange` after focus picks up the same.)

### Server declines: cursor on a non-identifier

1. Park the caret on whitespace, a keyword, or inside a string literal. Press F2.
2. Expected: a quiet flash like `Rename: not a renameable symbol`. Panel does **not** open.

### User cancels

1. F2 on a renameable symbol. Panel opens.
2. Press **Escape**. Panel closes, no edits, focus returns to the editor at the original caret position.
3. F2 again, then click anywhere in the editor (this typing or click triggers a doc-change transaction). Panel auto-closes because doc-change closes the rename state.

### Pre-existing dirty state

1. Open a file, type a few characters (don't save).
2. F2 on a symbol used elsewhere in the same buffer. Type a new name. Enter.
3. Expected: rename applies on top of the existing dirty state. Ctrl+S saves both your manual edits and the rename in one go.

### Empty-name / no-op

1. F2, clear the input, Enter. Panel closes, no flash, no edits.
2. F2, leave the placeholder unchanged, Enter. Same — closes cleanly, no IPC call, no flash.

### Rust / Python / Go

Repeat the same-file and cross-file cases for one Rust, Python, and Go project (rust-analyzer, basedpyright, gopls all advertise rename). Expected: same UX. Some servers report `null` from `prepareRename` while still implementing `rename` (we gate on a cheap "looks like an identifier" check before bailing, so a bare ASCII word still attempts the full rename request even on a no-prepare server).

### Container-routed workspace

1. With a container workspace (TS or Python) up and `Running`, repeat the cross-file test. The LSP runs inside the container; rename results land back as host-relative paths because the translator strips against the container's mount root.
2. Closed-file writes route through `WorkspaceHost::save_file` (= writeFile in the container). FileTree's row decoration updates from the SCM signal within ~500ms.

## What must keep working

- **Goto definition / hover / completion** keep behaving exactly as before. The new keymap entry sits at `Prec.high` on F2 only; nothing else changed in the editor extension stack.
- **Closed-file format-on-save**: the host's `save_file` runs the existing editorconfig pre-save and lint-staged formatter pipeline. Renaming a closed file produces the same bytes a manual Ctrl+S of that file would.
- **Watched-files refresh**: after `lsp_notify_files_changed`, the diag-logs panel shows the per-server `workspace/didChangeWatchedFiles` notification at debug level (see [test plan 0066's diagnostics flow](0066-python-format-on-save.md)).
- **No global try/catch**: every command path returns errors via `MoonError`; the frontend uses targeted `try` around the IPC call and surfaces a `flash` rather than crashing the editor.

## Known limitations

- **Cross-folder rename**: a rename whose results touch files in a sibling bound folder silently drops the cross-folder edits at the translation boundary. Lands when we grow the multi-bound-folder LSP path.
- **No undo grouping**: a rename touching 12 files yields 12 separate `updateText` transactions on the open buffers' CM histories. `Ctrl+Z` after a rename undoes one file's worth at a time, per the active buffer. A future improvement is bundling the whole `LspWorkspaceEdit` into one annotated CM transaction group, but that requires reaching into each CM view and is deferred until anyone misses it.
- **No preview surface**: VSCode's "Refactor Preview" panel isn't replicated. Server-computed renames are applied directly. The dirty-buffer review path (tab strip → SCM panel → `Ctrl+S` per file) plus `Ctrl+Z` covers the common "wait, no" case.
- **Resource operations** (file create / move / delete inside a `WorkspaceEdit`) are dropped. A pure identifier rename never asks for them; servers that do (e.g. tsserver renaming a file when its default export changes) get their text edits applied and their resource ops silently ignored.

## Related

- [specs/lsp.md § F2 rename](../lsp.md#f2-rename) — design.
- Backend: `crates/moon-core/src/lsp/server.rs` (`prepare_rename` / `rename`), `crates/moon-core/src/lsp/translate.rs` (`workspace_edit` / `prepare_rename_response`), `src-tauri/src/commands/lsp.rs` (`lsp_prepare_rename` / `lsp_rename`).
- Frontend: `src/lib/editor/lspRename.ts` (extension + applier), `src/lib/editor/lspRename.test.ts` (offset-math unit tests).
- Predecessor: [test plan 0027 — goto-definition + nav history](0027-lsp-goto-definition-nav-history.md) (the F2 surface piggybacks on the same broker + capability declaration patterns).
