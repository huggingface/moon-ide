# LSP

Status: partial — TypeScript diagnostics + hover + completion + goto-definition + nav history shipped across the stage-1 and stage-2 slices of Phase 4. Every other language (Rust, Svelte, CSS, HTML, JSON) is architecturally in scope and not yet wired.

## The non-negotiable invariant

LSP lives in `moon-core`. Nothing in the UI speaks LSP JSON-RPC directly. The frontend sees **moon-shaped** types (`LspDiagnostic`, `LspHover`, `LspCompletionList`, `LspLocation`, `LspStatusEvent` — see `crates/moon-protocol/src/lsp.rs`) and Tauri commands (`lsp_open` / `lsp_update` / `lsp_close` / `lsp_hover` / `lsp_completion` / `lsp_definition`) that forward to the broker.

This mirrors the Phase 5 git layer's discipline: one translation wall between upstream protocol types and the UI, one place to fix things when either side moves.

## Layers

```
┌─────────────────────────────────┐
│ Svelte UI                       │
│ (Editor.svelte, StatusBar,     │  ← reads LspDiagnostic[], LspHover,
│  state.svelte.ts)              │    listens to lsp:diagnostics + lsp:status
└───────────┬─────────────────────┘
            │ Tauri commands / events (moon-shaped types)
┌───────────▼─────────────────────┐
│ src-tauri/commands/lsp.rs       │
│ AppState.lsp: Option<LspHandle> │  ← lazy broker, lifecycle tied to
│                                 │    active folder
└───────────┬─────────────────────┘
            │ moon-core public API
┌───────────▼─────────────────────┐
│ moon-core::lsp::LspBroker       │  ← per-workspace, keyed on root path
│  ├ spec_for(language_id)        │  ← wire-in table (TS today, more soon)
│  └ LspServer ×N                 │  ← one child process per language id
│     ├ LspClient                 │  ← JSON-RPC over framed stdio
│     │  ├ framing                │  ← `Content-Length: N\r\n\r\n{json}`
│     │  └ actor tasks            │  ← reader + writer on tokio channels
│     └ translate::*              │  ← lsp_types ↔ moon_protocol::lsp
└───────────┬─────────────────────┘
            │ LSP JSON-RPC over stdin/stdout
┌───────────▼─────────────────────┐
│ tsgo --lsp --stdio              │  ← child process; stderr → tracing::debug
│ (Microsoft TS 7 native port,    │
│  @typescript/native-preview)    │
└─────────────────────────────────┘
```

## Decisions

### Custom thin client, not `tower-lsp`

`tower-lsp` is a **server** framework. We're an LSP _client_, which it doesn't help with. We use the upstream `lsp-types` crate for message shapes (zero code, all value) and roll ~300 LOC of framing + actor-pattern client on top. Fewer crates, smaller binary, and we control the places we'd have to look when a race bites us later.

Resolves the corresponding open question in `architecture.md` (`tower-lsp` vs thinner roll-your-own).

### One process per `(workspace, language_id)`

Not per file, not global. `tsgo` (like `tsserver` before it) handles multi-tsconfig via internal project pools — firing up a server per file would defeat that cache and cost seconds of boot per open.

Workspace close (the Tauri ExitRequested hook) calls `LspBroker::shutdown_all` which sends `shutdown` + `exit`, waits up to 2s per server, and drops. `kill_on_drop(true)` on the child is the escape hatch so even a wedged server can't outlive the IDE.

### Lazy spawn

Nothing spawns until the first `lsp_open`. A workspace with zero TypeScript files pays zero LSP cost.

### Full-document sync (stage 1)

`initialize` doesn't advertise incremental sync, so every `didChange` carries the whole file. Simpler for now; typescript is fast on full-doc for buffers <100 KB which covers everyone's normal day. Incremental sync is a later optimisation when we have a repro of a slow file.

### Position encoding is UTF-16

LSP's default. CodeMirror's native string offsets are UTF-16 code units too (JS strings). No conversion happens in either direction; the one spot that could have disagreed (the `offsetFor(doc, line, char)` helper in `src/lib/editor/lsp.ts`) uses `doc.line(n).length`, which is UTF-16 as well.

