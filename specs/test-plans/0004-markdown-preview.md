# Test plan 0004: markdown rendered preview

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- New `src/lib/markdown.ts`: tiny markdown-it pipeline configured
  with `html: false` (raw HTML escaped) + `linkify: false` (no
  surprise auto-links), then run through DOMPurify for defense in
  depth. Every rendered `<a>` carries `rel="noopener noreferrer"`.
- New `MarkdownView.svelte`: scrollable `.markdown-body` article
  rendered from sanitised HTML. Click handler intercepts every
  `<a>`: `http(s)://`, `mailto:`, and `tel:` URLs are forwarded to
  the OS via `@tauri-apps/plugin-opener`'s `openUrl` (default
  browser, mail client, dialer); in-page `#anchors` get the native
  scroll; everything else (relative paths, `file://`, custom
  schemes) is dropped on the floor. The webview itself never
  navigates, so a stray `https://…` click can't replace the IDE
  shell with the page.
- `EditorPane` picks `MarkdownView` over `Editor` whenever the
  active path is markdown and `previewModeFor(path) === 'preview'`.
- `EditorTabs` grows a Source/Preview toggle anchored to the right
  end of the strip. The toggle only renders when the active tab is
  markdown; switching tabs hides it again.
- `WorkspaceState` learns `previewModeFor(path)`,
  `setPreviewMode(path, mode)`, `togglePreviewMode(path)`. The mode
  is stored per buffer (not per pane), defaults to `preview` for
  `.md`/`.markdown`/`.mdown`, and is GC'd along with the buffer
  when the last pane drops it.
- New palette command "Markdown: Show Preview" / "Markdown: Show
  Source" (label flips with the current state). Hidden when the
  active tab isn't markdown via the new optional `Command.visible`
  predicate.

## How to test

Prerequisites: `bun install`, host deps installed per `README.md`,
`bun run dev` running.

1. Open the moon-ide repo. Click `README.md`. Expected: the file
   opens directly in **Preview** with formatted headings, links,
   code blocks, and tables; the right end of the tab strip shows
   `Source / Preview`, with `Preview` highlighted.
2. Click `Source`. Expected: CodeMirror takes over with the
   markdown source; headings stop being formatted; the toggle now
   highlights `Source`.
3. Click another non-markdown tab (e.g. `Cargo.toml`). Expected:
   the toggle disappears; the editor shows source as before.
4. Click back to `README.md`. Expected: it remembers the last
   mode you picked (Source), not the default — the per-buffer
   memory survives tab switches.
5. Open the command palette (`Ctrl+Shift+P`) with `README.md`
   active. Type "markdown". Expected: a single command
   `Markdown: Show Preview` (or `Show Source`, depending on
   current mode). Activate it; the view flips. Reopen the
   palette; the label has flipped.
6. With `Cargo.toml` active, open the palette and type "markdown".
   Expected: no result (the command's `visible()` returned false).
7. Edit the source (in Source mode), tab back to Preview. Expected:
   the rendered HTML reflects the in-memory text (no save needed),
   and the dirty marker is still on the tab.
8. Split the editor (`Ctrl+\`). Open `README.md` in both panes.
   Toggle preview on the left. Expected: both panes flip together
   (preview mode is per-buffer by design — see
   `WorkspaceState.previewModeFor` rationale). If we later want
   per-pane modes that's a deliberate follow-up.
9. **XSS smoke test.** Create a `.md` file with this body:

   ```markdown
   <script>document.title='OWNED'</script>
   <img src=x onerror="document.title='OWNED'">
   [trap](javascript:alert(1))
   [data-html](data:text/html,<script>alert(1)</script>)
   ```

   Expected in Preview:
   - The `<script>` block renders as escaped text, not as a
     script element. Page title stays "moon-ide" (or whatever it
     already was).
   - The `<img onerror=…>` is gone after sanitisation.
   - The `javascript:` link renders as text or as an `<a>` with
     no working `href` (markdown-it's URL validator drops it
     before sanitisation; DOMPurify drops anything that slips
     through).
   - The `data:text/html,…` link is dropped by DOMPurify
     (`ALLOW_UNKNOWN_PROTOCOLS: false`).

10. Click an `https://` link in the preview (e.g. one in the
    README). Expected: the URL opens in your default OS browser;
    the IDE webview itself stays on the README, the active tab
    doesn't change, no new IDE window. `mailto:` and `tel:` links
    behave the same way (mail client / dialer instead of browser).
11. Click a relative-path link (e.g. `[other](./other.md)`) or a
    custom-scheme link. Expected: nothing happens — those still
    fall through the click handler. Linked-workspace-file
    navigation is a deliberate follow-up.
12. Click an in-page `[link](#anchor)` (or use any of the auto-
    generated heading anchors once we ship them). Expected: the
    article scrolls to the target without leaving the tab.

## What must keep working

- All Phase 1 / Phase 1.5 invariants from test plans 0001-0003.
- Editorconfig for `.md` files still honoured in Source mode
  (test by setting `indent_size = 2` for `*.md` and verifying
  Tab inserts 2 spaces in Source).
- Closing a markdown tab cleans up `previewModes` along with the
  buffer; reopening starts back at the default `preview`.
- `closeSplit` GCs preview-mode entries for any path that was
  only on the right pane.

## Known limitations

- **No syntax highlighting inside code fences.** `marked`/`markdown-it`
  - a highlighter (highlight.js, shiki, prism) is a significant
    bundle hit; we add it when someone asks. Code blocks render as
    monospaced plain text in a styled `<pre>`.
- **No math, no Mermaid, no footnotes, no GFM task lists.** All
  follow-ups; the plain CommonMark + tables that markdown-it ships
  with by default is what we render.
- **Relative image paths don't resolve.** A `![](logo.png)` in a
  markdown file inside the workspace renders a broken image — we'd
  need to rewrite the URL to `convertFileSrc(absolutePath)`. Lands
  with the broader "linked assets" follow-up.
- **Relative-path and `file://` links are not openable.** Linking
  to a sibling `.md` does nothing in preview today — landing that
  cleanly means resolving paths against the workspace and opening
  a tab, which is the broader "linked assets" follow-up.
- **Per-pane preview mode is not supported.** Same buffer in two
  panes shows the same mode. Splitting a markdown file with one
  pane in Source and one in Preview is on the follow-up list.

## Related

- Specs: `specs/roadmap.md` (Phase 1.5 — Markdown rendered preview).
- Prior test plans: `0002-editorconfig.md`, `0003-per-pane-tabs.md`.
- ADRs: none directly.
