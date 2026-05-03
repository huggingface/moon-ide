# Test plan 0014: CodeMirror syntax highlighting follows the theme

- **Date**: 2026-04-29
- **Phase**: Phase 1.5 (editor polish)

## What shipped

- Editor gets a hand-rolled `HighlightStyle` (`moonHighlight`)
  covering keywords, strings, numbers, functions, types,
  properties, tags, regex, operators, meta, and the markdown /
  diff token families.
- Syntax colours are driven by new `--m-syntax-*` CSS tokens
  defined in both `:root` and `:root.light`, so toggling theme
  re-skins the editor for free — the highlight style itself
  stays static.
- Chrome theme becomes a function of `dark: boolean`, wrapped in
  a dedicated `themeCompartment` that reconfigures on theme
  flip so CodeMirror's internal `dark` flag tracks the palette.
- Search and goto-line panels pick up the same CSS-variable
  treatment (light + dark), as do shared CM buttons / text
  fields.

## How to test

Prerequisites: `bun install`, then `bun run dev` (or `bun run dev:vite` + `bun run dev:tauri` per README).

1. Open `src/lib/editor/theme.ts` (TS), a `.rs` file from `crates/`, `Cargo.lock` (TOML), `bun.lock` (JSON), `README.md` (Markdown), and an HTML/Svelte file. Confirm each one shows non-default coloring: comments dimmed and italic, keywords purple, strings green, numbers/booleans warm orange, function names blue, types soft green.
2. Trigger the theme toggle from the command palette or the status-bar toggle ("Switch to Light Theme"). Expected:
   - Editor background, gutter, and selection flip to the light palette.
   - Syntax colors flip too — purple keywords become a darker purple, strings become a deep green, numbers a burnt orange, etc. Nothing stays dark-mode-tinted on a light background.
3. Toggle back to dark. Colors return to the dark palette without any flicker or full-document reflow.
4. Open `Ctrl+F`. The search panel uses the editor background tokens, the input is themed with `--m-bg-2` / `--m-fg`, and buttons use the same border / hover treatment as the rest of the app. Toggle theme: the panel re-skins with the editor.
5. Open a `.husky/pre-commit` (extension-less shell script) — confirm shebang detection still works and shell tokens use the new palette.

## What must keep working

- Existing language detection: `.ts/.tsx/.mts/.cts/.js/.json/.toml/.md/.rs/.css/.html/.svelte/.sh/.bash/.zsh` still highlight; `.<word>ignore` files still get the comment-only treatment.
- Tab markers (`→`) still render at the start of every leading tab.
- `editorconfig` reactivity (changing `indent_size` / `tab_width` and saving `.editorconfig` updates open buffers) still works — its compartment is independent from the theme compartment.
- Splits keep their own theme state in lockstep — toggling theme repaints both panes simultaneously.
- The scrollbar-corner moon icon still renders (its color is hardcoded; this is the existing WebKitGTK caveat documented in `theme.ts`).

## Known limitations

- The tab-marker `→` SVG and the scrollbar-corner moon SVG embed `#5a6480` directly because `data:` URLs can't read CSS variables. Both look slightly off in light mode; this is pre-existing and explicitly out of scope per the request.
- The syntax palette is hand-tuned for the moon-ide accent family rather than matching any existing public theme (One Dark, Tokyo Night, etc.). If the team wants a familiar palette later, swapping the `--m-syntax-*` token values is enough — no JS change needed.
- Coverage relies on each language grammar emitting Lezer tags. Anything emitted that we didn't list (extremely rare; the `tags` enum is small) falls back to plain `--m-fg`. That's a graceful degradation, not a crash.

## Related

- Specs: none directly; this is editor polish.
- ADRs: none.
- Prior test plans: 0002 (editorconfig), 0004 (markdown preview).