### Active-folder is the broker root

Switching the active folder drops the broker and rebuilds. The frontend's `openFile` handles re-issuing `didOpen` naturally — tabs that survive the switch will re-open against the new broker the first time the editor re-renders them. Not atomic across the switch; the user sees a short "starting…" pill and diagnostics return when tsserver finishes its first pass.

### TypeScript server: `tsgo`, not `typescript-language-server`

We target Microsoft's native TS 7 port (`@typescript/native-preview`, binary name `tsgo`) rather than the community `typescript-language-server` wrapper. Rationale:

1. **Already installed.** `@typescript/native-preview` is in moon-ide's `devDependencies` for the `check:ts` script. Discovery finds it in `node_modules/.bin/tsgo` without any extra setup, and every contributor gets LSP on their first `bun install`.
2. **Upstream alignment.** The `typescript-language-server` README states it expects to be superseded by TS 7 + tsgo. Adopting the native port now avoids a migration later.
3. **No Node runtime.** `tsgo` is a prebuilt native binary distributed via npm's optionalDependencies (one per platform). For future container-backed workspaces that don't otherwise need Node, this removes a dependency.
4. **Performance.** ~10× speed-up on the compile path, meaningful latency improvements on every LSP request. The whole stack (Rust host + Go language service + Tauri UI) is native.

Trade-off: tsgo is still a preview channel. The `API over LSP implementation` PR (`microsoft/typescript-go#2302`) is in draft; a few LSP features may be incomplete or behave differently from `typescript-language-server`. If we hit a gap that blocks us, the migration path is a **one-string change** in `moon-core/src/lsp/server.rs`'s `TS_SERVER` spec — our client code is wire-protocol-agnostic.

### Binary discovery: project-local first, then PATH

`moon-core::lsp::server::discover_binary` walks up from the broker's root looking for `<ancestor>/node_modules/.bin/<bin_name>` at every level, then falls back to `which::which(bin_name)`. Matches Node's own resolution algorithm — so a pnpm-hoisted monorepo (single top-level `node_modules`) works the same as a classic per-package layout.

The first match wins: a project-pinned copy always beats a global install. This lets a monorepo freeze a specific LSP version without affecting other projects on the same machine.

On Windows we look for `<bin>.cmd` rather than `<bin>` because that's how npm's `.bin` wrapper lands on that platform. Spawning the `.cmd` is a regular `CreateProcess` call; no special handling needed.

If nothing is found on disk, the broker caches a `NotAvailable` slot per language and emits `lsp:status { status: 'notavailable' }`. The status bar paints a quiet pill whose tooltip is the spec's `install_hint` field (e.g. `bun add -D @typescript/native-preview`) — copy-pasteable into a terminal.

Phase 2 (containers) will pre-install the server in `moon-base` so the container-backed story is pill-free without relying on the host's package manager.

### Client capabilities are minimal

We only advertise what's wired up (`hover`, `completion`, `publishDiagnostics`, synchronisation). Adding a capability is a localised change: flip the flag in `server::initialize`, add the command in `commands/lsp.rs`, add the CM adapter in `src/lib/editor/lsp.ts`.

### Go-to-definition is Ctrl/Cmd-click + a link-preview hover

The "hold modifier, mouse over identifier, see underline, click to jump" UX (from VS Code, Cursor, and every IDE the team already uses) is baked into the editor itself rather than a palette command. Lives in `src/lib/editor/lspGotoDefinition.ts`:

- A `ViewPlugin` tracks modifier state (`Ctrl` on Linux/Windows, `Cmd` on macOS) by listening on the window — not the editor — so focus changes during a modifier-hold don't drop the state.
- `mousemove` with the modifier held resolves the word under the cursor, calls `ipc.lsp.definition`, and paints an underline (`Decoration.mark` → `.cm-lsp-link`) if the server offers a target.
- Probes are cheap-cached: re-hovering inside the same word span is a no-op; each probe carries an `epoch` that's invalidated if the pointer moves on, so a slow LSP response never lands stale.
- `mouseup` with the modifier held re-calls `definition` (the earlier response may have been discarded) and routes through `workspace.jumpTo(path, position)`.
- **External targets** (paths outside the workspace root — `node_modules/`, TS built-in lib, etc.) come back with `path: ''` and `externalUri` populated. The UI surfaces a toast rather than silently failing. A read-only external-file viewer is a later deliverable.

