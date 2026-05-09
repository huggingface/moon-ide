# ADR 0006 — No `settings.json`; `.editorconfig` + `AppState` cover us

Date: 2026-04-26
Status: accepted

## Context

Phase 1 shipped a per-project `settings.json` (committed to the repo) with three sections:

- `editor`: `tab_size`, `insert_spaces`, `format_on_save`, `render_tabs`.
- `keymap`: a `Record<string, string>` of command → shortcut.
- `theme`: `mode = dark | light`.

Of these:

- `editor.tab_size` and `editor.insert_spaces` are exactly what
  [`.editorconfig`](../editorconfig.md) specifies, and Phase 1.5 will
  read it (oxfmt, prettier and rustfmt all do already — the editor was
  the only piece in the chain that didn't).
- `editor.format_on_save` has no consumer yet. The lint/format pipeline
  is Phase 8.
- `editor.render_tabs` has one consumer (the tab-arrow decoration in
  [`src/lib/editor/highlightTabs.ts`](../../src/lib/editor/highlightTabs.ts))
  and no concrete request to flip it.
- `keymap` is unused. Bindings are hardcoded in `App.svelte` and the
  command palette (per [ADR 0005](./0005-bootstrap.md) and AGENTS.md
  scope discipline: hardcode first, configure later).
- `theme.mode` has one real read/write surface: the "Toggle Theme"
  command. Theme is per-user / per-machine, not per-project — it has no
  reason to live in a project-committed file.

Net: `settings.json` was one file with one in-flight knob (theme), one
already-redundant section (`editor.*`, soon owned by `.editorconfig`),
and two speculative sections (`keymap`, `format_on_save`).

## Decision

Delete `settings.json` entirely. Replace it with two clear surfaces:

1. **`.editorconfig`** (Phase 1.5) owns project-level code style:
   `indent_style`, `indent_size` / `tab_width`, `end_of_line`,
   `insert_final_newline`, `trim_trailing_whitespace`, `charset`. Same
   precedence rules as oxfmt / prettier / rustfmt. No moon-specific
   overlay.
2. **`AppState`** (`<config_dir>/state.json`, schema in
   [`crates/moon-protocol/src/app_state.rs`](../../crates/moon-protocol/src/app_state.rs))
   owns per-machine, per-user state. Today: `last_session` (workspace +
   open tabs + active pane) and `theme`. Read/written via a single pair
   of Tauri commands `app_state_load` / `app_state_save`; the frontend
   does read-modify-write.

The previously-configurable defaults are now hardcoded in
[`Editor.svelte`](../../src/lib/components/Editor.svelte): tab size 2,
tabs (not spaces), tab markers visible. These are the team's house style
and `.editorconfig` will refine them per file in Phase 1.5.

## Why this is OK now and not later

Per AGENTS.md "no premature migrations", the roadmap hasn't shipped a
stable surface yet. There are no users, so removing `Settings` /
`EditorSettings` / `ThemeSettings` and the `settings_*` Tauri commands
is just deletion — no aliases, no fallback parsing, no ADR-tracked
deprecation window.

When the final roadmap phase lands and we declare a stable user-facing
surface, the question "is this per-project or per-user?" gets a clear
answer:

- per-project → `.editorconfig`. moon-ide deliberately does not
  layer its own per-project config on top: format-on-save reads
  the project's existing tooling (`lint-staged`, `package.json`
  scripts, `editorconfig`), language services come from the
  project's own toolchain. Adding a `moon`-specific per-project
  file would create a parallel source of truth where there's
  already an established one.
- per-user → `AppState`.

## Consequences

- Phase 1.5 no longer has a "wire `Settings` precedence with
  `.editorconfig`" item; it has only "honor `.editorconfig` directly".
- `format_on_save` re-appears as a knob (probably in `.editorconfig`
  via the `[*]` section as a moon-specific extension, or hardcoded
  on/off per-language) when Phase 8 needs it. Not before.
- `keymap` re-appears when there is a concrete request to rebind a
  shortcut. Not before.
- `render_tabs` re-appears as a knob when someone wants tabs hidden.
  Until then, the constant in `Editor.svelte` is the single source of
  truth.
- The Tauri shell exposes one fewer command pair (`session_*` and
  `settings_*` collapse into `app_state_*`).
