# .editorconfig support

Status: planned for Phase 1.5.

## Why we care

`.editorconfig` is the de-facto cross-editor file for "how should this codebase be typed". Every formatter we use already reads it (oxfmt, prettier, rustfmt). The editor is the only piece in the chain that doesn't, which means typing in moon-ide diverges from oxfmt / prettier / rustfmt output until the pre-commit hook fires. For a project that targets self-hosting (see [ADR 0005](decisions/0005-bootstrap.md)) this is a real bootstrap problem, not just a polish item.

## Scope (v1)

We honor a deliberately small set of keys. Everything else is parsed and stored so plugins can reach it later, but the editor and the save pipeline only act on these:

| Key                         | Effect                                                                                                       |
| --------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `indent_style`              | `tab` or `space` — drives CodeMirror's Tab keymap (insert `\t` vs N spaces).                                 |
| `indent_size` / `tab_width` | CodeMirror `EditorState.tabSize` and the number of spaces inserted on Tab.                                   |
| `end_of_line`               | Pre-save hook normalizes line endings (`lf` / `crlf` / `cr`).                                                |
| `insert_final_newline`      | Pre-save hook ensures the file ends with exactly one `\n` (or strips trailing ones).                         |
| `trim_trailing_whitespace`  | Pre-save hook strips trailing whitespace per line, except inside multi-line strings.                         |
| `charset`                   | Only `utf-8` (and `utf-8-bom`, recorded but discouraged) supported in v1; warn if anything else is declared. |
| `max_line_length`           | Stored, surfaced as a CodeMirror ruler decoration in a later phase. Not enforced.                            |

Glob handling is the standard editorconfig precedence: most-specific section wins, `[*]` is the catch-all, `root = true` stops the upward walk.

## Architecture

### Parser lives in `moon-core`

A new module `moon_core::editorconfig`:

- Wraps the `ec4rs` crate (`ec4rs = "1"`).
- Exposes `WorkspaceHost::editorconfig_for(rel_path) -> EditorConfig` returning a normalized struct (everything resolved, no inheritance to walk client-side).
- Caches per-directory configs; invalidates on any `.editorconfig` change reported by the existing fs watcher.
- Returns the same struct shape regardless of whether the host is local or remote (Phase 2). The remote host serves it over JSON-RPC.

This keeps the rule that the UI never reaches into the filesystem; it asks the host. It also means agents and devcontainer-hosted tools see the same config.

### Pre-save pipeline

Today, save is a one-shot `host.write_text(path, contents)`. Phase 1.5 introduces:

```rust
// Conceptual; lives in moon-core.
trait BeforeSaveTransform {
    fn apply(&self, file: &mut FileState, ec: &EditorConfig) -> Result<()>;
}
```

The default pipeline is, in order:

1. `EnsureLineEndings` — `end_of_line`.
2. `TrimTrailingWhitespace` — `trim_trailing_whitespace`.
3. `EnsureFinalNewline` — `insert_final_newline`.

Phase 8 (lint/format) appends a `RunFormatter` step at the end of this list. Whether to run it is a Phase-8 decision (probably an `.editorconfig` extension key or a hardcoded per-language default — see [ADR 0006](decisions/0006-no-settings-file.md), there is no `Settings.editor.format_on_save` to consult).

### Editor (CodeMirror) integration

A small Svelte module reads the `EditorConfig` for the active file and produces a `Compartment` with:

- `EditorState.tabSize.of(ec.tab_width)`
- An `indentUnit` set from `indent_style + indent_size` (`'\t'` or `'  '`).
- A custom Tab keymap that defers to `indentMore` / `indentLess` (already in CM6 `defaultKeymap`) but inserts the right characters.

When a different file becomes active, the compartment is reconfigured. When the file's `.editorconfig` changes, the compartment is reconfigured.

### Precedence

For any given file, the effective config is computed by:

1. Start from moon-ide built-in defaults (`tabs, tab_width = 2, lf, final newline, trim trailing ws`).
2. Apply `.editorconfig` resolution from the file upward.

There is no third "user override" layer — see [ADR 0006](decisions/0006-no-settings-file.md). If we ever need glob-scoped overrides, they live as moon-specific keys inside `.editorconfig` (per editorconfig spec, unknown keys are preserved) rather than in a parallel file.

This precedence is the same for the editor and for the pre-save hooks, so what you see while typing matches what gets saved.

## Out of scope for v1

- No support for non-utf-8 charsets beyond surface warnings.
- No pluggable transforms — Phase 9 (custom tool plugins) takes care of that, not this phase.
- No UI to inspect "which `.editorconfig` rule won for this file"; we'll add it when there's an actual debugging need.
- No automatic creation / repair of `.editorconfig`. This is a read-only feature.

## Test plan

- Unit: `moon-core` parses fixtures (`indent_style = tab`, `[*.md] indent_size = 4`, nested with `root = true`, etc.) and returns the expected resolved struct.
- Unit: each `BeforeSaveTransform` is idempotent and a no-op when its key is unset.
- Integration: open the moon-ide repo in moon-ide, type Tab in `crates/moon-core/src/lib.rs` (rust, tabs, width 2) and in `src/App.svelte` (svelte via prettier config, tabs, width 2) and `Cargo.toml` (tabs from the `[*]` section); verify the inserted character and visual width match what oxfmt / prettier / rustfmt would output for the same line.