Server-side capability advertisement is `definition: { linkSupport: true }`; we take `LocationLink`'s `targetSelectionRange` (the identifier) over `targetRange` (the whole body) so the caret lands on the name, not inside a function body.

### Navigation history (Alt+Left / Alt+Right)

Position-aware, browser-style history lives on `WorkspaceState`. Each entry carries enough to re-establish both the active buffer and the caret inside it — VS Code-style — and the entries are folder-tagged so a multi-folder workspace can walk back through files in folder B while folder A is active.

- `navStack: NavEntry[]` + `navIndex: number`. `NavEntry = { folder, path, line, character }`. `folder` is the absolute host path of the bound folder (the same value as `WorkspaceFolder.path`); `path` is folder-relative the way `openFiles` entries already are. The pair survives folder switches and tab close/re-open.
- Two mutation modes feed the stack:
  - **Push** on genuine navigations: a mouse click inside the editor (CM transaction annotated `select.pointer`), a file switch (`setActive`), or a `jumpTo` call truncate any forward entries and append. VS Code's behaviour: if you're at line 10 and click line 50, the entry at line 10 becomes the bookmark Alt+Left returns to.
  - **Tip update** on every other selection change: arrow keys, selection extension, the programmatic dispatch from our own `pendingJumps` consumer. These update `navStack[navIndex]` in place. Net result: when you go back and explore with arrow keys, Alt+Right returns you to where you actually ended up, not where the original click landed.
- `pushFileSwitchEntry` records a fresh `(folder, path)` at `(0, 0)` on tab/tree clicks and goto-def jumps; the first `updateNavTip` after the file renders corrects the position. It skips the push when `(folder, path)` already matches the tip, so rapid re-clicks on the same tab don't inflate history.
- `pushClickNavigation` always pushes a fresh entry unless the click lands on the exact same caret as the tip (a refocus gesture). Truncates the forward stack, matching browser semantics.
- Pushes during `navigateBack` / `navigateForward` / `jumpTo` are suppressed via a private `suppressNavPush` flag so stepping through history doesn't re-record itself.
- `canNavigateBack` / `canNavigateForward` are `$derived` so keybindings can fall through to CM's default when history is empty (on macOS, Option+Arrow is word-motion in the default keymap; we only shadow it when there's somewhere to go).

**Cross-folder restores.** `restoreNavEntry` awaits `setActiveFolder(entry.folder)` before `openFile(entry.path)` when the active folder differs. It also bails gracefully (with a flash) when the target folder was removed from the workspace since the entry was recorded; `removeFolder` prunes stale entries on the way out so this bail path is rare.

**Cross-folder goto-definition.** When `textDocument/definition` returns a target outside the active folder's root, the broker hands back an `externalUri`. The frontend `resolveExternalUri` walks the bound-folder list (longest-prefix first, so nested bindings beat their parent) and, if a bound folder contains the target, rewrites it to `{ folder, relative-path }` and calls `jumpTo(path, pos, side, folder)`. Only genuinely-external targets (node_modules, Rust toolchain, `ts://` pseudo URIs) keep surfacing as a toast.

### One-shot caret hand-off via `pendingJumps`

Goto-definition and any "open file at specific position" caller (including `navigateBack` / `navigateForward`) set an entry in `WorkspaceState.pendingJumps: Map<"${folder}::${path}", { line, character }>` before calling `openFile`. The `Editor` component has a `$effect` that consumes the entry for the `(folder, path)` it's currently displaying, dispatches a selection-change + `scrollIntoView(…, 'center')`, and drops the entry.

Key includes the folder so folder A's `src/lib.rs` and folder B's `src/lib.rs` don't cross the streams. Microtask-deferred: the path-change effect's `setState` has to finish first, otherwise the selection dispatch lands in the outgoing view.

