# ADR 0004 — Code style and tooling

Date: 2026-04-26
Status: accepted

## Context

Moon IDE mixes Rust (`crates/`, `src-tauri/`) and TypeScript / Svelte (`src/`).
We want a single, fast, opinionated formatter and linter so the codebase
stays uniform without arguing about it. The Oxc project (oxlint, oxfmt) is
fast, supports our JS/TS surface, and matches Prettier output where it
overlaps. Type-aware lint checks let us catch mistakes that ESLint would
need full type info to find.

## Decision

### Indentation: tabs, everywhere

- All hand-edited files use **hard tabs**. Visual width is 2 columns for
  every language, set in `.editorconfig` and matched by `tab_spaces = 2`
  in `rustfmt.toml`. The on-disk character is always `\t`; what differs
  between languages is only how the formatter measures wrapping.
- Markdown, JSON, YAML, TOML: tabs for indentation as well.
- `.editorconfig` is the source of truth for indentation, line endings,
  trailing whitespace, and final-newline behavior. Every formatter we
  use already reads it; Phase 1.5 makes the editor itself read it too
  (see [editorconfig.md](../editorconfig.md)).

### Line length: 120

- `printWidth: 120` for oxfmt and prettier.
- `max_width: 120` for rustfmt.
- Reason: an IDE codebase has long type names (Svelte event types,
  `Arc<dyn ... + Send + Sync>` traits, CodeMirror extensions). 80 / 100
  forces too many noisy wraps; 120 is the most common modern default.

### Braces: always

- No single-statement `if` / `else` / `for` / `while` / `do` without
  braces. Enforced by `curly: ["error", "all"]` in oxlint, and by reviewer
  taste in Rust (rustfmt does not check this; bodies are always blocks
  anyway with our wrapping rules).

### JS / TS / JSON / CSS / HTML / Markdown — Oxc

- **Formatter**: `oxfmt` (`useTabs: true`, `tabWidth: 2`, `printWidth: 120`).
- **Linter**: `oxlint` with the **type-aware** suite enabled
  (`oxlint --type-aware`). The default categories `correctness` (error)
  and `suspicious` (warn) are on; `style` is on with the project rules.
- **Type-checker**: `@typescript/native-preview` (the Go port of `tsc`,
  shipped as `tsgo`), used for `tsgo --noEmit` checks. The classic
  `typescript` package stays as a dev dependency only because
  `svelte-check` needs it; once `svelte-check` learns about `tsgo` we
  drop classic `tsc`.

### Svelte — Prettier (for now)

Oxfmt does not yet support `.svelte` files (see the
[Oxc compatibility matrix](https://oxc.rs/compatibility)). For `.svelte`
specifically we run `prettier --plugin prettier-plugin-svelte`. Same
options as oxfmt (tabs, width 120, single quotes, trailing commas).
When oxfmt grows Svelte support we delete prettier from this repo.

Lint for `.svelte`:

- Oxlint runs on the `<script>` blocks via its TS plugin.
- Template-level lint we accept is missing today; we'll revisit when
  oxlint or another tool fills the gap.

### Rust — rustfmt + clippy

- `rustfmt.toml`: `hard_tabs = true`, `tab_spaces = 2`, `max_width = 120`,
  `edition = "2021"`. The community default is `tab_spaces = 4`, but
  since we use hard tabs the on-disk file is unchanged — `tab_spaces`
  here only tells rustfmt how wide a tab is when computing whether a
  line fits. We pick 2 to match every other language in the repo,
  which keeps `printWidth: 120` meaningful across the stack.
- `cargo clippy --all-targets --all-features -- -D warnings` is the
  CI gate.

### Editor target

- `tsconfig.json` `target: "ES2024"`, `lib: ["ES2024", "DOM", "DOM.Iterable"]`.
- Vite build target: bumped to `safari17`/`chrome120` (Tauri 2 ships
  webkit2gtk-4.1 ≥ WebKit 2.36 and modern Edge WebView2; both handle
  ES2023+ fully). We don't need to be a polyglot.
- Source code can use anything ES2024 / TC39 stage-3 the runtime
  supports (e.g. `Object.groupBy`, `Promise.withResolvers`,
  `Iterator.prototype.*`).

## Scripts

`package.json` exposes top-level aggregates that cover the whole
codebase, plus `:js` / `:rust` variants for the impatient:

- `bun run fmt` — `fmt:js` + `fmt:rust`.
- `bun run fmt:check` — same, in check-only mode (CI).
- `bun run lint` — `lint:js` (`oxlint --type-aware`) + `lint:rust`
  (`cargo clippy ... -- -D warnings`).
- `bun run lint:fix` — auto-fixable JS/TS rules, plus a clippy run
  for visibility.
- `bun run check` — `tsgo --noEmit`, `svelte-check`, and
  `cargo check` (excludes the Tauri shell, which needs system libs).
- `bun run test` — `cargo test --workspace --exclude moon-desktop`.

Hooks: a pre-commit hook (managed by `husky` + `lint-staged`) runs the
formatters on staged files only.

## Consequences

- One formatter for the whole JS/TS surface, one for Svelte, one for
  Rust. Three configs, no daisy-chaining of plugins.
- We accept that Svelte template lint is weaker than the rest until
  oxlint catches up. We compensate with `svelte-check` types.
- We commit to the Oxc ecosystem evolving. If `oxlint --type-aware`
  regresses or the Svelte gap stays open for too long, we revisit and
  potentially fall back to ESLint for those files only.
