# Test plan 0004: markdown rendered preview

- **Date**: 2026-04-26
- **Phase**: Phase 1.5

## What shipped

- Markdown files now open in a rendered preview by default via a
  sandboxed markdown-it + DOMPurify pipeline (`html: false`,
  `linkify: false`, `rel="noopener noreferrer"` on every `<a>`).
- Tab-strip Source / Preview toggle (and matching palette
  command, hidden for non-markdown tabs) flips per-buffer;
  preview mode is stored on the buffer, not the pane.
- Link click handler is scheme-aware: `http(s)` / `mailto` /
  `tel` open in the OS default via Tauri's opener; in-page
  `#anchors` scroll natively; workspace-relative and root-
  absolute paths open in the editor after lexical + host-side
  validation; every other scheme is dropped.
- New optional `Command.visible` predicate gives the palette a
  way to hide context-dependent commands — used for the
  markdown toggle, reusable for later.

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
11. **Linked workspace files.** In `README.md`, click the link to
    `specs/roadmap.md` (or any other markdown file referenced by a
    relative path). Expected: a new tab opens for that file in the
    focused pane, defaulting to Preview if the target is markdown,
    and the file tree selects the new path. If the target lives in
    a collapsed directory, the tree expands every ancestor on the
    way down so the row is actually visible (the same applies to
    any other openFile flow — session restore, Save As, etc.).
    `[code](./src/App.svelte)` works the same way and opens the
    source in the code editor.
12. **Workspace-root-absolute links.** Edit a markdown file
    somewhere deep in the tree to include `[root](/README.md)` and
    click it. Expected: opens the workspace's `README.md`, not the
    filesystem root. Trailing `?query` and `#fragment` parts are
    stripped before resolution; the fragment is dropped (anchor-
    scroll inside a freshly-opened file is a follow-up).
13. **Escape attempt.** Edit a markdown file to include
    `[escape](../../../etc/passwd)` and click it. Expected: nothing
    happens — the lexical resolver rejects the link before any IPC
    call. Even if it didn't, the host would refuse on resolve.
14. **In-page anchors.** Click an in-page `[link](#anchor)` (or
    use any of the auto-generated heading anchors once we ship
    them). Expected: the article scrolls to the target without
    leaving the tab.
15. **Custom-scheme links.** A link with an unknown scheme
    (`steam://run/123`, `vscode://…`) does nothing.

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
- **Anchor-scroll inside a freshly-opened file is a follow-up.**
  Clicking `[other](./other.md#section)` opens `other.md` and
  drops the fragment; the renderer doesn't emit heading anchors
  yet either, so there's nothing to scroll to inside the same
  document beyond what the markdown source provides explicitly.
- **`file://` and custom-scheme links** are still swallowed —
  same posture as before.
- **Per-pane preview mode is not supported.** Same buffer in two
  panes shows the same mode. Splitting a markdown file with one
  pane in Source and one in Preview is on the follow-up list.

## Related

- Specs: `specs/roadmap.md` (Phase 1.5 — Markdown rendered preview).
- Prior test plans: `0002-editorconfig.md`, `0003-per-pane-tabs.md`.
- ADRs: none directly.