### Server → client requests get `null`

tsserver issues `workspace/configuration`, `window/workDoneProgress/create`, and `client/registerCapability` during initialisation. We respond `null` to all of them; tsserver treats that as "no config / OK / nothing to do" and continues. When we need to answer (e.g. the future linting rule config for rust-analyzer), a server-request-handler slot lives in `client.rs` and we can special-case methods individually.

## Events

Two Tauri events, both keyed by language-agnostic payloads so the UI doesn't need a per-language dispatch:

- **`lsp:diagnostics`** — `LspDiagnosticsEvent { path, diagnostics: [] }`. Full replacement. Every `textDocument/publishDiagnostics` notification from any running server becomes one of these.
- **`lsp:status`** — `LspStatusEvent { languageId, status, detail? }`. Emitted on every server-state transition (spawn attempt, initialise success, crash, shutdown). The UI caches the latest per language id and only renders the pill when the status is anything other than `running`.

## Frontend architecture

- **`src/lib/state.svelte.ts`** is the single source of LSP state on the frontend:
  - `diagnostics: Map<path, LspDiagnostic[]>` — populated by the `lsp:diagnostics` listener.
  - `lspStatuses: Map<language_id, LspStatusEvent>` — populated by the `lsp:status` listener.
  - `lspOpen(path, text)` / `lspScheduleUpdate(path, text)` (150ms debounce) / `lspClose(path)` wrap the three lifecycle calls and no-op on file types without a server.
- **`src/lib/editor/lsp.ts`** is the CodeMirror adapter surface:
  - `filePathFacet` — current buffer path, read by every adapter.
  - `lspDiagnosticsExtension()` — just `lintGutter()`; the actual diagnostics come in via `applyDiagnostics(view, list)` which `Editor.svelte` calls from a reactive `$effect`.
  - `lspHoverExtension()` — CM `hoverTooltip` delegating to `ipc.lsp.hover`. Rendered inside `.cm-lsp-hover` which the editor chrome theme styles.
  - `lspCompletionSource` — CM `CompletionSource` registered as an `override` on the editor's `autocompletion` extension. Never auto-opens; Ctrl-Space or a panel re-query triggers it.
- **`src/lib/editor/lspGotoDefinition.ts`** — Ctrl/Cmd-hover link preview + Ctrl/Cmd-click jump. Takes a `{ jumpTo, flash }` callback bag so it doesn't import `state.svelte.ts`. `Editor.svelte` wires the real workspace methods through.
- **`src/lib/editor/lspLanguage.ts`** — path → LSP language-id mapping. Also the feature-flag: returning `null` means "no LSP here", so adding Rust is literally one entry plus a wire-in on the backend.

## Non-goals of stage 1

- ~~Go-to-definition~~ — shipped in stage 2 (this section). Find-references, rename, code actions — stage 4+.
- Workspace symbols. Requires a separate UI surface (palette split).
- Signature help, selection range, semantic tokens — nice, not needed.
- Snippets in completion — `snippet_support: false` in client capabilities. Turning it on requires a snippet-renderer extension on the frontend.
- Progress reporting (the `window/workDoneProgress` dance). tsserver uses it to report indexing; we ignore it for now and the status bar pill covers the "is it ready?" gap.
- Incremental document sync. Full-body on every change. Revisit when a real buffer is actually too big.
- Crash recovery beyond the broker's cache. A crashed server stays crashed until workspace close; the `lsp:status` event makes that visible.

## When to update this doc

- Adding a new language server: add a `LspBinarySpec` constant, extend `LspBroker::spec_for`, mirror the language-id in `src/lib/editor/lspLanguage.ts`, and note it here. Add a test plan.
- Adding a new capability (go-to-def, rename, etc.): extend `moon-protocol::lsp`, add a Tauri command, add a CM adapter, advertise the capability in `server::initialize`, and document it here. Add a test plan.
- Changing the frontend ↔ broker wire shape: bump `moon_protocol::PROTOCOL_VERSION`, update this doc, update the test plan for the affected surface.
