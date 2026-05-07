# Test plan 0063: Format on save — file-based lint-staged invocation

- **Date**: 2026-05-07
- **Phase**: Phase 1.5 / Phase 8 bootstrap slice — supersedes the stdin/stdout pipeline shipped in [test plan 0047](0047-format-on-save.md).

## What shipped

- **`format::run_formatter` rewritten end-to-end.** The function used to parse a lint-staged command, look the binary up in a `KnownTool` allow-list (`oxfmt` / `prettier` / `rustfmt`), strip mode flags (`--write` / `-w` / `--check` / `-l` / `--list-different`), and translate the invocation to its stdin-mode equivalent (`--stdin-filepath=<abs>` / `--stdin-filepath <abs>` / `--emit stdout`). It now spawns the user's binary with the user's args verbatim, appends the absolute file path as the last positional argument, and lets the tool mutate the file in place — same shape `bun run lint-staged` uses on commit.
- **No allow-list.** Anything the team writes in `.lintstagedrc.json` runs: `eslint --fix`, `biome format --write`, `node scripts/lint.ts --fix`, `python -m black --quiet`, an in-repo shell script, anything. The old `format-on-save: unsupported tool; skipping` warning is unreachable and gone with the allow-list.
- **`PATH` is enriched, not searched manually.** Every `node_modules/.bin/` from `config_dir` up to `workspace_root` is prepended to the inherited `PATH` for the spawned subprocess. `prettier` / `eslint` / `oxfmt` resolve to project-installed copies; `node` / `bun` / `rustfmt` fall through to the system path. `npm-run-path` semantics, no per-tool flag.
- **Chain-truncation caveat (temporary).** `LintStagedRules::match_command` (singular, returned the first command of the first matching glob) became `match_commands` (plural, returns the whole chain). The intended end state is "run every command in the chain". The current implementation in `LocalHost::run_formatter_chain` ships a deliberately-narrowed version: **only the last command in the chain runs** for chains of length > 1. This keeps `moon-landing/server`'s `[node scripts/lint.ts --fix, prettier -w]` chain workable on every save (the slow `node` step is skipped, `prettier -w` runs). A deduped `format-on-save: lint-staged chain truncated to last command` warning fires once per process per chain length so the deviation is visible. The TODO + trigger condition are documented in `LocalHost::run_formatter_chain` and ADR 0013 — flip back to "run the whole chain" when moon-landing's lint script is fast enough to be save-time-friendly. Single-command rules are unaffected. **When the flip happens** the chain will run all commands regardless of failures (a deliberate divergence from `bun run lint-staged`'s commit-time abort-on-first-failure: format-on-save is best-effort, and a slow / broken `eslint --fix` shouldn't block `prettier -w` from doing its job).
- **`save_file` is a two-stage pipeline now.** Stage 1: editorconfig pre-save transforms (line endings → trim trailing ws → final newline) on the in-memory text, then a single write to disk. Stage 2: every command in the lint-staged chain runs against that on-disk file. Stage 1 always runs (it gives the formatter coherent bytes to read and is the entire pipeline for files without a lint-staged rule). Stage 2 only runs when there's a matching rule; on success the file is re-stat'd so the response carries the post-format mtime / size.
- **`prettier -w`, `eslint --fix`, etc. are no longer rewritten.** The whole point of the rewriting was to coerce a stdin/stdout pipeline; with file-based invocation, those flags do exactly what they advertise (mutate the file) and that's exactly what we want. A user-written `prettier --check` would honestly be a no-op on save — same as on commit — and the user's intent ("check don't write") is honoured.
- **Failures still never abort the save.** Missing binary, non-zero exit, 5s timeout, or spawn error all collapse to a `tracing::warn!` and the chain bails. The editorconfig pass already wrote bytes to disk, so the file is at minimum normalised. Per-process dedup on `format-on-save: tool not found` keeps the log clean across repeated saves with a missing tool.
- **New ADR**: [ADR 0013 — Format on save: file-based lint-staged invocation](../decisions/0013-format-on-save-file-based.md), supersedes the stdin/stdout sections of [ADR 0012](../decisions/0012-format-on-save.md). [`editorconfig.md`](../editorconfig.md), [`roadmap.md`](../roadmap.md), and the docstrings in `format.rs` / `pre_save.rs` / `lint_staged.rs` all point at the new ADR.

## How to test

Prerequisites: `bun install`, `cargo build`, `bun run tauri dev`. Open the moon-ide repo on itself.

### Happy paths against the moon-ide repo

1. **oxfmt for TS / JSON / etc.** Open `src/lib/state.svelte.ts`. Add a stray space at end of any line and a blank line at the very top. Ctrl+S.
   - Buffer re-reads from disk after save and matches `bun run fmt:js` output.
   - Tab dirty dot clears immediately.
