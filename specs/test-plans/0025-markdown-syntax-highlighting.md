# Test plan 0025: Rich Markdown rendering — LSP hovers + README previews

- **Date**: 2026-05-04
- **Phase**: Phase 4 (LSP) — hover UX polish

## What shipped

- LSP hover popovers now render Markdown (headings, paragraphs, fenced code blocks, inline code, lists, emphasis, links) instead of dumping raw `\`\`\`typescript` backticks as text.
- Fenced code blocks in both the hover popover and `.md` file previews are syntax-highlighted using the editor's own CodeMirror grammars — same parser, same colours, same light/dark theme flip.
- New `src/lib/editor/highlightCode.ts`: lazy grammar loader + `classHighlighter`-based HTML emitter; `renderMarkdown` became async so it can preload grammars before markdown-it's synchronous render.
- Shared `.markdown-body` and `.tok-*` styles lifted to `src/styles.css` so `MarkdownView.svelte` and the hover popover stay visually in lockstep.

## How to test

Prerequisites: `bun install`, `bun run tauri dev` against moon-ide itself (so `node_modules/.bin/tsgo` is present and the TS LSP is live).

### Hover popover — structured content

1. Open any `.ts` file in moon-ide (`src/lib/state.svelte.ts` is a good one).
2. Hover over an identifier imported from another module — e.g. `renderMarkdown` in `src/lib/components/MarkdownView.svelte`, or `workspace` in any file that imports it.
3. Expected: the hover popover shows
   - the function/const signature as a **syntax-highlighted TypeScript block** (keywords purple, strings green, types green, etc.) — matching how the same tokens render in the editor,
   - any TSDoc body rendered as Markdown (paragraphs, inline code in a pill, maybe a list).
   - No stray triple-backticks, no "typescript" label as literal text.
4. Compare colours: hover over the same identifier in the hover **and** look at its declaration in the editor. Keyword / string / type colours should be visually identical. If they disagree, that's a bug (we reuse the same CSS variable tokens on both sides).
5. Hover tooltip dimensions: it should not exceed ~72 characters wide or ~360px tall. Anything longer gets an internal scrollbar rather than blocking the editor.

### Hover popover — edge cases

1. Hover over a JS standard-library identifier (e.g. `Array.from`, `Promise`). Should still render correctly — the TSDoc fenced blocks are typical.
2. Hover over an identifier with **no** LSP info (a random whitespace span, or a comment). No popover should appear — `null` from the LSP is still the silent-skip path.
3. Hover over something that produces a multi-paragraph hover (e.g. a complex generic function). Scrolling inside the popover should work smoothly without hijacking the outer editor scroll.
4. First-ever hover of a given language may have a tiny one-time delay (dynamic grammar import). Subsequent hovers for the same language are instant.

### README preview — `.md` files

1. Open `README.md` in moon-ide (or any `.md` file in `specs/`).
2. Switch to preview mode (if there's an editor/preview toggle — otherwise it's the default for `.md`).
3. Expected: fenced code blocks that were showing as plain `<pre>` now have syntax colour, matching the editor's palette:
   - ` ```rust ` blocks: keywords (`fn`, `let`, `mut`, `pub`) in purple, lifetimes / types in green, strings in green.
   - ` ```typescript ` blocks: same palette as the editor would show.
   - ` ```bash ` / ` ```sh ` blocks: keywords highlighted via the legacy `shell` mode.
   - ` ```json ` blocks: keys/values separated.
   - ` ```toml ` blocks (try `Cargo.lock`-style excerpts): sections and key=value differentiated.
4. Scroll a long `.md` (e.g. `specs/lsp.md`). The scroll wrapper is unchanged; only the typography and code-block contents should look different.

### Unknown-fence fallback

1. Open a `.md` file containing a fence like ` ```glsl ` or ` ```prolog ` (grammars we don't bundle).
2. Expected: the block renders as plain monospaced text inside the usual `<pre>` chrome — no colour, no error, no mis-highlighted output. Selection / copy still work.
3. Open a `.md` file with a fence that has **no** language info (` ``` ` on its own). Same expectation: plain monospaced fallback.

### Theme flip

1. Open a `.md` file or trigger an LSP hover so a Markdown surface is visible.
2. Use the theme picker to flip between dark and light.
3. Expected: code-block colours re-skin _instantly_ without a re-render — they're driven by `--m-syntax-*` CSS variables, so the whole palette updates at CSS level without JS being involved.
4. Specifically verify light-mode contrast is readable (purple keywords on white, green strings, etc.) — we share the `:root.light` token overrides with the editor, so if one palette works the other should too.

## What must keep working

- Plain-text hover rendering (when the server returns a non-Markdown body) still renders — the `renderMarkdown` pipeline emits sensible HTML for a string that has zero markdown syntax (it produces a single `<p>`).
- Markdown preview clicking/opening of internal links (see `MarkdownView.svelte`'s `onArticleClick`).
- DOMPurify sanitisation surface is unchanged: raw HTML in a `.md` file is still escaped, `javascript:` URIs still dropped, `<script>` tags can't sneak through the fence-highlighter either (we escape HTML before wrapping spans, so any `<` or `>` in code becomes `&lt;` / `&gt;` before reaching innerHTML).
- LSP stage-1 behaviour from test plan 0024: diagnostics chips, status-bar pills, completion on Ctrl-Space, missing-binary handling, lifecycle edge cases.
- Editor syntax highlighting is unchanged — the code-block highlighter is a separate consumer of the same grammars, not a replacement.

## Known limitations

- Only languages we ship a CodeMirror grammar for get coloured. The current set is the one already shipped for the editor: ts/tsx/js/jsx, rust, css/scss/less, html/svelte, json(c), markdown, toml, shell, yaml, dockerfile, properties. Everything else falls back to plain `<pre>` — silent mis-highlighting would be worse.
- Inline code (single backticks) stays plain (no highlighting, just a background pill). Fence-style blocks are the point of interest.
- `completion.info` — the documentation body shown next to an autocomplete entry — is still rendered as plain text. Different CM API (accepts `string | HTMLElement`), small follow-up commit when someone needs it.
- First hover of a given language may take ~50–150 ms longer than steady-state because the grammar is dynamically imported on the first use. Steady-state hovers are instantaneous. We don't eagerly preload grammars to keep the initial bundle small.
- Hover popover is capped at 72 ch wide / 360 px tall. Extreme hovers (a whole file's worth of JSDoc) get an internal scrollbar. If this is ever wrong for a real team use-case we bump the caps.
- Rendering is async → one event-loop turn between cursor settle and popover showing. Imperceptible in practice; would only matter if we switched to a high-frequency hover probe.

## Related

- Specs: `specs/lsp.md` (hover pipeline), `specs/frontend.md` (editor stack), ADR 0004 — code style.
- Prior test plans: `0024-lsp-typescript-stage-1.md` (which originally noted "Hover tooltip is plain textContent; Markdown formatting is preserved only as whitespace" as a known limitation — this plan retires that bullet).
