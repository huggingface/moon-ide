# ADR 0032 — Block-level markdown caching for streaming

Date: 2026-07-09
Status: accepted

## Context

`CoderMarkdown.svelte` renders the model's streaming output
(thinking + answer text) as markdown. The old path ran markdown-it +
DOMPurify over the **entire** source string on every coalesced rAF
tick, then did a single `{@html html}` — a wholesale `innerHTML`
replacement of the entire `<article>`. During streaming (~30
deltas/sec) every paragraph, list, and code block was torn down and
rebuilt ~30 times/sec, which caused visible flicker on longer
thinking traces.

The module-level `markdownCache` (keyed on the full source string)
couldn't help mid-stream: every delta changes the string, so every
delta is a cache miss. The cache only helped on folder-swap /
re-mount, where the same final string re-appeared.

Test-plan 0042 already flagged this as a known limitation: _"Streaming
patches via DOM diffing land if anybody actually hits this in
practice."_ A team member reported the flicker.

## Decision

Split the parsed token stream into top-level blocks and render each
independently, so frozen blocks (everything except the still-growing
tail) keep their DOM nodes across deltas.

### How it works

1. `renderMarkdownBlocks(source)` in `markdown.ts` calls
   `parser.parse(body, {})` once, then `splitTopLevelBlocks` walks
   the flat token array and groups it into runs: each run is a
   maximal sequence that starts at `level === 0` and continues until
   the level returns to 0 after a close (or, for self-closing tokens
   like `fence` / `hr`, just that one token).
2. Each block's token slice is rendered to HTML via
   `parser.renderer.render(slice, …)` and sanitized by DOMPurify
   independently. The per-block cache key is
   `${index}\x00${sourceText}` — the index disambiguates blocks
   with identical source text whose HTML differs (e.g. duplicate
   headings whose `id` slugs are suffix-de-duplicated
   document-wide).
3. `CoderMarkdown.svelte` replaces the single `{@html html}` with
   `{#each blocks as block (block.key)}{@html block.html}{/each}`.
   Svelte's `{@html}` effect checks `value === (value = get_value())`
   and skips the `innerHTML` write when the string is unchanged, so
   frozen blocks' DOM nodes are never touched mid-stream.

### Why token-level splitting, not source-level

Naive source splitting on blank lines breaks on fenced code blocks
(they swallow `\n\n`), setext headings (`===` retroactively turns the
previous paragraph into a heading), and lazy list continuation.
markdown-it's parser already resolves all of that, so splitting its
_output_ token array is correct by construction.

### The streaming invariant

Once markdown-it closes a top-level block and moves on, appending
more source can never retroactively change it — no backtracking. Only
the **last** block is "live" and still growing. A frozen block's
source text, tokens, and rendered HTML are identical across deltas,
so its cache entry is a permanent hit. The tests in
`markdown.test.ts` verify this invariant explicitly.

### What stayed the same

- The rAF coalescer, `renderToken` guard, visibility gate
  (`visibleOnce`), and placeholder path are unchanged in shape —
  they now bound the block-array re-render instead of the
  whole-document one.
- `renderMarkdown` / `getCachedMarkdown` (the whole-string path) are
  unchanged. `MarkdownView.svelte` (file preview), the LSP hover
  popover, and review-comment rendering still use them — those are
  non-streaming surfaces where whole-string rendering is fine.
- The heading-anchor slug deduplication rule runs document-wide
  during `parser.parse`, so heading `id`s are correct even though
  rendering is per-block. The per-block cache key includes the
  block's index so duplicate headings with different suffixes don't
  collide in the cache.

## Consequences

- Frozen blocks' DOM nodes survive across streaming deltas — no
  flicker on paragraphs, lists, or code blocks that have already
  finished. Only the one still-growing tail block gets an
  `innerHTML` swap, which reads as streaming movement, not flicker.
- Two new caches: a per-block HTML cache (`blockHtmlCache`, capped
  at 2000) and a whole-source → block-array cache
  (`blockArrayCache`, capped at 500). Both are FIFO-evicted and
  reset on page reload, same as the existing `markdownCache`.
- `CoderMarkdown.svelte` is the only consumer of the block-level
  path. Other markdown surfaces keep the simpler whole-string path.

## Alternatives considered and rejected

- **DOM diffing library (morphdom, etc.).** Adds a dependency and a
  reconcile pass to solve a problem that per-block keying solves
  structurally — Svelte's keyed `{@html}` already skips unchanged
  strings for free.
- **Source-level splitting on `\n\n`.** Breaks on fenced code
  blocks, setext headings, and lazy list continuation. Token-level
  splitting is correct by construction.
- **Incremental markdown-it parsing.** markdown-it doesn't support
  resumable parsing; it always parses the full source. The per-block
  cache achieves the same outcome (frozen blocks skip the parse
  entirely) without modifying the parser.