2. **prettier for Svelte.** Open `src/App.svelte`. Mis-indent a block (insert a leading tab on a closing `</div>`). Ctrl+S → prettier runs as `prettier -w --ignore-path ../.prettierignore /abs/path/to/App.svelte`. The mis-indent is fixed.
3. **rustfmt.** Open `crates/moon-core/src/host.rs`. Mangle the indentation on a `match` arm. Ctrl+S → rustfmt runs as `rustfmt --edition 2021 /abs/path/to/host.rs`. Reformatted in place.
4. **Idempotency.** Re-save any of the above immediately. The chain runs again but the file is already canonical, so the post-stat bytes match the pre-stat bytes — no spurious dirty state.

### Chain truncation — the moon-landing config that motivated this change

5. Open the `~/code/moon-landing/server` workspace (or wherever the local clone lives). It's the project whose logs surfaced the original report:
   ```
   format-on-save: only the first command in a lint-staged chain runs pattern=*.{js,mjs,ts,svelte} count=2
   format-on-save: unsupported tool; skipping tool="node"
   ```
6. Open any `.ts` / `.svelte` / `.mjs` file under that folder. Make a small edit (introduce a stray space, mis-indent a line). Ctrl+S.
   - **Both old warnings are gone** (no allow-list, no first-command-only behaviour).
   - The file is formatted on save by `prettier -w --ignore-path ../.prettierignore <file>` only — the slow `node scripts/lint.ts --fix` step is skipped per the truncation caveat. The result on disk should match `bun x prettier -w --ignore-path ../.prettierignore <file>` from a terminal.
   - **Exactly once per dev session** the terminal logs `format-on-save: lint-staged chain truncated to last command chain_len=2`. Subsequent saves don't re-warn for the same chain length (per-process dedup).
7. Verify the truncation is visible but not noisy: edit and save five different files matching the same chain. The truncation warning should fire on the first save of the session and stay quiet for the next four.

### Arbitrary tool support

8. Drop a tiny shell script in the moon-ide repo (or a sandbox folder) that the allow-list previously rejected. Add a temporary lint-staged entry:
   ```json
   "*.fixme.txt": ["./scripts/uppercase.sh"]
   ```
   where `uppercase.sh` is `#!/bin/sh\ntr '[:lower:]' '[:upper:]' < "$1" > "$1.tmp" && mv "$1.tmp" "$1"`.
9. `chmod +x scripts/uppercase.sh`, save `.lintstagedrc.json` (clears the cache), then save a `whatever.fixme.txt` file with lowercase text.
   - On disk: uppercase. The script ran via the new pipeline.
   - Dev terminal: no warnings.
10. Revert the lint-staged entry and the script.

### Last-command failure leaves the editorconfig text on disk

11. Add a temporary chain to `.lintstagedrc.json`:
    ```json
    "*.fixme.txt": ["./scripts/marker.sh", "./scripts/exit-1.sh"]
    ```
    with `marker.sh` = `#!/bin/sh\nprintf 'first-ran' > "$1"` (would clobber the file if it ran) and `exit-1.sh` = `#!/bin/sh\nexit 1` (the only-run last command).
12. Save a `whatever.fixme.txt` with content `original  ` (trailing spaces). Expected:
    - Dev terminal: one `format-on-save: lint-staged chain truncated to last command` (first time saving a 2-command chain this session) **and** one `format-on-save: tool exited with error` for `exit-1.sh`. No `marker.sh` warning — it never ran.
    - File on disk: `original\n` (editorconfig stripped the trailing spaces and added the newline; the failing last command got to mutate nothing further; `marker.sh` was truncated out of the chain entirely so its text never lands).

### Project-local binary discovery

13. With moon-ide open on itself, save a `.svelte` file. Watch `node_modules/.bin/prettier` get used (you can confirm with `lsof -p <bun-tauri-dev-pid> | grep prettier` or by temporarily aliasing the system prettier to a no-op script and noticing the formatter still runs correctly).
14. Move `node_modules/.bin/prettier` aside (`mv node_modules/.bin/prettier node_modules/.bin/prettier.bak`). Restart `bun run tauri dev`. Save a `.svelte` file.
    - If a system `prettier` exists (say from a global `bun add -g prettier`), it runs. Otherwise the dev terminal logs exactly **one** `format-on-save: tool not found in node_modules/.bin or $PATH; skipping tool="prettier"` and the file lands editorconfig-normalised but unformatted. Subsequent saves of `.svelte` / `.ts` files don't re-warn (per-process dedup).
15. Restore: `mv node_modules/.bin/prettier.bak node_modules/.bin/prettier`.

### Editorconfig fallback for files without a rule

16. Rename `.lintstagedrc.json` aside (`mv .lintstagedrc.json /tmp/lsr.bak`). Edit a `.ts` file (add stray trailing whitespace + missing final newline). Ctrl+S.
    - Editorconfig pipeline still runs — trailing ws stripped, final newline added.
    - No formatter runs (no rule found). Save succeeds.
    - Restore: `mv /tmp/lsr.bak .lintstagedrc.json`. Next save formats again.

