# LSP

Status: partial — TypeScript diagnostics + hover + completion shipped as the stage-1 slice of Phase 4. Every other language (Rust, Svelte, CSS, HTML, JSON) is architecturally in scope and not yet wired.

## The non-negotiable invariant

LSP lives in `moon-core`. Nothing in the UI speaks LSP JSON-RPC directly. The frontend sees **moon-shaped** types (`LspDiagnostic`, `LspHover`, `LspCompletionList`, `LspStatusEvent` — see `crates/moon-protocol/src/lsp.rs`) and Tauri commands (`lsp_open` / `lsp_update` / `lsp_close` / `lsp_hover` / `lsp_completion`) that forward to the broker.

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
│ typescript-language-server      │  ← child process; stderr → tracing::debug
│   --stdio                       │
└─────────────────────────────────┘
```

## Decisions

### Custom thin client, not `tower-lsp`

`tower-lsp` is a **server** framework. We're an LSP _client_, which it doesn't help with. We use the upstream `lsp-types` crate for message shapes (zero code, all value) and roll ~300 LOC of framing + actor-pattern client on top. Fewer crates, smaller binary, and we control the places we'd have to look when a race bites us later.

Resolves the corresponding open question in `architecture.md` (`tower-lsp` vs thinner roll-your-own).

### One process per `(workspace, language_id)`

Not per file, not global. `typescript-language-server` handles multi-tsconfig via internal tsserver project pools — firing up a server per file would defeat that cache and cost seconds of boot per open.

Workspace close (the Tauri ExitRequested hook) calls `LspBroker::shutdown_all` which sends `shutdown` + `exit`, waits up to 2s per server, and drops. `kill_on_drop(true)` on the child is the escape hatch so even a wedged server can't outlive the IDE.

### Lazy spawn

Nothing spawns until the first `lsp_open`. A workspace with zero TypeScript files pays zero LSP cost.

### Full-document sync (stage 1)

`initialize` doesn't advertise incremental sync, so every `didChange` carries the whole file. Simpler for now; typescript is fast on full-doc for buffers <100 KB which covers everyone's normal day. Incremental sync is a later optimisation when we have a repro of a slow file.

### Position encoding is UTF-16

LSP's default. CodeMirror's native string offsets are UTF-16 code units too (JS strings). No conversion happens in either direction; the one spot that could have disagreed (the `offsetFor(doc, line, char)` helper in `src/lib/editor/lsp.ts`) uses `doc.line(n).length`, which is UTF-16 as well.

### Active-folder is the broker root

Switching the active folder drops the broker and rebuilds. The frontend's `openFile` handles re-issuing `didOpen` naturally — tabs that survive the switch will re-open against the new broker the first time the editor re-renders them. Not atomic across the switch; the user sees a short "starting…" pill and diagnostics return when tsserver finishes its first pass.

### Binary discovery: PATH lookup only

`which::which("typescript-language-server")`. If it's missing, the broker caches a `NotAvailable` slot per language and emits `lsp:status { status: 'notavailable' }`. The status bar paints a quiet pill. We don't bundle a binary or auto-install — that's an unsolved trust question and `npm i -g typescript-language-server` is a one-liner. Phase 2 (containers) pre-installs it in `moon-base` so the container-backed story is pill-free.

### Client capabilities are minimal

We only advertise what's wired up (`hover`, `completion`, `publishDiagnostics`, synchronisation). Adding a capability is a localised change: flip the flag in `server::initialize`, add the command in `commands/lsp.rs`, add the CM adapter in `src/lib/editor/lsp.ts`.

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
- **`src/lib/editor/lspLanguage.ts`** — path → LSP language-id mapping. Also the feature-flag: returning `null` means "no LSP here", so adding Rust is literally one entry plus a wire-in on the backend.

## Non-goals of stage 1

- Go-to-definition, find-references, rename, code actions — stage 4+.
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
