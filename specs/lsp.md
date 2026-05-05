# LSP

Status: partial — TypeScript + Rust both have diagnostics + hover + completion + goto-definition + nav history wired via the stage-1/stage-2 slices of Phase 4. Both additionally route **inside the workspace container** when one is up and the binary is reachable there (see [Container-backed LSP](#container-backed-lsp)); the broker falls back to host LSP per-language when it isn't. Every other language (Svelte, CSS, HTML, JSON) is architecturally in scope and not yet wired.

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

### Rust server: `rust-analyzer`

The ecosystem-standard LSP. No per-project install exists for Rust LSPs (unlike `tsgo`), so we rely on the system toolchain. Install on the developer's host:

```
rustup component add rust-analyzer
```

This drops `rust-analyzer` at `$CARGO_HOME/bin/rust-analyzer` (typically `~/.cargo/bin/rust-analyzer`). A `cargo install rust-analyzer` build or a distro package manager install both land on `$PATH`; both shapes resolve without extra work.

No startup args: `rust-analyzer` defaults to stdio + LSP when invoked with no flags, which is exactly what we want. It auto-detects workspace layout from `initialize.workspaceFolders`, so the generic init we already send suffices. Advanced configuration (`checkOnSave`, `cargo.features`, proc-macro toggles, etc.) is left at defaults for now — we'll add `workspace/didChangeConfiguration` plumbing when a real need surfaces.

### TypeScript server: `tsgo`, not `typescript-language-server`

We target Microsoft's native TS 7 port (`@typescript/native-preview`, binary name `tsgo`) rather than the community `typescript-language-server` wrapper. Rationale:

1. **Already installed.** `@typescript/native-preview` is in moon-ide's `devDependencies` for the `check:ts` script. Discovery finds it in `node_modules/.bin/tsgo` without any extra setup, and every contributor gets LSP on their first `bun install`.
2. **Upstream alignment.** The `typescript-language-server` README states it expects to be superseded by TS 7 + tsgo. Adopting the native port now avoids a migration later.
3. **No Node runtime.** `tsgo` is a prebuilt native binary distributed via npm's optionalDependencies (one per platform). For future container-backed workspaces that don't otherwise need Node, this removes a dependency.
4. **Performance.** ~10× speed-up on the compile path, meaningful latency improvements on every LSP request. The whole stack (Rust host + Go language service + Tauri UI) is native.

Trade-off: tsgo is still a preview channel. The `API over LSP implementation` PR (`microsoft/typescript-go#2302`) is in draft; a few LSP features may be incomplete or behave differently from `typescript-language-server`. If we hit a gap that blocks us, the migration path is a **one-string change** in `moon-core/src/lsp/server.rs`'s `TS_SERVER` spec — our client code is wire-protocol-agnostic.

### Binary discovery: ecosystem-idiomatic first, then PATH

Discovery is per-language via `LspBinarySpec::discovery`:

- **`DiscoveryStrategy::NodeModules`** (TS / JS): walks up from the broker's root looking for `<ancestor>/node_modules/.bin/<bin_name>` at every level, then falls back to `which::which(bin_name)`. Matches Node's own resolution algorithm — pnpm-hoisted monorepos (single top-level `node_modules`) work identically to classic per-package layouts. The first match wins: a project-pinned copy always beats a global install, letting a monorepo freeze a specific LSP version without affecting other projects on the same machine.
- **`DiscoveryStrategy::CargoHome`** (Rust): checks `$CARGO_HOME/bin/<bin_name>` (fallback `$HOME/.cargo/bin/<bin_name>`, or `$USERPROFILE/.cargo/bin/` on Windows), then `$PATH`. Covers `rustup component add rust-analyzer` — the default install location isn't always on a GUI-launched process's inherited `PATH`, especially on macOS and some Linux desktop environments.

On Windows we adjust the filename per strategy: `<bin>.cmd` for the Node case (npm's `.bin` wrapper), `<bin>.exe` for Cargo (native executables).

If nothing is found on disk, the broker caches a `NotAvailable` slot per language and emits `lsp:status { status: 'notavailable' }`. The status bar paints a quiet pill whose tooltip is the spec's `install_hint` field (e.g. `bun add -D @typescript/native-preview` or `rustup component add rust-analyzer`) — copy-pasteable into a terminal.

Container-backed workspaces (ADR 0008) skip host discovery entirely for languages whose server the container already ships — see [Container-backed LSP](#container-backed-lsp). Rust is the first (and currently only) such language: `moon-base` pre-installs `rust-analyzer` via `rustup component add`, and the broker pipes stdio through `docker exec` when the container is `Running`. Falling back to the host is automatic when the container is down, not configured, or doesn't have the server.

### Container-backed LSP

When the workspace shell container is `Running`, the broker spawns its language servers **inside** the container via `docker exec` rather than on the host. That makes the server see the same filesystem the build commands see (`/workspace/<basename>/...`) and removes the "have you installed rust-analyzer locally?" step from a new contributor's first hour.

**Routing table.** The Tauri layer picks a **primary** target per-broker at construction time (`ensure_broker` in `src-tauri/src/commands/lsp.rs`). The broker itself keeps a **host fallback** whenever the primary is `DockerExec`, and retries on it per-language if the primary can't resolve / probe / spawn that server's binary:

| Workspace shape      | Container state                    | Primary   | Per-server outcome inside the broker                                                                       |
| -------------------- | ---------------------------------- | --------- | ---------------------------------------------------------------------------------------------------------- |
| No container config  | n/a                                | Host      | Host LSP (no fallback needed).                                                                             |
| Container configured | Absent / Stopped / Paused / Failed | Host      | Host LSP (no fallback needed).                                                                             |
| Container configured | Running                            | Container | Container LSP when the binary is reachable + `--version` probe succeeds; else host fallback automatically. |
| Container configured | Running                            | Container | If both container AND host fallback lack the binary, `NotAvailable` pill with the spec's install hint.     |

The per-server fallback covers two separate cases without a user setting:

1. **Custom image dropped the server** (e.g. `moon-base` rebuilt from a fork that removed `rust-analyzer`) — container probe fails → broker retries on host, with a `tracing::info!` breadcrumb.
2. **Binary isn't reachable inside the container at all** — most commonly `tsgo`, whose real binary sits in `node_modules/.bin/tsgo` and is a Linux-specific shim installed by `bun install`. When the host `node_modules` sits **inside** the bind mount (the normal case for moon-ide itself), container-side LSP works; when it's hoisted to a parent of the active folder (some pnpm monorepos), the path is outside the mount and the broker falls back to host for that language.

In-container binary-path resolution lives in `moon_core::lsp::server::container_binary_path`. It walks host ancestors for `NodeModules`-strategy specs and translates matches through the `HostMount` translator; for `CargoHome`-strategy specs it hands back the basename because `moon-base` installs `rust-analyzer` on the container's `$PATH` via rustup.

**Pieces.**

- `moon_core::lsp::LspSpawner` (in `spawn.rs`) is the ADT: `Local` runs `Command::new(bin)`; `DockerExec { container_name }` wraps it as `docker exec -i <container> <bin> <args...>`. `-i` (no `-t`) is critical — LSP framing is raw bytes over stdio, a TTY would mangle them.
- `moon_core::lsp::server::PathTranslator` bridges the two filesystem views. `Identity` is a no-op; `HostMount { host_root, server_root }` rewrites paths in both directions so the server sees `/workspace/<basename>` URIs while the UI and tree stay in host absolute-path land. Every URI that crosses the boundary (initialize's rootUri, didOpen, diagnostics, goto-def) goes through the translator.
- `LspBroker::new_with_spawner(root, spawner, translator)` is what Tauri calls. `LspBroker::new(root)` still works — it's the host-only helper used by tests. Constructing with a `DockerExec` spawner auto-populates the host fallback; constructing with `Local` leaves it empty.
- `LspSpawner::probe(bin)` runs `<bin> --version` via the same build-command pipeline that the real spawn uses, and reports whether it exited cleanly. The broker calls it per-server on each route it tries; the first success wins. Cached outcome lives in the existing per-language `ServerSlot` map.
- `TerminalTarget::container_cwd_for_folder` (in `moon-terminal`) is the single place that defines the in-container mount convention (`/workspace/<basename>`). `ensure_broker` reuses it so terminals and LSP never drift.

**Teardown on container transitions.** Every mutating container command (`container_setup`, `container_stop`, `container_pause`, `container_resume`, `container_rebuild`, `container_teardown`, `container_apply_bound_folders`) calls `reset_lsp_broker` after the compose action completes. That drops the current broker; the next `lsp_open` rebuilds against whatever state the container is in now. Cheaper and more deterministic than trying to mutate the broker in place.

**Image responsibility.** `moon-base` pre-installs language servers the broker knows how to route in a container. Today that's `rust-analyzer` via `rustup component add`. Python and the others land here when each language gets wired; see [`containers.md`](containers.md#the-moon-base-image) for the current tool inventory.

**Known non-goals for this slice.**

- Python / Svelte / CSS / HTML / JSON — wire up when the language itself ships in the broker. Routing through the container follows the same template at that point.
- Remote (SSH / Codespaces) LSP — `DockerExec` doesn't generalise; a future `RemoteHost` spawner is its own design.
- Containers the user starts outside moon-ide — we only know how to route into the workspace's own compose project.

### Client capabilities are minimal

We only advertise what's wired up (`hover`, `completion`, push + pull diagnostics, synchronisation). Adding a capability is a localised change: flip the flag in `server::initialize`, add the command in `commands/lsp.rs`, add the CM adapter in `src/lib/editor/lsp.ts`.

### Diagnostics: push and pull, both feed one event

LSP 3.17 carries two diagnostic delivery modes and we support both:

- **Push** (`textDocument/publishDiagnostics`): the server fires unsolicited notifications whenever its analysis finishes. `rust-analyzer` and `typescript-language-server` use this. The notification pump in `lsp::server` translates the URI to a workspace-relative path and forwards a `LspServerEvent::Diagnostics` to the broker's broadcast channel.
- **Pull** (`textDocument/diagnostic` request → `DocumentDiagnosticReport`): the client asks for the current report after every `didOpen` / `didChange`. We do this fire-and-forget from `LspServer::open` / `update`, on a detached task so the notification path doesn't wait. `tsgo` (`@typescript/native-preview`) [explicitly does not implement push diagnostics](https://github.com/microsoft/typescript-go/issues/2362) and only answers pull requests, which is the whole reason we wired this in.

Servers that implement only one of the two are handled symmetrically: a push-only server returns `MethodNotFound` (-32601) for the pull request and we drop the error at debug; a pull-only server simply never sends `publishDiagnostics`. Both paths feed the **same** `LspServerEvent::Diagnostics` event, and the frontend's `lsp:diagnostics` listener doesn't care which mode produced any given report.

The `result_id` round-trip from a previous pull (which would let a server reply `Unchanged` and skip resending unmodified diagnostics) is not threaded through yet — we always fire a fresh full pull. Extension slot when latency starts to matter; today the per-request cost is negligible compared to the type-check itself.

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

- **`lsp:diagnostics`** — `LspDiagnosticsEvent { path, diagnostics: [] }`. Full replacement. Either a `textDocument/publishDiagnostics` notification from a push-mode server _or_ a `Full` `DocumentDiagnosticReport` returned by a pull-mode server (see "Diagnostics: push and pull" above) becomes one of these — the frontend can't tell the modes apart and shouldn't have to.
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
