# Test plan 0024: LSP stage 1 — TypeScript diagnostics + hover + completion

- **Date**: 2026-05-03
- **Phase**: Phase 4 (LSP) — stage 1 slice

## What shipped

- End-to-end TypeScript LSP: open a `.ts` / `.tsx` / `.js` / `.jsx`
  file, see `tsserver`-backed red squigglies, hover a symbol for
  its type, press Ctrl-Space for completions.
- New `moon-core::lsp` module: framing, thin JSON-RPC client,
  per-language server actor, broker. Client surface is
  `moon_protocol::lsp` (moon-shaped types, UI never sees raw LSP).
- Status-bar pills: error/warn count for the current file; quiet
  "install typescript-language-server" pill when the binary is
  off PATH.
- Architectural spec `specs/lsp.md` documents the layering and
  retires the `tower-lsp` open question.

## How to test

Prerequisites:

- `bun install` (installs `@codemirror/autocomplete`, already
  added), `bun run tauri dev`.
- `typescript-language-server` on PATH for the "happy path":
  `bun add -g typescript-language-server typescript` (or `npm`
  equivalent). Skip this to test the missing-binary path.

### Happy path

1. Open a TypeScript project (moon-ide itself works — it has a
   `tsconfig.json` at the root). Click any `.ts` file.
2. Expected within ~3-5 seconds: a "typescript: starting…"
   pill appears in the status bar while tsserver boots. Pill
   disappears when it transitions to `Running`.
3. Write an obvious type error, e.g. `const n: number = "x";`.
   Expected: red gutter marker on the line, underline under
   the problem span, hover the marker → tsserver's error
   message surfaces.
4. Status bar shows a `1 error` chip (red dot). Type
   additional warnings (e.g. `// @ts-expect-error` on a
   line with no error). Chip updates.
5. Hover over any declared identifier for ~300ms. Expected:
   Markdown-styled popover with the type signature. Dismiss
   with mouse-out or keyboard navigation.
6. Place the caret inside an identifier and press Ctrl-Space.
   Expected: completion popover with kind icons (function,
   class, etc.), labels, and docstrings in the side panel.
   Accept an item with Enter. The `insertText` is applied,
   replacing the prefix under the caret.
7. Save a fix for the type error. Red squigglies should clear
   within ~150-400ms (150ms debounce + tsserver turnaround).

### Missing-binary path

1. Rename or temporarily remove `typescript-language-server`
   from PATH (`mv "$(which typescript-language-server)" /tmp/`).
2. Relaunch moon-ide. Open any `.ts` file.
3. Expected: status bar shows a `typescript: not available`
   pill in muted grey. Hovering it reveals the install hint
   in the tooltip. No red squigglies appear — the editor
   reads as though LSP is off.
4. No error toast, no panel popup, no repeat on every file open.
5. Restore the binary (`mv /tmp/typescript-language-server "$(dirname …)"`)
   and restart moon-ide to re-try. (We don't auto-rediscover
   within a session by design; the restart is a one-time
   cost.)

### Lifecycle edge cases

1. Switch the active folder to a different workspace. Expected:
   tsserver for the previous folder is shut down (tracing
   shows `stop_all: stopped lsp broker` equivalent log via
   `reset_lsp_if_root_changed`), next TS file open spawns a
   fresh one rooted at the new folder.
2. Close the IDE. Expected: shutdown logs include
   `stop_all: stopped lsp broker` before process exit. No
   orphan `typescript-language-server` processes on the OS
   (check with `ps aux | rg typescript-language-server`
   after moon-ide quits cleanly).
3. Kill tsserver from the terminal (`pkill typescript-language-server`)
   while moon-ide is running. Expected: status pill flips to
   `crashed` on the next file interaction. A subsequent file
   open respawns the server automatically (the broker treats
   crash + retry as one cache-miss; no manual "restart LSP"
   command).
4. Open a file type with no wired-up server (e.g. a `.md`
   file). Expected: no pill, no LSP activity. Diagnostics map
   has no entry for the path.

## What must keep working

- Editor QoL from test plan 0023: auto-close brackets,
  bracket matching, active-line highlight, selection-match
  highlight.
- `@codemirror/lint`'s keyboard navigation on markers
  (Ctrl-Shift-M to toggle the panel — we don't ship the
  panel by default but the binding is still wired).
- Editorconfig compartment, theme flip (dark ↔ light),
  tab insertion, search panel, per-file language grammar.
- Fs-watcher refresh, discard-changes, git status — all
  orthogonal to LSP.

## Known limitations

- One language server (TypeScript). Rust / Svelte / CSS /
  HTML / JSON are wire-compatible but unwired — stage 5.
- No go-to-definition, find-references, rename, code actions.
  Stage 4.
- Full-document sync on every `didChange`. Fine for normal
  buffers; revisit if someone opens a 500 KB generated file
  and tsserver starts panting.
- No signature-help popover while typing inside argument
  lists. Useful but not asked-for yet.
- Completion popover doesn't render snippet placeholders
  (we explicitly advertise `snippet_support: false`). Snippets
  round-trip as plain insertion until someone wants the
  tab-stop UI.
- Completion runs on explicit invocation only (Ctrl-Space
  or the panel's own re-query). Typing an identifier won't
  pop the list automatically — the editor's
  `activateOnTyping: false` is the source of truth for that.
- Status-bar pill counts only errors and warnings. Info
  and hint diagnostics still paint markers in the gutter
  but don't contribute to the pill — too noisy otherwise
  (tsserver emits hints for many things).
- Hover tooltip is plain `textContent`; Markdown formatting
  is preserved only as whitespace (fenced blocks look like
  plain text with `\`\`\`` fences). Swapping in markdown-it
  rendering is a one-line change when we want prettier
  tooltips.

## Related

- Specs: `specs/lsp.md` (new), `specs/architecture.md`
  (open-question resolved), `specs/frontend.md` (editor
  stack), `specs/roadmap.md` (Phase 4).
- ADRs: `specs/decisions/0001-stack.md` (CodeMirror +
  Pierre Diffs baseline).
- Prior test plans: `0023-editor-qol-brackets.md`.
