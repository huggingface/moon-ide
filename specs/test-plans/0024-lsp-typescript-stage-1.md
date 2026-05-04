# Test plan 0024: LSP stage 1 — TypeScript diagnostics + hover + completion

- **Date**: 2026-05-03
- **Phase**: Phase 4 (LSP) — stage 1 slice

## What shipped

- End-to-end TypeScript LSP: open a `.ts` / `.tsx` / `.js` / `.jsx`
  file, see `tsgo`-backed red squigglies, hover a symbol for its
  type, press Ctrl-Space for completions.
- New `moon-core::lsp` module: framing, thin JSON-RPC client,
  per-language server actor, broker. Client surface is
  `moon_protocol::lsp` (moon-shaped types, UI never sees raw LSP).
- Status-bar pills: error/warn count for the current file; quiet
  install-hint pill when `tsgo` is missing from both the project
  and PATH.
- Architectural spec `specs/lsp.md` documents the layering and
  retires the `tower-lsp` open question.

## How to test

Prerequisites:

- `bun install` (installs `@codemirror/autocomplete` and
  `@typescript/native-preview` — which provides the `tsgo` binary
  in `node_modules/.bin/`), `bun run tauri dev`. No global
  install required for the happy path: discovery walks up from
  the active folder looking for `node_modules/.bin/tsgo` before
  falling back to `$PATH`.

### Happy path

1. Open a TypeScript project (moon-ide itself works — it has a
   `tsconfig.json` at the root). Click any `.ts` file.
2. Expected within ~3-5 seconds: a "typescript: starting…"
   pill appears in the status bar while `tsgo` boots. Pill
   disappears when it transitions to `Running`; hover the
   pill's tooltip to confirm the resolved path points at
   `node_modules/.bin/tsgo` (project-local) rather than a
   system location.
3. Write an obvious type error, e.g. `const n: number = "x";`.
   Expected: red gutter marker on the line, underline under
   the problem span, hover the marker → `tsgo`'s error
   message surfaces.
4. Status bar shows a `1 error` chip (red dot). Type
   additional warnings (e.g. `// @ts-expect-error` on a
   line with no error). Chip updates.
   Note: tsgo is a preview channel; if any specific diagnostic
   disagrees with `bun run check:ts` that's the tsgo PR
   (`microsoft/typescript-go#2302`) still landing features,
   not a moon-ide bug. File upstream and mention it in the
   commit body.
5. Hover over any declared identifier for ~300ms. Expected:
   Markdown-styled popover with the type signature. Dismiss
   with mouse-out or keyboard navigation.
6. Place the caret inside an identifier and press Ctrl-Space.
   Expected: completion popover with kind icons (function,
   class, etc.), labels, and docstrings in the side panel.
   Accept an item with Enter. The `insertText` is applied,
   replacing the prefix under the caret.
7. Save a fix for the type error. Red squigglies should clear
   within ~150-400ms (150ms debounce + tsgo turnaround).

### Project-local vs PATH discovery

1. With moon-ide as the workspace root: open any `.ts` file.
   The `Running` pill's detail (hover tooltip) should be an
   absolute path ending in
   `moon-ide/node_modules/.bin/tsgo`.
2. Open a subfolder workspace (e.g.
   `moon-ide/src/lib/components`) so the active folder is
   several levels deep. Open a `.ts` file. Discovery should
   still find the same `node_modules/.bin/tsgo` by walking
   up — verify the pill tooltip points at the repo-root
   copy, not a system install.
3. Open a TypeScript project **without** a local
   `node_modules/.bin/tsgo`. If `tsgo` is also on global
   PATH (e.g. `bun add -g @typescript/native-preview`),
   discovery should fall back to PATH — tooltip shows e.g.
   `/usr/local/bin/tsgo` or `~/.bun/install/global/...`.

### Missing-binary path

1. Move to a TypeScript project with no local install of
   `@typescript/native-preview` (i.e. no
   `node_modules/.bin/tsgo`), and ensure `tsgo` is not on
   global PATH either.
2. Open any `.ts` file.
3. Expected: status bar shows a `typescript: not available`
   pill in muted grey. Hovering it reveals the install hint
   `bun add -D @typescript/native-preview` in the tooltip.
   No red squigglies appear — the editor reads as though
   LSP is off.
4. No error toast, no panel popup, no repeat on every file open.
5. Run the install hint (`bun add -D @typescript/native-preview`)
   and restart moon-ide to re-try. (We don't auto-rediscover
   within a session by design; the restart is a one-time
   cost.)

### Lifecycle edge cases

1. Switch the active folder to a different workspace. Expected:
   `tsgo` for the previous folder is shut down (tracing shows
   `stop_all: stopped lsp broker` equivalent log via
   `reset_lsp_if_root_changed`), next TS file open spawns a
   fresh one rooted at the new folder — and re-runs discovery,
   so the new root's `node_modules/.bin/tsgo` wins if it has
   one.
2. Close the IDE. Expected: shutdown logs include
   `stop_all: stopped lsp broker` before process exit. No
   orphan `tsgo` processes on the OS (check with
   `ps aux | rg tsgo` after moon-ide quits cleanly).
3. Kill `tsgo` from the terminal (`pkill tsgo`) while
   moon-ide is running. Expected: status pill flips to
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
  (`tsgo`, like `tsserver` before it, emits hints for many
  low-impact conditions).
- ~~Hover tooltip is plain textContent~~ — superseded by test
  plan 0025 (`0025-markdown-syntax-highlighting.md`). Hovers
  now render Markdown, and fenced code blocks are syntax-
  highlighted via the editor's own grammars.

## Related

- Specs: `specs/lsp.md` (new), `specs/architecture.md`
  (open-question resolved), `specs/frontend.md` (editor
  stack), `specs/roadmap.md` (Phase 4).
- ADRs: `specs/decisions/0001-stack.md` (CodeMirror +
  Pierre Diffs baseline).
- Prior test plans: `0023-editor-qol-brackets.md`.
