# Test plan 0002: `.editorconfig` honored end-to-end

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- `.editorconfig` is resolved end-to-end via `ec4rs` in
  `moon-core`, cached per directory, and fed into CodeMirror
  through a dedicated compartment so `indent_size` / `tab_width`
  flip live when you save a `.editorconfig`.
- Server-side save pipeline in `moon-core::pre_save` applies
  line-ending, trim-trailing-whitespace, and final-newline rules
  from the resolved config on every write.
- Replaces the hardcoded `TAB_SIZE = 2` / `INDENT_UNIT = '\t'`
  constants with `EditorConfig::default()` — the moon-ide
  defaults now live in one place (Rust + TS twin).

## How to test

Prerequisites: `bun install`, host deps installed per `README.md`,
`bun run dev` running (or the packaged binary launched against the
moon-ide repo).

1. Open the moon-ide repo. Open `crates/moon-core/src/lib.rs`.
   - Type `Tab` at the start of a line. Expected: a single `\t` is
     inserted (visible because `highlightTabs` paints the tab arrow);
     the cursor advances by 2 visual columns (the `tab_width` set in
     `.editorconfig`).
2. Open `README.md` (catch-all `[*]` section in `.editorconfig`).
   - Type `Tab`. Expected: `\t` inserted, advances by 2 columns.
3. Add an end-of-line space to a line, save (`Ctrl+S`). Expected: the
   trailing space is gone after the save, the buffer (CodeMirror's doc)
   is in sync with disk, the dirty marker clears.
4. Delete the trailing newline at the end of a file, save. Expected: a
   single trailing newline reappears (final newline rule).
5. With Cargo not installed in the test workspace? Skip: rustfmt
   formatting is unrelated. The point of this step set is the editor
   pre-save pipeline, not formatters (those land in Phase 8).
6. Edit `.editorconfig` itself. Change `[*]` from `indent_size = 2` to
   `indent_size = 8`. Save (`Ctrl+S`). Expected: typing `Tab` in any
   already-open file updates to advance by 8 columns immediately, no
   tab-switch needed. Revert the change before moving on.
7. Add a section `[*.md] indent_style = space` to `.editorconfig`,
   save. Open `README.md`. Expected: typing `Tab` inserts spaces
   (count = `indent_size`). Revert.
8. Open `Cargo.toml`. Expected: `Tab` inserts `\t` (the `[*]` rule
   wins because `Cargo.toml` has no specific section in our
   `.editorconfig`).
9. Quit and relaunch. Expected: open files are restored AND each tab's
   indent settings match the saved `.editorconfig` from frame one — no
   visible flicker between defaults and resolved values.

## What must keep working

Regression checks. If any of these break, the commit needs a follow-up.

- `Ctrl+W` still closes the active tab; closing a dirty tab still
  prompts for discard.
- `Ctrl+S` saves; the dirty marker still clears on save.
- `Ctrl+Z` after a save still reverts the dirty marker once the doc
  matches the freshly-loaded fingerprint (the post-save reload is what
  makes this still true — verify by editing a line, saving, then
  Ctrl+Z'ing back to before the edit and confirming the marker
  disappears at the right point).
- Theme toggle (status-bar moon button or `Ctrl+Shift+P → Toggle Theme`)
  still works.
- Image files still preview (binary detection in `read_file` is
  unchanged).
- Search (`Ctrl+Shift+P → Find in Files`) still returns results.
- `cargo test -p moon-core` passes (covers editorconfig resolution and
  pre-save transforms).

## Known limitations

Things we deliberately did not do, with one-line justification.

- **No fs watcher.** `.editorconfig` cache only invalidates when
  moon-ide itself writes the file. External edits (`git pull`, another
  editor) need an IDE restart. Phase 5 ships the watcher alongside git
  status.
- **No multi-line-string exemption for `trim_trailing_whitespace`.**
  Spec calls for it; doing it correctly requires per-language parsing.
  Workaround: set `trim_trailing_whitespace = false` for the affected
  glob.
- **`charset` is recorded, not enforced.** Files outside utf-8 still
  fail at `read_file` (which already mandates UTF-8). No conversion or
  warning UI yet.
- **`max_line_length` is parsed but unused.** Becomes a CodeMirror
  ruler decoration in a later phase.
- **`insert_final_newline` has no "unset" state in our model.** The
  EditorConfig spec leaves it tri-valued; we collapse to `true` by
  default — see ADR 0006 (no settings layer to disambiguate against).

## Related

- Specs: `specs/editorconfig.md`, `specs/roadmap.md` (Phase 1.5).
- ADRs: `0006-no-settings-file.md`.
- Prior test plans: `0001-initial-bootstrap.md`.
