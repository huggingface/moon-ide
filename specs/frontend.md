# Frontend

STATUS: partial — Phase 0 ships file tree + single editor + tabs scaffolding.

## Stack

- Svelte 5 (runes) + TypeScript
- Vite (Tauri's recommended bundler)
- CodeMirror 6 for the editor
- `@pierre/trees` (vanilla mode) for the file tree
- CSS variables for theming, no CSS framework
- No global CSS reset beyond a tiny `:root` defaults block

## Module layout

```
src/
├── App.svelte                  Root layout: sidebar | coder | editor area | files (optional) | status bar
├── main.ts                     Mount + global setup
├── styles.css                  CSS variables (theme tokens) + a small reset
├── lib/
│   ├── ipc.ts                  Typed wrappers around Tauri commands
│   ├── protocol.ts             TS types mirroring crates/moon-protocol
│   ├── state.svelte.ts         Workspace/editor state ($state runes)
│   ├── components/
│   │   ├── FileTree.svelte     Wraps @pierre/trees vanilla
│   │   ├── Editor.svelte       Wraps CodeMirror 6
│   │   ├── EditorTabs.svelte   Tab strip
│   │   ├── StatusBar.svelte
│   │   └── Sidebar.svelte
│   └── editor/
│       └── language.ts         Lezer/CM6 language extension picker per file
```

## State

Workspace and editor state lives in `state.svelte.ts` using Svelte 5 `$state`. Single shared store; components read it directly via `import { workspace } from '$lib/state.svelte'`.

```ts
type WorkspaceState = {
	rootPath: string | null;
	paths: string[]; // flat path list for Pierre Trees
	openFiles: OpenFile[];
	activeFileId: string | null;
};
```

Avoid Svelte stores (`writable`) — runes are simpler for app-level state. Use stores only for cross-cutting reactive primitives if we need them.

## IPC convention

Every Tauri command is wrapped in `lib/ipc.ts` with a typed function:

```ts
export const ipc = {
	fs: {
		readDir: (path: string) => invoke<DirEntry[]>('fs_read_dir', { path }),
		readFile: (path: string) => invoke<ReadFileResult>('fs_read_file', { path }),
		// ...
	},
};
```

Components never call `invoke` directly. This keeps the Rust/TS surface auditable in one file.

## Editor

CodeMirror 6 is configured in `Editor.svelte`. Language extensions are loaded per file (lazy via dynamic import). Phase 0 includes TS/JS, JSON, CSS, MD, HTML.

State per editor:

- doc + selection + scroll
- dirty flag
- file mtime at load (to detect external changes later)
- per-tab view-state snapshot (caret + anchor + `scrollTop` + serialized CM history) cached on `WorkspaceState.viewStateByKey`, captured right before a tab swap and replayed on return. Includes the undo history (`EditorState.toJSON({ history: historyField })`) so `Ctrl+Z` after `Alt+Left`-ing back to an unsaved buffer still walks through edits made before the user clicked away. Dropped when the buffer falls out of every pane (`closeFile`'s GC block), so a fresh open from the tree starts with an empty undo stack.

Saves go through `ipc.fs.writeFile`. The editor only tracks dirty state; persistence is owned by the workspace state module.

### Extensions baseline

`baseExtensions()` in `Editor.svelte` is the single source of truth for what every editor instance gets. It's deliberately explicit rather than pulled from `codemirror`'s kitchen-sink `basicSetup` — we want to know exactly which behaviours are on:

- **Line numbers + active line/gutter** — structural.
- **`bracketMatching` + `closeBrackets`** — matches the paren/bracket/brace under the caret, and auto-inserts the matching close glyph on opener input. `closeBracketsKeymap` is merged into the keymap so backspace on an empty pair deletes both sides.
- **`indentOnInput` + `history`** — language-aware reindent after tokens and full undo/redo.
- **`highlightSelectionMatches`** — highlights other occurrences of the current selection in the buffer (matches VSCode's "selection highlights").
- **`searchAsYouType()`** (`editor/searchAsYouType.ts`) — the Ctrl-F find panel. We replace the stock `@codemirror/search` panel (which only commits the query on Enter) with a custom `createPanel` that keeps the same DOM + feature set (case / regexp / whole-word toggles, replace fields, next/prev/all buttons) but jumps the selection to the first match on every keystroke, like the browser find bar. The first-match search anchors to the caret position captured when the panel opens, so refining the query stays on the same match instead of marching the caret forward. Enter / Shift+Enter still step to the next / previous match from the current selection. This extension must be present (not just `searchKeymap`) so `openSearchPanel` uses our panel rather than auto-enabling the default `search()`. Wired into all three editor surfaces (regular editor, working-tree diff, review sections).
- **`autocompletion({ activateOnTyping: false })`** — completion surface only; no source is registered yet. Opens explicitly on Ctrl-Space or when a future LSP source dispatches one. We keep the popover off the typing path because the built-in identifier-from-document source is too noisy to show unprompted, and the real sources (LSP, snippets) arrive in a later phase.
- **`foldGutter`** — code folding driven by each language's Lezer grammar (`languageData.foldNodeProp`). Languages with a Lezer grammar (TS/JS, JSON, Rust, Go, Python, HTML/Svelte, Vue, CSS, Markdown, YAML) fold; legacy `StreamLanguage` modes (TOML, shell, Dockerfile, properties, ignore/dotenv, JSONL, Dart) have no fold info and render an empty marker column. Gutter sits immediately right of the line numbers; click the marker to fold/unfold. Fold state is in-memory per editor instance — we don't persist folds across file reopens. Wired into `DiffView` on both sides too: `@codemirror/merge` re-measures and rebalances its block spacers when a side's height changes (its update listener calls `updateSpacers` on `heightChanged`), so folds on one pane don't desynchronise the alignment — the other pane gains spacers at the next unchanged-region boundary. We deliberately skip CM's `foldKeymap` because `Ctrl-Alt-[` / `Ctrl-Alt-]` (foldAll / unfoldAll) shadow the AltGr-`[` / AltGr-`]` glyphs on French AZERTY and other AltGr-using layouts (Linux browsers report AltGr as `ctrlKey + altKey`, and CM's keyName-based match fires before any layout heuristic) — installing the keymap would mean a French-keyboard user couldn't type `[` without folding the entire file. Add a layout-stable keyboard binding (F-key, or a `Ctrl+K` leader sequence) if one becomes a real ask.
- **Tab / indent** — `indentWithTab` keyed, indent unit and tab size driven by the `editorConfigCompartment`.
- **Sticky scroll** (`editor/stickyScroll.ts`) — VS Code-style pinned header showing the chain of enclosing definitions (class → method → inner fn) of the first visible line, capped at 5 rows, one row per scope, clickable to jump to the definition. Scope detection reuses `enclosingStack` from `editor/enclosingScope.ts` (the same indent-walk heuristic the diff collapse bars fall back to — never wrong, occasionally absent; see [ADR 0038](decisions/0038-sticky-scroll.md)). Rows render the real line text through the live syntax tree + active highlight style, so colors and column alignment match the document. Installed on the regular editor and both `DiffView` panes; not on review-tab sections (their editors don't scroll — the outer container does, and the section already has a sticky file header).

Anything not in that list is intentionally off. Notably **no** rectangular selection, **no** rainbow brackets (deferred — wants a Lezer-aware scan to skip strings/comments; will land as a small standalone extension when it's someone's actual itch).

### Per-file language extensions

`languageFor(path, firstLine?)` in `editor/language.ts` is the single entry point that picks a CodeMirror language extension for a buffer. It maps filename → extension (`.ts` → `javascript({ typescript: true })`, `.rs` → `rust()`, …), with filename-fallbacks for extensionless files the repo ships (`Cargo.lock` → toml, `Dockerfile.*` → dockerfile) and shebang sniffing for the rest. Diff and review editors share the same resolver so a file highlights identically in every pane. Fenced ```dart code blocks in Markdown previews and LSP hover popovers highlight through the same grammar via `editor/highlightCode.ts`.

**GitHub Actions workflow files** (`.github/workflows/*.{yml,yaml}`) get a derived YAML grammar instead of the plain `yaml()`: the upstream Lezer YAML parser configured with a `parseMixed` wrapper that overlays the legacy shell stream-parser onto `run:` and `shell:` values. Both block scalars (`run: |` → `BlockLiteralContent`) and plain scalars (`run: echo x` → `Literal`) are overlaid; the wrapper climbs to the enclosing `Pair`, reads the `Key` text, and mounts the shell parser over the value's range only. Keys, `env:`, `with:`, and every other YAML node keep their original grammar — block-mapping folding and indentation survive untouched. Plain `.yml`/`.yaml` files outside `.github/workflows/` still get the standard `yaml()` extension. The overlay is a behaviour of the workflow grammar, not a separate file kind: LSP, git, blame, and editorconfig wiring are unchanged.

**Doc-comment tags** (`editor/doctags.ts`) get an emphasis layer on top of the grammar's flat comment styling. One flavor per language branch, each with its own "is this a doc comment" rule because the signal differs:

- **JSDoc** (JS/TS, and Svelte `<script>` bodies) — marker bytes: `/** … */` blocks get `@param` / `{@link Target}` (sigil + target, `linkcode`/`linkplain` included), plain `//` lines get only the `@ts-*` pragmas (allowlist, so `@handle` mentions stay plain). The nested `BlockComment` nodes the composite Svelte grammar produces are covered too.
- **rustdoc** (Rust) — `///` / `//!` / `/**` / `/*!` docs only: `# Section` headings (whole-line span) and `[Foo]` / `[`Bar`]` / `[crate::baz]` intra-doc links; ordinary `//` chatter stays plain.
- **godoc** (Go) — position rule, since Go has no doc marker bytes: a comment block counts when its last line directly precedes a declaration (walk `nextSibling` through comment siblings — gopls applies the same adjacency). Decorates `[Name]` / `[pkg.Name]` doc links (identifier-path inner only, so `[1, 2]` prose stays plain) and single-`#` headings.
- **pydoc** (Python) — docstrings are _string_ nodes in the first-statement position of a module/class/function body (PEP 257; `String` under the body's first `ExpressionStatement`). Decorates Sphinx fields (`:param x:`) and roles (`:func:`foo`` → sigil tag + payload link). Ordinary strings stay plain.

Implementation: a `ViewPlugin` that iterates **syntax-tree nodes** in the visible ranges and decorates tag / link / heading spans (`cm-doctag`, `cm-doctag-link`, `cm-doctag-heading`; colors via `--m-syntax-doctag` + `--m-syntax-link` + `--m-syntax-heading` so theme flips repaint for free). The tree gate keeps TS `@decorator` syntax, annotations, and emails in prose from lighting up. Registered per-language branch in `language.ts`, not globally. Rebuild is gated on doc/viewport/tree-generation change.

**Ctrl/Cmd-click jumps through doc-link targets with no extra wiring**: the goto-definition extension already probes any word under the held modifier (comment or not), and the servers answer — verified live: tsgo resolves `definition` on the target inside `{@link Widget.spin}` (the method, when the click lands on `spin`), rust-analyzer on `[Widget]` in a doc comment. Servers that refuse comment positions just return null → no underline, no jump — the gesture degrades to nothing rather than breaking.

**Dart** (`.dart`) has no upstream Lezer grammar (`@lezer/dart` doesn't exist), so it uses the legacy CM5 mode exported from `@codemirror/legacy-modes/mode/clike` rather than a `lang-*` package. That mode is the right call regardless: it handles Dart's `$x` / `${…}` string interpolation, `r'…'` raw strings, triple-quoted strings, and `@annotations`, which a keyword-and-string clike would mis-highlight. Like the other stream modes it has no fold info.

**Zig** (`.zig`) uses `codemirror-lang-zig` — the de-facto CM6 grammar (Jared Hughes, Zed team), generated from the zig-spec repository's `grammar.y`. No official `@codemirror/lang-zig` exists upstream. It's a proper Lezer parser with `styleTags` wired to the standard tag set, so the `--m-syntax-*` theme paints it with zero extra theme work; its `languageData` also registers keyword autocomplete and `//` comment tokens (which the comment-toggle keymap picks up for free). `build.zig.zon` (Zig's package manifest) stays plain text — no grammar for the ZON format exists and `languageFor` keys on the final extension. Highlighting-only: no ZLS / LSP wiring; add `zig` to `lspLanguageFor` + a broker spec when the team asks for more than colors.

### Diff and conflict surfaces

Different jobs, different tools:

- **Working-tree diff** (modified-file `View diff`, deleted files): `@codemirror/merge`'s `MergeView` — two CodeMirror editors side by side, sharing the regular editor's language / theme / editorconfig stack. Left side (`HEAD`) is read-only, right side (working tree) is editable so the user can fix things up directly inside the diff. `Ctrl+S` writes the right-hand buffer back through the normal `saveActive` path.
- **Unified layout** (working-tree diff, and read-only in the commit and pull-request tabs): `@codemirror/merge`'s `unifiedMergeView` — one editable editor on the working text with deleted lines shown inline (syntax-highlighted) and a per-chunk "Revert". Toggled by the **Unified** button next to Diff in the tab strip or `Toggle Unified / Side-by-side Diff`; the preference is app-wide and persisted (`AppState.diff_layout`). Panes narrower than 720px show unified whatever the preference, since two columns are unreadable there. Switching rebuilds the view and keeps caret + scroll via the view-state snapshot. Deleted lines are widgets, so they can't be selected and can't carry review comments (the working pane's comments are unaffected). Review and commit views stay side by side until there's a need.
- **Hide-whitespace toggle** (review tab + working-tree diff): `workspace.ignoreWhitespace`, **default-on** and persisted per folder (`FolderSession.ignore_whitespace`, same reasoning as `compare_baseline`); palette `Toggle Whitespace in Diffs`, banner pill in the review tab. When on, the `diffConfig.override` wrapper in `editor/diffWhitespace.ts` drops changes whose A/B texts differ only in whitespace — `git diff -w` semantics, string literals included — so line tints, chunk highlights, collapse placeholders, and chunk navigation all agree. Flips reconfigure the live MergeView in place (no remount; docs/folds/selections survive).
- **Pull-request context** in the review tab: when the branch has an open GitHub PR, a panel above the sections shows its description, state, review decision and checks, with moonbot's notes and the open review threads as a jump list; threads and line-bound notes also render under their lines in the diff, read-only. The publish dialog takes a verdict (comment / approve / request changes). See [ADR 0113](decisions/0113-pr-context-in-review-tab.md).
- **Evenness and collapsing** are shared by every diff surface ([ADR 0116](decisions/0116-even-diff-views.md)): split panes stay level around comment and thread cards, gutters are one width, colours come from the theme tokens, and unchanged runs collapse to a bar (`editor/diffCollapse.ts`) that names the enclosing definition and expands to that block (**Block above** / **Block below**), by 20 lines where no block is known, or entirely.
- **Read-only diffs** (commit detail, SCM review panel, PR hover): same `@codemirror/merge` engine, configured with both sides read-only. Avoids carrying two diff renderers around.
- **Review changes pseudo-tab** (`review://default-branch`, see Phase 5 §5.4): same `@codemirror/merge` engine, stacked one `MergeView` per changed file inside a single scrollable container; each section lazy-mounts on `IntersectionObserver` hit so opening a 100-file branch stays snappy. Long unchanged runs collapse to the bars described above. **Left side (base / `HEAD`) is read-only; right side (working tree) is editable** for everything except `deleted` rows, matching the working-tree-diff surface above. Edits route through `workspace.updateText` against a lazily-attached `OpenFile`; `Ctrl+S` from inside a section saves the underlying file via `saveReviewSection` (the global Ctrl+S handler delegates when the active tab is the synthetic `review://` buffer and a section has focus). Ctrl/Cmd-click goto-definition is wired on the right side with a lazy first-modifier-hover attach (the section calls `ensureBackingBuffer` only when the user is actually probing it, so broker traffic stays proportional to user intent); the jump itself routes through `workspace.jumpTo`, which opens the target as a regular editor tab in the same pane — exactly the "leave review mode and land on the function" behaviour. Hover / completion / diagnostics still aren't wired (they'd require eager `didOpen` on every visible section); click the section header's path to open the file as a regular tab when those are wanted.
- **Editable merge conflicts**: handled in the regular CM editor (not a MergeView) by the `editor/conflictMarkers.ts` extension. It decorates the `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` block in place and overlays an inline `Accept current` / `Accept incoming` / `Accept both` widget on the opening marker line. Same keybindings, theme, selection, undo stack as any other buffer — the conflict surface is just the regular editor with extra decoration. Gated on `gitStatusEntries[path].status === 'conflicted'` so files that happen to contain marker syntax (this very document, test fixtures) stay inert. The full conflict-resolution flow (SCM panel reshape, abort, auto-stage on save, commit-merge with soft-warn on residual markers) lives in Phase 5 §5.6.
- **Main editor buffer**: CodeMirror 6, as above.

The split matters because Pierre Diffs is display-only — forcing it to also edit would either duplicate CM's work or give us a second inferior editor. Keeping CM for every editable surface keeps "what works in the main editor" identical to "what works in the conflict editor".

### Markdown preview images

`MarkdownView.svelte` resolves relative `<img src>` in the rendered preview against the markdown file's own directory (`/foo.png` = workspace root, same rules as relative links) and rewrites them to asset-protocol URLs — the same streaming path the binary viewers use. The rewrite happens per file _after_ the shared render cache, which is keyed by source text only and must not leak resolved URLs across directories. Absolute `http(s)`/`data:` srcs pass through untouched; unresolvable ones (escape the root via `..`) stay as-is and render as broken images with their alt text. Untitled buffers skip resolution (no directory to resolve against).

### Binary file viewers

`util/fileKind.ts` maps a path to a `FileKind` (`text` / `image` / `pdf` / `video`); anything not recognised as a previewable binary is `text`. All binary kinds open as **read-only preview buffers**: `OpenFile.text` stays empty, the bytes never round-trip through the editor, and the buffer carries a `previewUrl` (`convertFileSrc(absolutePath)` — a Tauri asset-protocol URL that streams the on-disk bytes through the active host). All the `kind === 'text'` guards (LSP `didOpen`, blame, HEAD seed, format-on-save, Save As) naturally skip them. Save As is refused with a toast — copying bytes through the host is unimplemented until someone asks.

- **Images** (`ImageView.svelte`): an `<img src={previewUrl}>` on a checkerboard, footer reports natural dimensions. When the fs-watcher reports the image's own path changed, `WorkspaceState.refreshPreviewFile` re-resolves the bytes with a cache-busted URL (`?t=<previewToken>`) and bumps `previewToken`, which re-keys the `<img>` — the webview caches `asset:` responses per URL, so an unchanged URL would keep the stale render. The `null`-subset full sweep (window focus, palette refresh) deliberately skips binaries. `PdfView` re-renders on the same token. See [ADR 0051](decisions/0051-tab-rename-reveal-live-previews.md).
- **Videos** (`VideoView.svelte`): a native `<video controls src={previewUrl}>`, footer reports dimensions and duration. The asset protocol answers HTTP Range requests (206 / `Accept-Ranges`), so seeking streams from disk instead of buffering the file. Extensions are limited to containers the webviews can actually decode (`mp4` / `m4v` / `webm` / `mov`); playback still depends on platform codecs — WKWebView decodes H.264 natively, WebKitGTK needs the GStreamer plugins — so a decode failure shows an in-pane explanation rather than a black box. Same `previewToken` refresh as images. Note videos do **not** flow to the coder: there is no portable LLM wire format for video (see `coder.md` § Image attachments), so `read_file` on a video still errors as binary.
- **PDFs** (`PdfView.svelte`): rendered with **pdf.js** (`pdfjs-dist`), one `<canvas>` per page stacked in a scrollable column. We rasterise ourselves rather than embedding an `<iframe>`/`<embed>` because WebKitGTK — the Linux / container webview — ships no native PDF viewer, so a browser-native embed renders blank there. The library (~400 kB) and its worker load lazily on first PDF open (dynamic `import('pdfjs-dist')`) so they stay out of the main bundle. Bytes are fetched from `previewUrl` as an `ArrayBuffer` and handed to pdf.js as `{ data }` (simpler than teaching pdf.js's URL transport about the custom asset scheme; PDFs are small enough that one-shot loading is fine). Read-only — no text selection, search, or zoom controls yet; add them when the team asks.

**Out-of-workspace previews** (an agent's screenshot under `/tmp`, a fixture in `~/Downloads`) can't use the asset protocol: it resolves through the workspace-root-gated host and only ever reads the _host_ filesystem, so a path outside every bound folder is refused and a container-only path is invisible. Those load through `fs_read_preview_host` instead, which returns a `data:` URL — bytes read host-first, falling back to `docker exec cat` when the workspace shell container is running and the host has no such file. Capped at 25 MB (base64 in memory, versus streaming for the in-workspace path). Same read-only buffer otherwise, with `isExternal: true`. Extension→MIME lives in `commands/fs.rs::preview_mime` and must stay in step with `fileKindFor`'s `IMAGE_EXTS` / `VIDEO_EXTS`; a test pins the pairing.

### Browser tab

`browser://<id>` synthetic tabs (`BrowserView.svelte`): URL bar, reload, open-in-system-browser, and an `<iframe>` on the URL resolved by `browser_resolve_url` — unchanged for host URLs, a loopback tunnel into the workspace shell for container ones. The tab set is a mirror of the backend registry (`browser:tabs` event), which the coder's browser tools and the user's own actions both mutate. `browserBridge.ts` relays the coder's `browser:page_request`s into the iframe's injected page bridge over `postMessage` and reports in-page navigation back to the registry. Not persisted. See [ADR 0088](decisions/0088-in-ide-browser-tab.md).

### Inbox tab

`inbox://github` (`InboxView.svelte`; opened with `Ctrl+Shift+G`, the Inbox button at the top of the sidebar, or `Inbox: Open GitHub Inbox`) lists the pull requests and notification threads that want the user in the workspace's GitHub repositories. See [ADR 0112](decisions/0112-github-inbox.md) for the sources and why.

- **One list**, newest first, each item once. Toggle chips above it (Unread, Review requested, Watched, Mentioned, Mine, each with a count, and Snoozed when anything is) filter it: none selected shows every item that isn't snoozed, several show their union. The selection is not persisted.
- **Row**: unread dot, state icon (open / draft / merged / closed PR, issue, other), title, number, repository and author, tags for why it is listed (the notification reason for an unread item), then `+added −removed`, comment count, review decision, check rollup, author avatar, age.
- **Keys** (focus in the list): `j` / `k` or arrows move, `Enter` or a click opens a pull request or an issue of a bound repository in its own tab (anything else opens on GitHub); `Ctrl+Enter`, `Ctrl`-click or a middle click adds the tab without leaving the inbox, so several can be queued, `o` opens it on GitHub, `e` marks its thread read, `s` / `w` snooze it until 08:00 tomorrow / next Monday (or wake it), `r` refreshes, `/` focuses the filter, `1`–`5` toggle the chips and `0` clears them.
- **Snoozed** items leave the list and the button's counts until their time; the Snoozed chip shows them. Snoozes are local to the workspace; GitHub is not told, so the thread stays unread there. New activity does not wake an item early.
- **Signals**: a row of an open pull request also shows how many review threads are unresolved and how many minor notes moonbot left, read when the row is new or GitHub says it changed ([ADR 0119](decisions/0119-inbox-signals-and-groups.md)). The row's `+/−` leaves test files out once that read is in; the tooltip gives their share ([ADR 0123](decisions/0123-test-files-step-back-in-review.md)).
- **Group by topic** (`g`, or the button right of the chips): the open pull requests are sorted into named groups by one model call, and the list is shown under group headings (name, count, what the group has in common), newest group first; a heading folds its rows. Everything else (issues, notifications, closed pull requests, anything the model left out) comes last under "Not grouped". The chips and the filter apply as before.
  - A pull request stays in its group until it leaves the inbox. New ones are placed as they arrive, into the existing groups where they fit. **Regroup** sorts everything again.
  - Groups are kept per workspace. If the call fails (the coder isn't signed in, say) the list says so and stays as it was; nothing is retried until asked.
- **Filter** matches every typed word against title, repository, author, branch and `#number`.
- **Watching** lists the watched authors; add a login or `name[bot]`, remove with the chip's ×. The list is per workspace.
- The sidebar's Inbox button carries three counts: unread, waiting for review, and open from watched authors (snoozed items excluded).
- **Pinned.** Once open, the tab is the first tab of every folder's left strip until it is closed: it stays put while other tabs scroll, cannot be dragged, survives "Close others" / "Close all", and comes back at the next launch. Switching folders while looking at it keeps it in view. Closing it closes it everywhere.

### Pull request tab

`pr://<folder>#<number>` (`PrView.svelte`), opened from the inbox, shows a pull request of a bound project's repository without checking it out ([ADR 0115](decisions/0115-pr-tab-without-a-checkout.md)): the PR panel, then a read-only diff per changed file from the merge base to the PR head, with review threads and moonbot's notes under their lines. The header carries **Review** (verdict and summary), **Check out in a worktree** and **Start an agent on it**; the last two are disabled for a fork PR. Not restored at launch.

- **Belongs to the workspace, not to a folder** ([ADR 0121](decisions/0121-pr-tabs-belong-to-the-workspace.md)): it opens where the user is, whichever project the pull request is in, and is in every folder's tab strip from then on. Switching folders while looking at it keeps it in view; closing it closes it everywhere. A pull request of another project than the active one names that project in the banner. **Check out in a worktree**, **Start an agent on it** and a click on a file's name act in the pull request's project and make it the active one.
- **State controls** under the title ([ADR 0120](decisions/0120-pr-state-controls.md)): **Ready for review** on a draft; on an open pull request the merge button (with a method picker when the repository allows several), **Convert to draft** and **Close**; **Reopen** on a closed one. The merge asks once more before going ahead. When GitHub would refuse the merge the button is disabled and says why (review, required check, conflicts, branch behind). The same controls are in the Review changes tab of a branch with a pull request.
- **Mentions** (`@login`) in descriptions, comments and threads link to the user; the signed-in user's own are highlighted.
- **Emoji names** (`:robot:`) in the same text show as the emoji, from GitHub's list (`markdown-it-emoji`). Smileys (`:)`) stay as typed.
- **Attached images** of a private repository are shown: their addresses are traded for signed ones (`github_asset_urls`) as the text is painted.
- **Review** (banner): a verdict or a comment alone. Posting closes the card and re-reads the pull request; a failure stays in the card with what was written.
- **Ask the agent** ([ADR 0124](decisions/0124-review-agent-and-notes.md), [ADR 0126](decisions/0126-pr-agent-talked-to-from-its-tab.md)): **Explain** on a collapsed-lines bar asks about the block under it, **Explain** on a file's header about the file; the answer is a card under the line, with a box to ask back. A box under the conversation asks about the pull request, and **Ask** on a comment or a review thread about that; the answer is a comment under the question. The first ask starts the pull request's agent, a coder session in a checkout of its branch, without changing the active project or the coder panel's session; it is in the sessions list. One session answers everything, so it keeps what it has learned. Nothing goes to GitHub. Not offered on a fork's pull request.
- **Find** (Ctrl+F anywhere in the tab): searches every changed file, the new version whole and the old one where lines were removed, and steps through the hits file by file (Enter / Shift+Enter), opening a folded file and the collapsed lines a hit is in. The files' text is read once per head commit.
- **Avatars** beside comment authors: the user's GitHub picture, the inbox's for an app (which has none at the user address), else an initial.
- **Loads in stages**: the title from the tab at once, then description, conversation and checks, then review threads and the diff as each arrives. Coming back to the tab paints the last answer while it re-reads.
- **Review threads** in the PR panel list the open ones; "Show N resolved" adds the rest.
- **Conversation**: top-level comments and review summaries, oldest first, each a card of its own under the pull request's card (which holds what the author and the checks say) and above the diff. A long one is cut to a few lines with "Show all". Read-only; each links to GitHub.
- **Files** ([ADR 0118](decisions/0118-pr-files-one-at-a-time.md)): a list of the changed files beside the diffs (over them, from the banner's **Files** button, in a pane too narrow for both) with status, `+added −removed`, the count of unresolved threads and notes, and a tick once viewed; the file under the top of the pane is highlighted. Each file header has the same numbers, a collapse toggle and a **Viewed** box.
  - Marking a file viewed folds it and opens the next unviewed one. Marks last until the IDE closes or the PR's head moves.
  - With more than 8 files nothing starts open and going to a file closes the others. Deleted, binary, generated (lockfiles, snapshots, minified), very large and test files start collapsed in any PR.
  - A file is shown as in the file tree, in the list and on its diff's header (here, in a commit tab and in Review changes): the icon of its kind, its name in its git status colour, struck through when deleted. A header also says "new" / "deleted" in words. The status colours are the `--m-git-*` tokens, which the tree is fed too.
  - The list's total counts lines outside test files; a test file's own count is shown greyed ([ADR 0123](decisions/0123-test-files-step-back-in-review.md)).
  - `j` / `k` go to the next / previous file, `v` toggles viewed on the current one.

### Issue tab

`issue://<folder>#<number>` (`IssueView.svelte`), opened from the inbox, shows an issue of a bound project's repository, in a tab that belongs to the workspace like a pull request's ([ADR 0117](decisions/0117-issue-tab.md)): title, state, labels, author, assignees, description and comments in GitHub's markdown. Read-only, with refresh and "Open on GitHub". Not restored at launch.

### Editor context menu

Right-clicking inside the main `Editor` opens a small `ContextMenu.svelte` popover (the same component the tab strip and file tree use, portaled onto `document.body` so it isn't clipped by the editor's `overflow: hidden`). Opening the menu first moves the caret to the click position unless the click landed inside the current selection, so the entries target what the user right-clicked on. It carries **Rename symbol** (drives the same LSP F2 flow via `triggerRename`; greyed out for buffers with no LSP server, i.e. `lspLanguageFor(path) === null`) and **Copy repo link**, the latter operating on the lines under the current selection (or the caret line when nothing is selected). It routes through `ipc.fs.gitPermalink` → `WorkspaceHost::git_permalink`, which builds a permalink pinned to the current commit SHA (so the link is stable across later commits, matching GitHub's own "Copy permalink") — `blob/<HEAD-sha>/<path>#L<a>-L<b>` on GitHub, `src/commit/<HEAD-sha>/…` on Forgejo (see [ADR 0073](decisions/0073-forgejo-support.md)). The menu is suppressed for untitled / external / `review://` buffers — a permalink makes no sense there, so the platform menu shows instead.

The menu lives in `editor/editorContextMenu.ts` (`EditorContextMenu` class) so the diff view's **editable right-hand pane** mounts the same actions — only right-clicks inside that pane qualify (left-pane / HEAD clicks fall through to the platform menu), and Rename is forced off on deleted-file diffs (empty, LSP-less right pane). See [test plan 0091](test-plans/0091-editor-github-permalink.md).

### Tab strip and file-tree context menus

The tab strip's right-click menu (`EditorTabs.svelte`) carries **Rename…** (inline edit of the tab label — leaf name only, separators refused — routed through `WorkspaceState.renamePath`, which rebinds open buffers and now refuses while the affected buffer is dirty), **Reveal in file manager** (`fs_reveal_in_folder`), copy-path entries, and the close-family actions. The file tree's row menu (`FileTree.svelte`) has its own Pierre-driven inline **Rename**, the same **Reveal in file manager** entry, and the new-file / diff / discard / delete actions. Both mount the shared `ContextMenu.svelte` portaled onto `document.body`. See [ADR 0051](decisions/0051-tab-rename-reveal-live-previews.md).

Right-clicking a web link anywhere in the app (transcript, tool rows, pull request and issue text, cards in a diff, Markdown previews) opens one menu (`LinkMenu.svelte`): **Open in IDE browser**, **Open in system browser**, **Copy link**. It takes precedence over a surrounding surface's own menu. A control that stands in for a link takes part by carrying the address in `data-href`.

### LSP hook

Planned shape (not yet built): a per-file `ViewPlugin` subscribes to moon-core's LSP broker (one adapter per capability — hover, go-to-def, diagnostics, completions). The adapter translates LSP events into CM's `hoverTooltip`, `gotoDefinition` command, `setDiagnostics` effect, and `autocompletion` source respectively. Nothing goes to the UI that didn't come through moon-core — no direct LSP process ownership on the frontend.

Git blame is shaped the same way: a `ViewPlugin` that reads per-line blame from moon-core and decorates the gutter with a soft right-aligned annotation. Both ride the same infra that discard-changes already uses (workspace state → IPC → rust core → surface).

## File tree

The tree lives in the optional files panel at the far right of the window, closed by default ([ADR 0114](decisions/0114-sessions-in-the-sidebar.md)). `Ctrl+0`, the status bar's `files` entry, `Files: Show / Hide Panel` and turning on the source-control changes-only filter open it; the open state persists (`AppState.files_panel_visible`).

`@pierre/trees` `FileTree` class is instantiated in `FileTree.svelte`'s `onMount`. Mounted into a div via `tree.render({ containerWrapper })`. Selection and double-click are wired to `workspace.openFile(path)`. The component cleans up via `tree.cleanUp()` in `onDestroy`.

We don't wrap Pierre Trees behind an adapter — we use it directly. If we need to swap implementations later, this single component is the only place that changes.

The path list fed to Pierre comes from `WorkspaceHost::collect_paths` (one IPC, full recursive walk on the blocking pool). Two performance choices matter:

- **Backend prunes gitignored directories.** Before walking, `collect_paths` runs `git status --porcelain=v1 -z --ignored=matching` to learn which directories git would collapse to a single `!! dir/` row. The walk emits each such directory as a single collapsed entry and **does not enumerate its descendants**. On a moon-ide-sized repo this drops the path count from ~127k to ~25k — the bulk of the savings is `node_modules/` and `target/`. Non-repo folders see an empty skip set and walk everything (no authoritative ignore source to consult).
- **Backend depth cap with lazy frontier.** `collect_paths` recurses to `MAX_TREE_DEPTH=16` (frontend constant). Directories whose recursion the cap stops short of return alongside the path set in `CollectPathsResult.depth_capped`; the frontend feeds them into the same lazy-load bucket as gitignored directories. 16 covers every realistic project (deepest SvelteKit `[param]/` route stacks, monorepo `packages/<scope>/<name>/src/**`); the lazy frontier is defence-in-depth for genuinely deeper trees we haven't seen yet. Empty leaf directories at the cap aren't marked lazy — there's nothing to fetch on expansion.
- **Lazy descendant fetch.** When the user expands a collapsed-ignored or depth-capped directory in the file tree, `FileTree.svelte` issues `fs_collect_paths_under(rel, max_depth=0)` to fetch its direct children and batch-adds them to Pierre. The same response carries a `depth_capped` list: only those direct children get re-flagged as lazy, so drilling deeper into `node_modules/foo/bar/` re-issues the command at the next level while empty leaves stay non-lazy. Loaded paths are tracked separately from `workspace.paths` and re-unioned into the merged path set on every refresh so the path-set effect's `applyPathsDiff` doesn't churn them. Folder switches reset the lazy bucket; same-folder watcher refreshes preserve it.
- **`resetPaths` over `applyPathsDiff` for wholesale changes.** Pierre's `tree.batch([{type:'add', path}, …])` runs an event-per-op emit + child-index update; on 80k+ paths that's seconds of main-thread time. `resetPaths` rebuilds the path store in one shot. `FileTree.svelte` picks `resetPaths` whenever the active folder changes, when the previous snapshot was empty and the next isn't, and on first mount; otherwise small steady-state diffs (save, delete, watcher event) use the cheaper `applyPathsDiff`. See [test plan 0076](test-plans/0076-folder-switch-perf.md).

## Theming

CSS custom properties only. The only stylesheet-global signal is the `.light` class on `:root`: present → light palette, absent → dark palette. Pierre Trees is themed via the same custom properties (it reads CSS vars from the host).

The user picks one of three modes in the status-bar theme picker — **System**, **Dark**, **Light** — persisted in `AppState.theme` (see `crates/moon-protocol/src/theme.rs`). System is the default for fresh installs.

- The picked mode is stored verbatim. The painted palette is the _resolved_ `WorkspaceState.effectiveTheme`, which is `dark` / `light` when the user picked one of those and the OS preference when they picked `System`.
- The OS preference is resolved by the desktop shell via the `system_theme` Tauri command. On Linux / BSD the command talks to the XDG Desktop Portal (`org.freedesktop.appearance color-scheme`) through `ashpd`, which is the same channel Firefox and Chromium listen on; on macOS / Windows it forwards `tauri::WebviewWindow::theme()`. The detour matters because in a WebKitGTK webview both `matchMedia('(prefers-color-scheme: dark)')` and Tauri's own `getCurrentWindow().theme()` ignore the GTK / GNOME / KDE preference and default to light, so System mode flipped to light on every launch for users on a dark desktop. `restoreAppState` awaits the probe before the first `applyTheme` call so the UI doesn't flash. On Linux the shell also pushes the portal value into GTK as the app theme (an unset preference counts as dark), which is what WebKitGTK maps onto `prefers-color-scheme`, so pages in the IDE's browser tabs follow the desktop's dark / light setting. It's the system value, not the IDE's own theme choice.
- Live OS-theme flips go through the same platform split. On Linux a tokio task in the desktop shell subscribes to `Settings.receive_color_scheme_changed` via ashpd and re-broadcasts each change as the `system:theme-changed` Tauri event, which the frontend `listen`s for. On macOS / Windows `getCurrentWindow().onThemeChanged` fires directly on the webview, so the Linux watcher compiles to a no-op there. `matchMedia` is left wired as a last-resort fallback for non-Tauri dev shells (vite-only).
- Surfaces that can't read CSS variables (CodeMirror's `dark: boolean` build-time flag, xterm.js's option-bag palette) have their own `$effect` blocks keyed on `effectiveTheme` and reconfigure when it flips.

### Shape, spacing and type tokens

`--m-radius-{sm,md,pill}`, `--m-space-{1..4}` (4 / 8 / 12 / 16 px) and `--m-text-{xs,sm,md,lg}` (11 / 12 / 13 / 15 px) are the scale for new components, and `Pill.svelte` is the one pill. They arrived with the inbox tab; older components still carry literal values and move over as they are touched, not in a sweep. `--m-text-prose` (14 px) is the coder transcript's reading size; its column is capped at 860 px and centred, with the composer on the same column. Icons are SVG components under `components/icons/`; a few Unicode glyphs remain (context-menu check marks, arrows in labels).

### Workspace colour scheme

Each workspace's badge colour (`WorkspaceMeta.color`, else the hash-derived hue) drives a full per-workspace colour scheme so multiple open windows are distinguishable at a glance — see [ADR 0047](decisions/0047-workspace-chrome-tint.md). At hydrate, JS derives the whole `--m-*` surface/border/text/accent token set from the colour's hue and writes it on `:root` (dark values on the plain names, light on `*-light` twins; `:root.light` re-points the plain names at the twins, so theme flips stay pure CSS). Surfaces get the hue at low saturation, text stays neutral, and `--m-accent` carries the hue at full voice so every existing accent consumer follows for free. Warning re-pins to amber unless the workspace hue is amber itself. Syntax highlighting (`--m-syntax-*`) and ANSI terminal colours are deliberately untouched.

## Window state

Window size, position, maximized, and fullscreen state are persisted by the official `tauri-plugin-window-state` plugin. It writes its own JSON next to `state.json` and hooks window creation / close automatically — we don't roll our own because it handles monitor placement and DPI edge cases we'd rather not reimplement. That means this slice of UI state lives outside `AppState`; everything else (theme, last session, bottom panel) stays in `state.json`.

## Keymap

Cm6 default keymap + a small layer of app-level shortcuts wired in `App.svelte`:

- `Cmd/Ctrl+S` — save active file
- `Cmd/Ctrl+P` — quick open (Phase 1)
- `Cmd/Ctrl+Shift+P` — command palette (Phase 1)
- `Cmd/Ctrl+L` — show / hide the coder panel (or attach the selection to its composer)
- `Cmd/Ctrl+Shift+G` — GitHub inbox
- `Cmd/Ctrl+0` — open the files panel and focus the tree
- `F6` / `Shift+F6` — cycle focus: sidebar (sessions search), editor panes, bottom panel, files panel when open, status bar

Linux uses `Ctrl`; macOS `Cmd`. We test for `navigator.platform`.
