# Test plan 0047: Format on save via lint-staged

- **Date**: 2026-05-05
- **Phase**: Phase 8 (Lint / format) — early bootstrap slice

## What shipped

- **`WorkspaceHost::save_file` is the new write seam.** Every editor save (`fs_write_file`) and every coder/agent edit (`write_file` / `edit_file` tools) goes through it. Raw `WorkspaceHost::write_file` keeps its meaning (bytes-to-disk, tests still call it directly). `save_file` runs the editorconfig pre-save pipeline (line endings → trim trailing ws → final newline) and then the new `RunFormatter` step that lint-staged drives.
- **`.lintstagedrc.json` (or `package.json#lint-staged`) drives the formatter.** A new `LintStagedService` walks from the file's directory up to the workspace root looking for either config; closest wins, no merge across levels. Per-directory cache; cleared inside `LocalHost::write_file` whenever a `.lintstagedrc.json` or `package.json` is saved (and the existing trash / delete / git-restore clears get a sibling `lint_staged.clear()` for symmetry).
- **Stdin/stdout invocation per known tool.** `crates/moon-core/src/format.rs` parses the lint-staged command, looks up the binary in a `KnownTool` table (`oxfmt`, `prettier`, `rustfmt`), strips file-mutation flags (`--write`, `--check`, `--list-different`), and appends the tool's stdin-mode argv (`--stdin-filepath=<abs>` for oxfmt / `--stdin-filepath <abs>` for prettier / `--emit stdout` for rustfmt). Project-local binaries (`node_modules/.bin/<name>`) win over `$PATH` for the node tools.
- **Failures never abort the save.** Missing binary, non-zero exit, 5s timeout, or non-UTF-8 stdout all fall back to the editorconfig-normalised text. `tracing::warn!` once on each, with `unsupported tool` and `tool not found` deduped per-process.
- **JSON only.** `.lintstagedrc.js` / `.cjs` / `.mjs` / `.yaml` / `.yml` and the no-extension `.lintstagedrc` log a one-shot warning per directory and the walk continues. The team's lint-staged map is JSON; the JS variants would need an embedded JS runtime, the YAML variants need a parser nobody's asked for.
- New ADR: [ADR 0012 — Format on save via lint-staged](../decisions/0012-format-on-save.md), superseding the third bullet of [ADR 0006 § Consequences](../decisions/0006-no-settings-file.md#consequences). [`editorconfig.md`](../editorconfig.md) and [`roadmap.md`](../roadmap.md) updated to point at the new shape.

## How to test

Prerequisites: `bun install`, `cargo build`, `bun run tauri dev`. Open the moon-ide repo on itself.

### Happy paths — one per tool in `.lintstagedrc.json`

1. **oxfmt for TS.** Open `src/lib/state.svelte.ts`. Add a stray space at end of any line and a blank line at the very top. Hit **Ctrl+S**.
   - The buffer re-reads from disk after save and now matches `bun run fmt:js` output: stray space gone, blank line gone, every other byte unchanged.
   - The tab title's dirty dot clears immediately (re-read fingerprint matches the new disk content).
2. **prettier for Svelte.** Open `src/App.svelte`. Mis-indent a block (insert a leading tab on a closing `</div>`). Ctrl+S → prettier reformats via `--stdin-filepath`.
3. **rustfmt with edition.** Open `crates/moon-core/src/host.rs`. Mangle the indentation on a `match` arm (extra tab in front of an arm body). Ctrl+S → rustfmt reformats. The lint-staged command is `rustfmt --edition 2021`; the spawn is `rustfmt --emit stdout --edition 2021` and the edition flag survived.
4. **Idempotency.** Re-save any of the three above immediately after step 1/2/3. The file is already formatted, so `save_file` is a no-op on disk, and no extraneous "modified" decoration appears in the tree.

### Lint-staged config edits take effect on the next save

5. Edit `.lintstagedrc.json` itself: add a comment-style key like `"*.fake": "oxfmt"` (any change suffices). Ctrl+S the `.lintstagedrc.json` (it gets formatted by oxfmt because `*.json` matches it — that's expected).
6. Open another `.ts` file with a stray space. Ctrl+S — still formatted by oxfmt (cache was cleared on the `.lintstagedrc.json` save, so the lookup re-resolved from disk and saw the new map).
7. Revert the `.lintstagedrc.json` change, save again. Same behaviour.

### Missing config: editorconfig path stays clean

8. Rename `.lintstagedrc.json` aside (`mv .lintstagedrc.json /tmp/lsr.bak`). Now neither `.lintstagedrc.json` nor any `package.json#lint-staged` field governs the file.
9. Edit any `.ts` file (add stray trailing whitespace + missing final newline). Ctrl+S.
   - Editorconfig pipeline still runs — trailing whitespace stripped, final newline added.
   - No formatter runs (no rule found). Save succeeds.
   - Restore: `mv /tmp/lsr.bak .lintstagedrc.json`. Next save formats again.

### Coder/agent writes funnel through `save_file`

10. With the Coder panel open and a `.ts` file in the workspace, prompt the agent to write a deliberately ugly file: `Create src/scratch_unformatted.ts with content 'export const   x =1' on one line, no trailing newline.`
11. The file the agent creates **is formatted** when it lands on disk (oxfmt fixed the spacing and added the final newline). The agent's tool result reports `bytes_written` matching the content it sent — that's fine, the post-format bytes go to disk.
12. Delete the scratch file when done.

### Spawn failures don't fail the save

13. Quit `bun run tauri dev`. Temporarily move the project-local `oxfmt` aside: `mv node_modules/.bin/oxfmt node_modules/.bin/oxfmt.bak`.
14. Restart `bun run tauri dev`. Edit a `.ts` file and Ctrl+S.

- Save succeeds.
- File on disk has the editorconfig-normalised text (trailing ws gone, final newline ensured) but is otherwise unformatted.
- The dev terminal shows exactly **one** `format-on-save: tool not found in node_modules/.bin or $PATH; skipping` warning for `oxfmt`. Subsequent saves of `.ts` / `.json` / `.md` files do not re-warn (per-process dedup).
- Restore: `mv node_modules/.bin/oxfmt.bak node_modules/.bin/oxfmt`.

15. Introduce a syntax error in a `.svelte` file (`<script>let x = </script>`). Ctrl+S.

- Save succeeds; bytes on disk are the user's broken text (editorconfig still ran).
- Dev terminal shows a `format-on-save: tool exited with error` warning with prettier's stderr trimmed.
- Fix the syntax, save again — formatter runs cleanly.

### Pipeline ordering

16. Take a `.ts` file with `\r\n` line endings (force it: `unix2dos` on a temp copy, then open). With `.editorconfig`'s `end_of_line = lf` for the repo, save the file. The file ends up `\n`-only on disk and oxfmt-formatted — meaning the editorconfig step ran first (so the formatter saw `\n` input) and the formatter ran second.
17. Confirm `cargo test -p moon-core lint_staged` and `cargo test -p moon-core format::tests` are both green: 9 + 7 unit tests covering basename matching, separator-anchored matching, package.json-vs-lintstagedrc precedence, brace expansion, glob malformations, cache invalidation, command parsing, and the per-tool argv translation.

## What must keep working

- **All editorconfig behaviour from test plans 0002 and 0037.** The `.editorconfig` cache invalidation, the line-ending / trim-trailing-ws / final-newline transforms, and the post-save re-read flow are unchanged. The new `RunFormatter` rung sits _after_ the existing transforms.
- **Coder write/edit tools (test plans 0040, 0044, 0046).** `write_file` and `edit_file` now route through `host.save_file` instead of `host.write_file`; the JSON tool result fields (`path`, `bytes_written`, `mtime_ms`, etc.) are unchanged. Plans 0040+ should still pass; the only observable difference is that the on-disk content is formatted.
- **Tab dirty-state coherence.** The `saveActive` flow re-reads the file post-save and recomputes the fingerprint; saving an already-formatted buffer doesn't leave the tab in a dirty state.
- **`bun run check`, `bun run lint`, `cargo check --workspace --exclude moon-desktop`, `cargo clippy --workspace --exclude moon-desktop --all-targets -- -D warnings`** all clean.
- **Direct `host.write_file` callers in tests** (`crates/moon-core/src/host.rs` integration tests around line 1267) keep their raw-write semantics. The pre-save / format pipeline is on `save_file` only.

## Known limitations

- **No fs watcher for external `.lintstagedrc.json` edits.** Same story as `.editorconfig`: an edit made outside moon-ide (`git pull`, another editor) waits for a moon-ide-issued save to invalidate the cache, or a restart. Fixed when the watcher arrives in Phase 5.
- **First-tool-only for chains.** A `*.ts: ["eslint --fix", "prettier --write"]` chain runs only `eslint --fix` and emits a `tracing::warn!` once. The team's current map has no chains.
- **Unknown tools log + skip.** `eslint`, `biome`, etc. don't run. Adding them is "extend the `KnownTool` table" — when the team needs it.
- **No "format on save" toggle.** Hardcoded on. Per AGENTS.md "hardcode first, configure later"; add a knob when there's a real request.
- **JS / YAML lint-staged variants log + ignore.** Anyone using one will hit the warning and either switch to JSON or extend the loader.
- **Saves are not async-cancelled.** A 5s formatter timeout caps the worst case but Ctrl+S does still wait for the formatter; on a healthy install this is sub-100ms per save and unmeasurable in practice.
- **RemoteHost (Phase 2) not yet exercised.** The seam works the same — `host.save_file` runs inside the container's `LocalHost` instance — but verifying that requires Phase 2 to be live. Smoke when it lands.

## Related

- ADRs: [0006 — no settings file](../decisions/0006-no-settings-file.md) (this plan supersedes the `format_on_save` paragraph), [0012 — format on save via lint-staged](../decisions/0012-format-on-save.md).
- Specs: [editorconfig.md](../editorconfig.md), [roadmap.md § Phase 8](../roadmap.md#phase-8--lint--format).
- Prior plans: [0002 — editorconfig](0002-editorconfig.md), [0037 — revert icon and utf8 save](0037-revert-icon-and-utf8-save.md), [0040 — coder write tools](0040-coder-write-tools.md).
