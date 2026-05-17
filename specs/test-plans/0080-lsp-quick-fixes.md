# Test plan 0080: LSP quick-fixes in the lint tooltip

- **Date**: 2026-05-17
- **Phase**: Phase 4 polish (LSP)

## What shipped

- The CodeMirror lint tooltip (the popup that shows when the cursor sits on a squiggle) now renders quick-fix buttons. Two sources feed the list:
  - **LSP-provided quickfixes** — `textDocument/codeAction` with `only: [quickfix]`, asked of the producer that emitted the diagnostic. For oxlint that surfaces three buttons per warning: an autofix when the rule is fixable, plus "Disable `<rule>` for this line" and "Disable `<rule>` for this whole file". `tsgo` adds its own autofix set.
  - **"Fix in coder"** — always present, no IPC. Opens the coder panel, attaches the diagnostic's range as a selection chip, and seeds the composer draft with `Fix [source code]: <message> @path:line` for the user to edit-and-send.
- New `LspCodeAction` protocol type (`title`, `kind`, `edit: LspWorkspaceEdit`, `isPreferred`, `producer`). Pure-`Command` actions and actions whose edits all target out-of-workspace URIs are dropped on the broker side so the tooltip never shows a clickable-but-silent entry.
- New `lsp_code_action(path, producer, range, diagnostic)` Tauri command. The broker routes per-`producer` rather than fanning out to every server — co-tenants don't know about each other's diagnostics, and asking the wrong one yields source-action noise.
- `applyDiagnostics(view, perProducer)` two-phase apply: synchronous dispatch with whatever quickfixes are cached, plus a background prefetch (one IPC per diagnostic in parallel) that re-dispatches once the actions land. Generation counter discards stale prefetches when the user swaps files mid-fetch.
- Shared applier `applyWorkspaceEdit` extracted from rename — quickfix buttons, `Fix in coder`, and F2 rename all dispatch through the same open-buffer/disk-write path, with `lsp.notifyFilesChanged` on closed-file edits so other servers re-pull.

Test plan: `specs/test-plans/0080-lsp-quick-fixes.md`. Spec changes: `specs/lsp.md` (`LspCodeAction` in the wire surface intro, new "Diagnostic quick-fixes" subsection under Frontend architecture).

## How to test

Prerequisites: `bun install` (oxlint in `node_modules/.bin/`), `bun run check` and `cargo test -p moon-core` clean. Open `~/code/boardgamers-mono` (the same monorepo we use for the oxlint co-tenant test plan) so there are real lint warnings to work against.

1. **Tooltip renders LSP quickfixes for an oxlint warning.** Open `apps/api/app/ws.ts`. Park the cursor on the line `const data = JSON.parse(String(message));` (the `String(message)` should be squiggled by `typescript-eslint(no-base-to-string)`). The tooltip should show three buttons in order: nothing autofixable here so just the two disable-rule entries plus our "Fix in coder". Expected: each button has visible borders, our accent palette on hover, and clicking either disable button inserts the comment at the right place. Undo (Ctrl+Z) reverts cleanly.
2. **Autofix surfaces when the rule is fixable.** Add a line `if (true) { console.log('hi'); }` to a file with `eslint/no-constant-condition`. Park the cursor on `true`. Expected: the tooltip shows oxlint's `Disable …` entries, plus "Fix in coder". (oxlint's `no-constant-condition` rule isn't autofixable in 1.47, but if you switch to a rule that is — e.g. `no-extra-semi` — the autofix entry should appear and applying it removes the offending characters.) Verify the cache invalidates: type into the file, wait for the new oxlint publish, the tooltip on the new diagnostic shows fresh actions (not the previous file's).
3. **Tooltip renders for a tsgo diagnostic too.** Add `const x: number = "hello";` to a TS file. Park on the type error. Expected: the tooltip shows tsgo's quickfixes (typically "Add type annotation" / "Convert to …" depending on tsgo version) plus "Fix in coder". Each button is labelled with the producer's title verbatim — no Moon-side rewriting.
4. **"Fix in coder" composes a sensible prompt.** Click "Fix in coder" on any diagnostic. Expected:
   - Right panel slides to the coder view.
   - The composer chip strip gets a new selection chip for the diagnostic's range, hovering the chip shows the file/line snippet.
   - The textarea is prefilled with `Fix [oxc typescript-eslint(no-base-to-string)]: <message first line> @apps/api/app/ws.ts:59` (or your file's range).
   - Cursor lands in the textarea, ready to edit before send.
   - Doing it twice on the same range adds the chip once but the prefill text appears twice (the second call prepends; the user can edit). Doing it on a different range adds a second chip.
5. **Apply through the closed-file path.** Trigger an LSP quickfix whose `documentEdits` targets a file that's NOT currently open in any tab. (Tsgo's "Add missing import" sometimes does this; alternatively, edit the broker behaviour temporarily.) Expected: the file gets rewritten on disk, the SCM panel registers the change, and any open server that watches that path re-publishes diagnostics within ~150 ms.
6. **Failure surfaces a flash, doesn't crash.** Apply a quickfix while the LSP server is mid-restart (kill the oxlint child via `pkill -f 'oxlint --lsp'`, then quickly click a button). Expected: a toast `Quick fix failed: …` (or `Quick fix: nothing to apply for "…"` if the edit was empty); the editor stays interactive; subsequent applies after the server respawns work normally.
7. **Concurrency: file swap mid-prefetch.** Open file A with many diagnostics. Within ~300 ms, swap to file B. Expected: B's diagnostics paint with their own actions; no actions from A leak into B's tooltips. (Inspect the `editor.diagnostics` log source for "code-action prefetch failed" entries — none should appear under normal operation.)
8. **Tooltip styling is theme-aware.** Toggle between light and dark themes. Expected: the buttons read clearly in both, hover state visibly distinct, no clipped borders.

