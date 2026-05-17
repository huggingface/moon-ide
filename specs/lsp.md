# LSP

Status: partial — TypeScript, Rust, Python, and Go all have diagnostics + hover + completion + goto-definition + F2 rename + nav history wired via the stage-1/stage-2 slices of Phase 4. All four additionally route **inside the workspace container** when one is up and the binary is reachable there (see [Container-backed LSP](#container-backed-lsp)); the broker falls back to host LSP per-language when it isn't. Every other language (Svelte, CSS, HTML, JSON) is architecturally in scope and not yet wired.

## The non-negotiable invariant

LSP lives in `moon-core`. Nothing in the UI speaks LSP JSON-RPC directly. The frontend sees **moon-shaped** types (`LspDiagnostic`, `LspHover`, `LspCompletionList`, `LspLocation`, `LspPrepareRename`, `LspWorkspaceEdit`, `LspStatusEvent` — see `crates/moon-protocol/src/lsp.rs`) and Tauri commands (`lsp_open` / `lsp_update` / `lsp_close` / `lsp_hover` / `lsp_completion` / `lsp_completion_resolve` / `lsp_definition` / `lsp_prepare_rename` / `lsp_rename`) that forward to the broker.

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

### Python server: `ty`

We target [`ty`](https://github.com/astral-sh/ty), Astral's Rust-native Python type checker + language server. Same vendor as `uv` and `ruff`, distributed as a single statically-linked binary, advertises an LSP under the `ty server` subcommand (matching `ruff server`'s convention).

Discovery treats `ty` like `tsgo`: project-local first via `.venv/bin/ty` (where `uv pip install ty` / `uv add --dev ty` lands), then `$PATH` (where `uv tool install ty` ends up via `~/.local/bin`). A monorepo with a single top-level `.venv/` works without configuration — same ancestor-walk semantics as `node_modules/.bin/`. Install hint:

```
uv add --dev ty (or uv tool install ty)
```

`ty` is in beta as of mid-2026 (`0.0.x` versions; the Astral team explicitly notes breaking changes between any two releases). If a feature gap blocks us, switching to `pyright-langserver` or `pylsp` is a one-string edit on `PYTHON_SERVER` in `crates/moon-core/src/lsp/server.rs` — the broker is wire-protocol-agnostic.

We don't bake `ty` into the `moon-base` image. Python projects vary too much for a single global pin to be useful (some need `ty`, others want `pyright`, others need a project-pinned version), and the per-project install matches what `bun install` already does for `tsgo`. The container surfaces an unavailable-pill until the user runs `uv add --dev ty` or installs globally in the container; matches the TS UX.

### Rust server: `rust-analyzer`

The ecosystem-standard LSP. No per-project install exists for Rust LSPs (unlike `tsgo`), so we rely on the system toolchain. Install on the developer's host:

```
rustup component add rust-analyzer
```

This drops `rust-analyzer` at `$CARGO_HOME/bin/rust-analyzer` (typically `~/.cargo/bin/rust-analyzer`). A `cargo install rust-analyzer` build or a distro package manager install both land on `$PATH`; both shapes resolve without extra work.

No startup args: `rust-analyzer` defaults to stdio + LSP when invoked with no flags, which is exactly what we want. It auto-detects workspace layout from `initialize.workspaceFolders`, so the generic init we already send suffices. Advanced configuration (`checkOnSave`, `cargo.features`, proc-macro toggles, etc.) is left at defaults for now — we'll add `workspace/didChangeConfiguration` plumbing when a real need surfaces.

### Go server: `gopls`

Same posture as `rust-analyzer` — the official LSP from the Go team (`golang.org/x/tools/gopls`) and the only one anyone ships in practice. Go has no per-project install convention; the binary always lands in the user-wide GOPATH after:

```
go install golang.org/x/tools/gopls@latest
```

This drops `gopls` at `$GOPATH/bin/gopls` (with `$GOPATH` defaulting to `$HOME/go` per the Go toolchain since Go 1.8). Discovery prefers `$GOBIN/gopls` (the explicit override the Go toolchain itself honours over `$GOPATH/bin`), then `$GOPATH/bin/gopls`, then `$PATH` — so distro packages (`golang-go`), Homebrew, and hand-compiled installs all resolve without extra work.

No startup args: `gopls` defaults to stdio + LSP when invoked with no flags. It reads `go.mod` / `go.work` itself and auto-detects the workspace layout from `initialize.workspaceFolders`. Advanced configuration (build flags, analyzer toggles, etc.) is left at defaults for now.

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
- **`DiscoveryStrategy::PythonVenv`** (Python): walks up from the broker's root looking for `<ancestor>/.venv/bin/<bin_name>` (Unix) / `<ancestor>/.venv/Scripts/<bin_name>.exe` (Windows), then `$PATH`. Mirrors the `NodeModules` shape for the Python ecosystem: `.venv/` is `uv`'s default virtualenv layout and where `uv pip install` / `uv add --dev` land. The PATH fallback catches users who installed via `uv tool install` (lands in `~/.local/bin`).
- **`DiscoveryStrategy::GoBin`** (Go): checks `$GOBIN/<bin_name>` first (the Go toolchain's explicit override), then `$GOPATH/bin/<bin_name>` (with `$GOPATH` defaulting to `$HOME/go` per the toolchain default since Go 1.8), then `$PATH`. Same posture as `CargoHome`: `go install` always writes user-wide, so a per-user pin is the canonical install path; the PATH fallback covers distro / Homebrew / hand-compiled installs.

On Windows we adjust the filename per strategy: `<bin>.cmd` for the Node case (npm's `.bin` wrapper), `<bin>.exe` for Cargo (native executables), `<bin>.exe` for Python (matches CPython's venv layout).

If nothing is found on disk, the broker caches a `NotAvailable` slot per language and emits `lsp:status { status: 'notavailable' }`. The status bar paints a quiet pill whose tooltip is the spec's `install_hint` field (e.g. `bun add -D @typescript/native-preview` or `rustup component add rust-analyzer`) — copy-pasteable into a terminal. The TypeScript hint is adapted to the workspace root's package-manager lockfile (`pnpm-lock.yaml` → `pnpm -wD add @typescript/native-preview`, `package-lock.json` → `npm i -D @typescript/native-preview`, otherwise `bun add -D ...`); the other languages have one canonical install path each so they keep their static hint.

Container-backed workspaces (ADR 0008) skip host discovery entirely for languages whose server the container already ships — see [Container-backed LSP](#container-backed-lsp). `moon-base` pre-installs `rust-analyzer` (via `rustup component add`) and `gopls` (via `go install`), and the broker pipes stdio through `docker exec` when the container is `Running`. Falling back to the host is automatic when the container is down, not configured, or doesn't have the server.

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

In-container binary-path resolution lives in `moon_core::lsp::server::container_binary_path`. It walks host ancestors for `NodeModules`- and `PythonVenv`-strategy specs and translates matches through the `HostMount` translator; for `CargoHome`- and `GoBin`-strategy specs it hands back the basename because `moon-base` installs the corresponding server (`rust-analyzer`, `gopls`) on the container's `$PATH`.

**Pieces.**

- `moon_core::lsp::LspSpawner` (in `spawn.rs`) is the ADT: `Local` runs `Command::new(bin)`; `DockerExec { container_name }` wraps it as `docker exec -i <container> <bin> <args...>`. `-i` (no `-t`) is critical — LSP framing is raw bytes over stdio, a TTY would mangle them.
- `moon_core::lsp::server::PathTranslator` bridges the two filesystem views. `Identity` is a no-op; `HostMount { host_root, server_root }` rewrites paths in both directions so the server sees `/workspace/<basename>` URIs while the UI and tree stay in host absolute-path land. Every URI that crosses the boundary (initialize's rootUri, didOpen, diagnostics, goto-def) goes through the translator. The translator also gates `initialize.processId`: we forward our host PID only when `Identity` (same PID namespace); under `HostMount` we send `null` because the server's `kill -0 <host_pid>` watchdog would always fail in the container's namespace and the server would suicide every few seconds (tsgo: "Parent process N has exited, shutting down" / "context canceled" on a 5s loop).
- `LspBroker::new_with_spawner(root, spawner, translator)` is what Tauri calls. `LspBroker::new(root)` still works — it's the host-only helper used by tests. Constructing with a `DockerExec` spawner auto-populates the host fallback; constructing with `Local` leaves it empty.
- `LspSpawner::probe(bin)` runs `<bin> --version` via the same build-command pipeline that the real spawn uses, and reports whether it exited cleanly. The broker calls it per-server on each route it tries; the first success wins. Cached outcome lives in the existing per-language `ServerSlot` map.
- `TerminalTarget::container_cwd_for_folder` (in `moon-terminal`) is the single place that defines the in-container mount convention (`/workspace/<basename>`). `ensure_broker` reuses it so terminals and LSP never drift.

**Teardown on container transitions.** Every mutating container command (`container_setup`, `container_stop`, `container_pause`, `container_resume`, `container_rebuild`, `container_teardown`, `container_apply_bound_folders`) calls `reset_lsp_broker` after the compose action completes. That drops the current broker; the next `lsp_open` rebuilds against whatever state the container is in now. Cheaper and more deterministic than trying to mutate the broker in place.

**Folder-switch teardown is detached.** `workspace_set_active_folder` / `workspace_open_local` / `workspace_remove_folder` snip the old `LspHandle` out of the mutex synchronously, then `tokio::spawn` the actual `broker.shutdown_all()` so the IPC roundtrip returns immediately. Otherwise the user's "switch folder" click would block on every running LSP server in sequence — each gets up to 2 s for `shutdown` + 2 s for `child.wait`, which on a TS + rust-analyzer + tailwind project added up to 6–12 s of UI freeze before the new folder bar even painted. Nothing on the frontend's critical path needs the old brokers to finish dying: the next `lsp_*` IPC lazily re-spawns against the new root regardless of whether the old child has reaped yet, and the OS handles any leaked stdio cleanup if a wedged server outlives the spawning task. See [test plan 0076](test-plans/0076-folder-switch-perf.md).

**Re-priming on folder switch.** `WorkspaceState.setActiveFolder` (the user-driven folder-bar click + the cross-folder Ctrl+click + Alt+Left/Right paths) iterates the new folder's open buffers and re-fires `lspOpen` for each. The backend's `ensure_broker` would otherwise rebuild the broker lazily on the next `lsp_*` IPC and the new server would start with an empty docs map: typing fires `lsp_update`, which reaches a server that doesn't know the file (silently dropped) so no diagnostics flow; hover / completion / definition either get `null` back or fall through to project-wide analysis with stale assumptions. Symptom in container-routed setups is typically TS "Cannot find name 'assert'" / missing-`@types/node` noise that only clears after a manual LSP restart (which re-fires `lspOpen` for the same reason). Mirrors what `restartLsp` does after a crash and what `restoreAppState` does for the active folder at startup — folder switches were the missing entry point.

**Image responsibility.** `moon-base` pre-installs language servers that benefit from a global pin: today that's `rust-analyzer` via `rustup component add` (Rust has no per-project install convention) and `gopls` via `go install` (same logic — Go has no per-project install convention either). TypeScript (`tsgo`) and Python (`ty`) are intentionally **not** pre-installed — both have first-class per-project install paths (`bun add -D @typescript/native-preview`, `uv add --dev ty`), and the per-project copy is what should win. The container surfaces an unavailable-pill until the user installs the server; matches the host UX. See [`containers.md`](containers.md#the-moon-base-image) for the current tool inventory.

**Known non-goals for this slice.**

- Svelte / CSS / HTML / JSON — wire up when the language itself ships in the broker. Routing through the container follows the same template at that point.
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

### Stale-diagnostics refresh on off-disk changes

Pull diagnostics only fire on `didOpen` / `didChange`, so when something rewrites a file _outside_ the editor — `git checkout` flipping branches, an external editor save, a coder tool patching an unopened file — the LSP server stays on its last-computed view of the world. The classic shape: branch A defines `foo(a, b)`, caller calls `foo(x, y, z)`, server emits "expected 2 args, got 3"; user `git checkout`s branch B (where `foo` takes 3 args), Ctrl+Clicks `foo` to verify the new signature, Alt+Lefts back to caller — diagnostic still says "expected 2 args".

We close the gap with the canonical LSP plumbing plus a thin focus-driven safety net.

**Server-driven (in-IDE off-disk changes).** We advertise `workspace.didChangeWatchedFiles.dynamicRegistration: true` and `workspace.diagnostics.refreshSupport: true` in `initialize`. Servers that care register watch globs via `client/registerCapability`; tsgo / tsserver register `**/*.{ts,tsx,js,jsx,json}` and friends, rust-analyzer registers `**/Cargo.toml` + the rust globs, gopls registers `**/*.go` etc. The frontend's `fs:changed` listener forwards every batch to [`lsp_notify_files_changed`](../src-tauri/src/commands/lsp.rs); the broker fans out to every running server, each filtering the paths through its own registered globs and firing one `workspace/didChangeWatchedFiles` notification with the matching subset (no notification at all when the burst doesn't intersect any registered glob — `.toml`-only changes never reach `tsserver`). Well-behaved servers respond by invalidating per-file caches and sending us `workspace/diagnostic/refresh`, which the broker's notification pump turns into a per-server [`LspServer::refresh_open_diagnostics`](../crates/moon-core/src/lsp/server.rs) call — re-pulling diagnostics for exactly the open buffers the server thinks were affected. End result: the panel catches up to the on-disk truth without the user having to retype.

The host fs-watcher emits a flat `paths` list with no per-path Create/Change/Delete classification, so we send `FileChangeType::Changed` for every entry. Every server we wire today reacts to `Changed` by invalidating caches regardless; if a fidelity issue surfaces (a brand-new file not getting indexed despite us "telling" the server) the right fix is extending the `FsChangedPayload` to carry a kind, not over-engineering it preemptively.

Server→client requests (`client/registerCapability`, `workspace/diagnostic/refresh`, `workspace/configuration`, …) are auto-replied with `null` by [`client.rs`](../crates/moon-core/src/lsp/client.rs) — that's spec-acceptable success for every request we currently react to — and forwarded to the same notification broadcast the broker already subscribes to. The server-module pump pattern-matches on `method` to decide whether to record glob registrations, refresh diagnostics, or drop on the floor.

**Focus-driven (cold-start safety net).** The fs-watcher only fires for changes that happen during _its_ lifetime; a `git checkout` that happened while moon-ide was closed leaves no trace. The window-focus event listener calls [`lsp_refresh_open_diagnostics`](../src-tauri/src/commands/lsp.rs) with no scope filter on every focus-gain (debounced 250ms via [`WorkspaceState.scheduleLspDiagnosticsRefresh`](../src/lib/state.svelte.ts) so a rapid alt-tab pair collapses to one IPC), which re-pulls every open buffer on every running server. Push-only servers (rust-analyzer, which uses `publishDiagnostics` rather than answering `textDocument/diagnostic`) silently no-op the pull at debug-log level, so the broad fan-out stays cheap regardless of which mix is up. This branch deliberately stays in even though the server-driven path covers the steady-state — the cold-start window can't be plugged any other way.

### Go-to-definition is Ctrl/Cmd-click + a link-preview hover

The "hold modifier, mouse over identifier, see underline, click to jump" UX (from VS Code, Cursor, and every IDE the team already uses) is baked into the editor itself rather than a palette command. Lives in `src/lib/editor/lspGotoDefinition.ts`:

- A `ViewPlugin` tracks modifier state (`Ctrl` on Linux/Windows, `Cmd` on macOS) by listening on the window — not the editor — so focus changes during a modifier-hold don't drop the state.
- `mousemove` with the modifier held resolves the word under the cursor, calls `ipc.lsp.definition`, and paints an underline (`Decoration.mark` → `.cm-lsp-link`) if the server offers a target.
- Probes are cheap-cached: re-hovering inside the same word span is a no-op; each probe carries an `epoch` that's invalidated if the pointer moves on, so a slow LSP response never lands stale.
- `mouseup` with the modifier held re-calls `definition` (the earlier response may have been discarded) and routes through `workspace.jumpTo(path, position)`.
- **External targets** (paths outside the workspace root — `node_modules/`, TS built-in lib, etc.) come back with `path: ''` and `externalUri` populated. The UI surfaces a toast rather than silently failing. A read-only external-file viewer is a later deliverable.

Server-side capability advertisement is `definition: { linkSupport: true }`; we take `LocationLink`'s `targetSelectionRange` (the identifier) over `targetRange` (the whole body) so the caret lands on the name, not inside a function body.

### F2 rename

The "park caret on an identifier, hit F2, type new name, every reference rewrites" UX is wired through `src/lib/editor/lspRename.ts` (CM extension) and the matching `prepare_rename` / `rename` server methods in `crates/moon-core/src/lsp/server.rs`.

- **Trigger.** F2 keymap entry reads `wordAt(caret)`, falls back to the bare text under the cursor, and fires `textDocument/prepareRename`. Servers that decline (cursor on punctuation, keyword, string body) get a quiet flash; we don't open the panel.
- **Panel.** A docked CM `showPanel` row at the top of the editor with a prefilled input + Rename / Cancel buttons. Enter fires `textDocument/rename`; Escape (or the Cancel button) dismisses. Any edit to the underlying buffer behind the panel also dismisses — the user typing in the document is the signal they've moved on.
- **Applier.** `LspWorkspaceEdit.documentEdits` is split into open buffers (routed through `workspace.updateText` so the file goes dirty and the CM `$effect` syncs the in-memory text into the view) and closed files (read → apply → write through the active folder's `WorkspaceHost`, then `lsp_notify_files_changed` so every server can invalidate caches and re-publish diagnostics). Open buffers' `didChange` carries their updates directly, so we don't double-notify them.
- **No auto-save.** Edited open buffers stay dirty; the user reviews them in the tab strip / SCM panel and commits with Ctrl+S. Matches VS Code's behaviour and gives a clear "review then save" path. Closed files write through the workspace host's `save_file`, which runs the normal format-on-save pipeline — same bytes the user would land on if they manually opened and saved.
- **Wire shape.** Server capability advertisement is `rename: { prepareSupport: true, ..., honorsChangeAnnotations: false }`. The runner-side translator drops `WorkspaceEdit.documentChanges` resource ops (`CreateFile` / `RenameFile` / `DeleteFile`) — a pure-identifier rename never asks for them, and applying file-system mutations without a confirmation surface is a trap.
- **Out of scope today.** Cross-folder rename (touching files in sibling bound folders). The LSP is rooted at the active folder, so its rename plan is folder-scoped by construction; the translator drops any URI outside that root with no surface — surfaces when we grow the multi-bound-folder LSP path.

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

- **`lsp:diagnostics`** — `LspDiagnosticsEvent { path, producer, diagnostics: [] }`. Full replacement **per producer**. Either a `textDocument/publishDiagnostics` notification from a push-mode server _or_ a `Full` `DocumentDiagnosticReport` returned by a pull-mode server (see "Diagnostics: push and pull" above) becomes one of these — the frontend can't tell the modes apart and shouldn't have to. `producer` carries the server's slot key (`"typescript"`, `"rust"`, `"oxlint"`, …) so two servers reporting on the same file don't clobber each other — the frontend keys diagnostics by `(path, producer)` and the editor's lint gutter consumes the flat union. See [Linter co-tenants](#linter-co-tenants).
- **`lsp:status`** — `LspStatusEvent { languageId, status, detail? }`. Emitted on every server-state transition (spawn attempt, initialise success, crash, shutdown). The UI caches the latest per language id and only renders the pill when the status is anything other than `running`. **Crash detection is push, not pull**: each `LspClient` carries an `AtomicBool` liveness flag flipped by either I/O loop on exit, plus a `Notify` that fans out the transition. `LspServer::spawn` parks a watcher task that emits `status: 'crashed'` the instant the flag flips, and the broker's `ensure_server` evicts dead slots via `is_alive()` so the next request re-spawns. The frontend re-`open`s the active buffer on a fresh `crashed` so the new server lands with the live text in its doc set.
- **`logs:entry`** (cross-cutting) — `LogEntry { source, level, message, tsMs, seq }`. Not LSP-specific; the broker emits into the workspace's shared `LogSink` (`crates/moon-core/src/logs.rs`) using `lsp.<language_id>` as the source key. Routing decisions (primary→fallback), discovery hits/misses, status transitions, and child stderr all flow through here so the user can open the bottom-panel logs view and see why a server didn't come up. See [test plan 0069](test-plans/0069-diag-logs-panel.md).

## Frontend architecture

- **`src/lib/state.svelte.ts`** is the single source of LSP state on the frontend:
  - `diagnosticsByProducer: Map<path, Map<producer, LspDiagnostic[]>>` — full state, written by the `lsp:diagnostics` listener (`producer` slice replaced on each event).
  - `diagnostics: Map<path, LspDiagnostic[]>` — flat union the editor reads; recomputed from `diagnosticsByProducer` on every event so consumers (`Editor.svelte`, `DiffView.svelte`, `StatusBar.svelte`) stay producer-agnostic.
  - `lspStatuses: Map<language_id, LspStatusEvent>` — populated by the `lsp:status` listener.
  - `lspOpen(path, text)` / `lspScheduleUpdate(path, text)` (150ms debounce) / `lspClose(path)` wrap the three lifecycle calls and no-op on file types without a server. The broker fans out each call to the language server **and** any [linter co-tenant](#linter-co-tenants) registered for the file's language id.
- **`src/lib/editor/lsp.ts`** is the CodeMirror adapter surface:
  - `filePathFacet` — current buffer path, read by every adapter.
  - `lspDiagnosticsExtension()` — just `lintGutter()`; the actual diagnostics come in via `applyDiagnostics(view, list)` which `Editor.svelte` calls from a reactive `$effect`.
  - `lspHoverExtension()` — CM `hoverTooltip` delegating to `ipc.lsp.hover`. Rendered inside `.cm-lsp-hover` which the editor chrome theme styles.
  - `lspCompletionSource` — CM `CompletionSource` registered as an `override` on the editor's `autocompletion` extension. Never auto-opens; Ctrl-Space or a panel re-query triggers it. The `apply` callback honours the server's primary `textEdit` range when present (so e.g. completing `foo.bar` from inside `foo` rewrites the dotted span instead of just the prefix), then chases lazy-resolved auto-import edits via `completionItem/resolve`: client capabilities advertise `resolveSupport: { properties: ["additionalTextEdits", "documentation", "detail"] }`, the backend stashes the server's `resolveProvider` flag at `initialize` time, and items returned to the frontend carry an opaque `resolveToken` (a JSON-encoded copy of the original `lsp_types::CompletionItem` so the resolver can hand it back verbatim — the spec demands round-tripping). The frontend dispatches the primary insertion immediately and applies the auto-import block in a follow-up transaction once `lsp_completion_resolve` returns. Two undo units rather than one is a deliberate trade-off — VS Code / Helix / Zed all do it this way; the alternative (everyone waits for the resolve round-trip before any character lands) makes accepts feel laggy on `tsgo` / `rust-analyzer` / `pyright` (the three big consumers of the lazy-resolve pipeline).
- **`src/lib/editor/lspGotoDefinition.ts`** — Ctrl/Cmd-hover link preview + Ctrl/Cmd-click jump. Takes a `{ jumpTo, flash }` callback bag so it doesn't import `state.svelte.ts`. `Editor.svelte` wires the real workspace methods through.
- **`src/lib/editor/lspRename.ts`** — F2 rename. Owns its own state field, keymap, panel, and applier; talks to `workspace` directly for the open-buffer / `flash` surface (the panel is editor-local, so callback-bagging would be over-engineered here).
- **`src/lib/editor/lspLanguage.ts`** — path → LSP language-id mapping. Also the feature-flag: returning `null` means "no LSP here", so adding Rust is literally one entry plus a wire-in on the backend.

### Linter co-tenants

Some linters speak LSP. Today that's `oxlint --lsp` (added upstream in oxc 1.47, [PR #19292](https://github.com/oxc-project/oxc/pull/19292) and friends); we route it through the **same broker** as the language servers, but in a parallel `lint_servers` slot map so it can run **alongside** the language server on the same file. A `.ts` open spawns both `tsgo` (in `servers["typescript"]`) and `oxlint` (in `lint_servers["oxlint"]`); both publish `textDocument/publishDiagnostics`, both stamps land on the editor's lint gutter, each scoped to its own `producer`.

The split is deliberate:

- **Routing key differs from file language id.** `tsgo`'s slot key is `"typescript"`; the file's `textDocument.languageId` is also `"typescript"`. For oxlint, the slot key is `"oxlint"` (the binary, also the producer name on diagnostics + the status pill) while the file's `textDocument.languageId` stays `"typescript"` — that's what oxlint needs in order to pick the right parser. Two maps, keyed by their own slot, sidesteps the language-id collision cleanly without needing a `(language_id, role)` tuple key.
- **Capability surface is narrower.** Linters only do diagnostics. Hover, completion, definition, rename, prepare-rename: not advertised, not asked for, not consulted. Only the diagnostics-producing surface (`open` / `update` / `close` / `notify_files_changed` / `refresh_open_diagnostics` / `shutdown_*`) fans out to both maps; everything else continues to look at `servers` only.
- **Producer stamp keeps clobbers honest.** `LspDiagnosticsEvent` carries `producer: String` — every emit site stamps it with `self.language_id` (the slot key). The frontend stores `diagnosticsByProducer: Map<path, Map<producer, LspDiagnostic[]>>` and recomputes the flat union for the editor on each event, so a fresh oxlint report only replaces oxlint's slice — `tsgo`'s last truth stays put until `tsgo` itself republishes, and vice versa.
- **Lifecycle is independent.** Status events for oxlint carry `language_id: "oxlint"` (matching its slot key), so the status bar paints a separate pill. Crash, restart, NotAvailable: all per-slot. The user can have `tsgo` running fine and `oxlint` not-installed; the install hint is `bun add -D oxlint` (lockfile-aware → `pnpm -wD add oxlint` / `npm i -D oxlint`, mirroring the TS server's adaptation).

`OXLINT_LANGUAGES` (in `crates/moon-core/src/lsp/server.rs`) is the wire-up: the four JS/TS file language ids on which the broker spawns oxlint as a co-tenant. Adding more linters in the future is the same template — a new `LspBinarySpec`, a new `<linter>_LANGUAGES` set, a new branch in `Broker::lint_spec_for`. We don't need a generic "registry" abstraction yet; one linter ships, more get added when the team asks.

## Non-goals of stage 1

- ~~Go-to-definition~~ — shipped in stage 2 (this section). ~~Rename~~ — shipped (see [F2 rename](#f2-rename)). Find-references, code actions — stage 4+.
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
