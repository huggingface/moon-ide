# Test plan 0078: oxlint as a linter co-tenant via LSP

- **Date**: 2026-05-17
- **Phase**: Phase 4 polish (LSP)

## What shipped

- `oxlint --lsp` runs as a co-tenant LSP server alongside the language server (`tsgo`) on every JS/TS file the user opens. Type errors and lint warnings show up on the same line in the editor's lint gutter without clobbering each other.
- A new `LspBinarySpec` (`OXLINT_LINTER`) plus an `OXLINT_LANGUAGES` set (`typescript` / `typescriptreact` / `javascript` / `javascriptreact`) wires it in. Discovery walks `node_modules/.bin/oxlint` first, falls back to `$PATH`. Install hint is lockfile-aware: `bun add -D oxlint` / `pnpm -wD add oxlint` / `npm i -D oxlint`.
- The broker grew a parallel `lint_servers: HashMap<String, ServerSlot>` map keyed by the linter's slot name. Diagnostics-producing operations (`open` / `update` / `close` / `notify_files_changed` / `refresh_open_diagnostics` / `shutdown_*`) fan out to both maps; hover / completion / definition / rename keep looking at `servers` only.
- `LspDiagnosticsEvent` gained a `producer: String` field stamped by the emitting `LspServer` from its own `language_id`. The frontend now stores `diagnosticsByProducer: Map<path, Map<producer, LspDiagnostic[]>>` as the source of truth and recomputes the flat `diagnostics: Map<path, LspDiagnostic[]>` union the editor reads, so a fresh oxlint report replaces only oxlint's slice.
- Status pill / log source / restart all per-slot: `lsp:status` for oxlint carries `language_id: "oxlint"`, so a missing-binary pill appears next to (not on top of) the `tsgo` pill. `Restart` per pill talks to `shutdown_language("oxlint")` independently of the language server.

Test plan: `specs/test-plans/0078-oxlint-lsp.md`. Spec changes: `specs/lsp.md` (events, frontend architecture, new "Linter co-tenants" subsection).

## How to test

Prerequisites: `bun install` (gets `oxlint` into `node_modules/.bin/`). `bun run check`, `cargo test -p moon-core` clean.

1. **Both servers come up on a TS file.** Open moon-ide, open `src/lib/editor/lsp.ts`. After tsgo finishes its first pass, the bottom-panel logs view shows two `lsp.<id>` sources active: `lsp.typescript` (tsgo) and `lsp.oxlint` (oxlint). The status bar shows no pills — both are Running. Expected: no UI pill noise, both servers logged as ready.
2. **Both producers contribute diagnostics.** Edit the file to introduce both kinds of error:
   - Add `const x: number = "hello";` (TS type error from tsgo).
   - Add `const unused = 1;` at file scope (lint warning from oxlint, `eslint(no-unused-vars)`).
     Wait for the 150ms debounce. Expected: both squiggles paint on the same file. Hover the type error → tooltip says `[ts]`. Hover the lint warning → tooltip says `[oxlint]`. Status bar problem count includes both.
3. **Per-producer clobber semantics.** With both errors visible, fix the TS one (`const x: number = 1;`). Wait. Expected: TS squiggle goes away; oxlint squiggle stays. Conversely, fix the oxlint one (`const _unused = 1;`); TS error continues to show if you reintroduce it. Neither producer's refresh removes the other's diagnostics.
4. **Linter without language server.** Open a `.js` file in a project with no `tsconfig.json`. Expected: tsgo may emit nothing useful, but oxlint still publishes lint warnings. Removing oxlint (`mv node_modules/.bin/oxlint /tmp/x`, restart) → status bar shows an `oxlint` pill with tooltip `bun add -D oxlint` (or the pnpm/npm variant). Restoring the binary and clicking Restart on the pill brings it back without restarting tsgo.
5. **Off-disk refresh.** With both servers running, modify a JS file from a terminal (`echo 'const z = 1;' > some.js`). Expected: both servers receive `workspace/didChangeWatchedFiles`, both re-publish, the editor's gutter updates without the user having to retype. Same on window-focus refresh: alt-tab away and back, both producers re-pull.
6. **Crash isolation.** Kill the oxlint child process from a terminal (`pkill -f 'oxlint --lsp'`). Expected: oxlint's status pill flips to Crashed with a tooltip; tsgo keeps running unaffected; oxlint's last-known diagnostics linger until the next open/edit re-spawns it. Click the oxlint pill → "Restart" → fresh oxlint spawns and re-publishes.
7. **Container path** (only if running with a workspace container). The broker tries oxlint inside the container first, then host fallback. Expected: behaves like `tsgo` — if `node_modules/.bin/oxlint` lives inside the bind mount, the container path resolves; if hoisted above, falls back to host.

## What must keep working

- TS-only diagnostics: removing `oxlint` from `node_modules` shouldn't affect tsgo at all. The status bar shows the oxlint "not available" pill but TS type errors still surface as before.
- Hover / completion / goto-definition / F2 rename still go through `tsgo` exclusively. None of them consult `lint_servers`.
- Diagnostics from a single producer (`tsgo` only on a `.ts` file with oxlint missing) still render correctly — the per-producer refactor doesn't introduce a producer-multiplexing bug for the single-producer path.
- Full-replacement semantics within a producer: tsgo emitting an empty diagnostic list for a file (server says clean) clears all tsgo diagnostics from that file's gutter; oxlint's are unaffected.

## Known limitations

- **No oxlint config required.** oxlint runs with its built-in defaults when `.oxlintrc.json` is absent. We don't add an extra check that warns the user "no config found" — oxlint's defaults are sensible and the team can opt in to a config when they want stricter rules.
- **Config-aware spawn.** We don't watch `.oxlintrc.json` for changes ourselves; oxlint's LSP layer registers its own `workspace/didChangeWatchedFiles` glob and reacts to edits the IDE forwards through `lsp_notify_files_changed` like every other server.
- **Restart of oxlint doesn't auto-re-pull other servers.** Each producer's slice is independent — if oxlint goes down and comes back up, oxlint re-publishes for open files but tsgo isn't poked. That's intentional (no cross-talk between co-tenants); the focus-event refresh path covers the cold-start case.
- **Other linters (`biome`, `eslint`, …).** Not wired. The pattern is documented in `specs/lsp.md#linter-co-tenants` for when there's a real ask; we don't preemptively add empty slots for tools nobody on the team uses.

## Related

- Specs: `specs/lsp.md` (Events, Frontend architecture, Linter co-tenants), `crates/moon-protocol/src/lsp.rs` (`LspDiagnosticsEvent.producer`).
- Code: `crates/moon-core/src/lsp/server.rs` (`OXLINT_LINTER`, `OXLINT_LANGUAGES`, `resolve_install_hint`), `crates/moon-core/src/lsp/broker.rs` (`lint_servers`, `lint_spec_for`, `slot_map_for`), `src/lib/state.svelte.ts` (`diagnosticsByProducer` + flat union recompute).
- Prior test plans: 0024 (TS LSP stage 1), 0049 (Python LSP), 0030 (Rust LSP), 0069 (diag logs panel).