## What must keep working

- F2 rename still applies through the same shared `applyWorkspaceEdit` helper. Renaming an identifier across multiple files behaves identically to before this change (touched-files flash, dirty/clean status, undo per buffer).
- Per-producer diagnostic clobber semantics: a quickfix that triggers a fresh oxlint publish doesn't accidentally clear tsgo's diagnostics, and vice versa.
- The lint gutter and status-bar problem count are unchanged in behaviour. Quickfixes are additive UI; the squiggles themselves come from `setDiagnostics` and update on every `applyDiagnostics` call as before.
- A diagnostic with zero LSP quickfixes still gets the "Fix in coder" entry — never an empty action list.

## Known limitations

- **No `Show all code actions` keybinding.** We only request `quickfix`-kind actions today, not `refactor.*` / `source.*`. A future Ctrl+. keybinding could open a wider menu; the wire format already carries `kind` so the UI side is the only piece left.
- **No `workspace/executeCommand`.** Pure-`Command` code actions (oxlint's `oxc.fixAll`, tsgo's "Organize imports") are dropped on the broker rather than rendered as no-op buttons. Wiring those needs an `lsp_execute_command` IPC and a per-server allow-list — out of scope for this change.
- **No bulk "Disable all this rule"** that walks the project. The "Disable `<rule>` for this whole file" action covers the per-file case; project-wide disabling is what `.oxlintrc.json` is for.
- **Cache lifetime.** The per-(path, diagnostic-key) cache lives for as long as the workspace tab is open. New publishes naturally produce new keys (positions shift), so stale entries become unreachable rather than stale. We don't actively GC them — at typical scales (~50 diagnostics × ~10 LSP actions × tens of files) the memory footprint is tiny, but a long-running session with thousands of edits will accumulate; not a leak in any practical sense.

## Related

- Specs: `specs/lsp.md` (Diagnostic quick-fixes section, wire surface intro), `crates/moon-protocol/src/lsp.rs` (`LspCodeAction`).
- Code: `crates/moon-core/src/lsp/translate.rs` (`code_actions`, `to_lsp_diagnostic`), `crates/moon-core/src/lsp/server.rs` (`LspServer::code_action`), `crates/moon-core/src/lsp/broker.rs` (`code_action`, `spec_for_producer`), `src-tauri/src/commands/lsp.rs` (`lsp_code_action`), `src/lib/editor/lsp.ts` (prefetch + cache + tooltip wiring), `src/lib/editor/lspWorkspaceEdit.ts` (shared applier), `src/lib/coder.svelte.ts` (`fixDiagnosticInCoder`).
- Prior test plans: 0078 (oxlint as a linter co-tenant), 0075 (F2 LSP rename — same `applyWorkspaceEdit` plumbing).