### Coder/agent writes

17. With the Coder panel open, prompt the agent to create an ugly `.ts` file: `Create src/scratch_unformatted.ts with content 'export const   x =1' on one line, no trailing newline.` The file the agent creates is formatted when it lands on disk — same path as before.
18. Delete the scratch file when done.

### Syntax errors don't break saves

19. Introduce a syntax error in a `.svelte` file (`<script>let x = </script>`). Ctrl+S.
    - Save succeeds; bytes on disk are the user's broken text (editorconfig still ran).
    - Dev terminal logs one `format-on-save: tool exited with error` with prettier's stderr trimmed.
    - Fix the syntax, save again — formatter runs cleanly.

### Unit test sweep

20. `cargo test -p moon-core --lib format::tests`: 6 green tests covering parse_command, bin_basename, the PATH walk, missing tool, non-zero exit, and the spawn-and-passes-path smoke test (uses a temp shell script — no oxfmt / prettier required in CI).
21. `cargo test -p moon-core --lib lint_staged::tests`: 9 green tests; `array_command_returns_full_chain` confirms the loader returns the entire chain (chain-handling policy now lives in the host caller, not the loader).
22. `cargo test -p moon-core --lib host::tests::save_file_`: 4 new integration tests exercising save_file end-to-end (arbitrary tool, only-last-command-in-chain runs per the truncation caveat, last-command failure preserves editorconfig text, editorconfig fallback when no rule matches). When the truncation TODO lifts, rename `save_file_runs_only_last_command_in_chain` to `save_file_runs_chain_in_order` and update its assertions to require both commands ran.

## What must keep working

- **All editorconfig behaviour from test plans 0002 and 0037.** Cache invalidation, the line-ending / trim-ws / final-newline transforms, the post-save re-read flow are unchanged. The lint-staged chain runs _after_ the editorconfig pass writes its bytes.
- **Coder write/edit tools (test plans 0040, 0044, 0046).** `write_file` and `edit_file` still route through `host.save_file`; the JSON tool result fields (`path`, `bytes_written`, `mtime_ms`, etc.) are unchanged. The only observable change is that on-disk content is now formatted by the whole chain.
- **Tab dirty-state coherence.** `saveActive` re-reads after save and recomputes the fingerprint. Saving an already-canonical buffer doesn't leave the tab dirty. Re-stat in `save_file` returns the post-format mtime so the editor's `loadedMtimeMs` tracks disk truth.
- **`bun run check`, `bun run lint`, `cargo check --workspace --exclude moon-desktop`, `cargo clippy --workspace --exclude moon-desktop --all-targets -- -D warnings`** all clean.
- **Direct `host.write_file` callers in tests** keep raw-write semantics. The pipeline is on `save_file` only.
- The team's `moon-landing` lint-staged config (the original bug report) — formats on save in moon-ide; matches the bytes `bun run lint-staged` would produce post-commit.

## Known limitations

- **No fs watcher for external `.lintstagedrc.json` edits.** Same story as `.editorconfig`: an edit made outside moon-ide (`git pull`, another editor) waits for a moon-ide-issued save to invalidate the cache, or a restart. Phase 5.
- **Chain truncation means earlier commands are silently skipped on save.** Documented above; reverts to "run the whole chain" once moon-landing's lint script is fast enough. Single-command chains and the `prettier -w`-as-last-step convention used across the team are unaffected, so the day-to-day surface still matches commit-time `lint-staged`.
- **No quoted-argument support in lint-staged commands.** Whitespace split. None of the team's configs use quoted args; if one starts to we'll add a real shlex.
- **No "format on save" toggle.** Hardcoded on, per AGENTS.md "hardcode first, configure later".
- **JS / YAML lint-staged variants** still log + ignore (JSON-only by design — same as ADR 0012).
- **Saves are not async-cancelled.** A 5s formatter timeout caps the worst case but Ctrl+S still waits for the chain. Sub-100ms in healthy installs.
- **RemoteHost (Phase 2) not yet exercised.** The seam works the same — `host.save_file` runs inside the container's `LocalHost` instance — but verifying that requires Phase 2 to be live. Smoke when it lands.

## Related

- ADRs: [0013 — file-based lint-staged invocation](../decisions/0013-format-on-save-file-based.md) (current), [0012 — format on save via lint-staged](../decisions/0012-format-on-save.md) (the original stdin/stdout design that this supersedes), [0006 — no settings file](../decisions/0006-no-settings-file.md).
- Specs: [editorconfig.md](../editorconfig.md), [roadmap.md § Phase 8](../roadmap.md#phase-8--lint--format).
- Prior plans: [0047 — format on save](0047-format-on-save.md) (the stdin/stdout shape this replaces), [0002 — editorconfig](0002-editorconfig.md), [0037 — revert icon and utf8 save](0037-revert-icon-and-utf8-save.md), [0040 — coder write tools](0040-coder-write-tools.md).
