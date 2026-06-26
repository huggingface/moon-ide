# LSP

Status: partial — TypeScript, Rust, Python, and Go have diagnostics +
hover + completion + goto-definition + F2 rename + nav history wired.
All four route inside the workspace container when one is up (see
[Container-backed LSP](#container-backed-lsp)), falling back to host
LSP per-language. Other languages (Svelte, CSS, HTML, JSON) are
architecturally in scope, not yet wired.

## The non-negotiable invariant

LSP lives in `moon-core`. Nothing in the UI speaks LSP JSON-RPC
directly. The frontend sees **moon-shaped** types
(`crates/moon-protocol/src/lsp.rs`) and `lsp_*` Tauri commands that
forward to the broker. One translation wall between upstream protocol
types and the UI — same discipline as the git layer.

## Layers

```
Svelte UI (Editor, StatusBar, state.svelte.ts)
  │  Tauri commands / events (moon-shaped types)
src-tauri/commands/lsp.rs — lazy broker, lifecycle tied to active folder
  │  moon-core public API
moon-core::lsp::LspBroker — per-workspace, keyed on root path
  ├ spec_for(language_id)  — wire-in table
  └ LspServer ×N           — one child process per language id
     ├ LspClient           — JSON-RPC over framed stdio (reader/writer actors)
     └ translate::*        — lsp_types ↔ moon_protocol::lsp
  │  LSP JSON-RPC over stdin/stdout
child process (tsgo --lsp --stdio, rust-analyzer, ty server, gopls)
```

## Decisions

### Custom thin client, not `tower-lsp`

`tower-lsp` is a **server** framework; we're a client. We use the
upstream `lsp-types` crate for message shapes and roll ~300 LOC of
framing + actor client on top. Fewer crates, and we control the
places a race would bite.

### One process per `(workspace, language_id)`

Not per file (defeats the server's internal project caches), not
global. Workspace close sends `shutdown` + `exit` with a 2 s grace
per server; `kill_on_drop(true)` is the escape hatch for a wedged
server.

### Lazy spawn

Nothing spawns until the first `lsp_open`. A workspace with zero
TypeScript files pays zero LSP cost.

### Full-document sync (stage 1)

Every `didChange` carries the whole file. Fast enough for normal
buffer sizes; incremental sync is a later optimisation when a slow
file shows up.

### Position encoding is UTF-16

LSP's default, and CodeMirror's native string offsets are UTF-16 code
units too — no conversion in either direction.

### Active-folder is the broker root

Switching the active folder drops the broker and rebuilds; surviving
tabs re-issue `didOpen` naturally on re-render. The user sees a short
"starting…" pill until the new server's first pass.

### Per-language servers

| Language   | Server          | Install                                      | Notes                                                                                                                                                                                                                      |
| ---------- | --------------- | -------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| TypeScript | `tsgo`          | `bun add -D @typescript/native-preview`      | Microsoft's native TS 7 port, not `typescript-language-server`: already in our devDependencies, upstream-aligned, no Node runtime, ~10× faster. Still a preview channel — switching back is a one-string edit on the spec. |
| Rust       | `rust-analyzer` | `rustup component add rust-analyzer`         | Ecosystem standard; no per-project install convention exists. No startup args; defaults suffice.                                                                                                                           |
| Python     | `ty`            | `uv add --dev ty` (or `uv tool install ty`)  | Astral's Rust-native checker. Beta — if a gap blocks us, `pyright-langserver` is a one-string edit.                                                                                                                        |
| Go         | `gopls`         | `go install golang.org/x/tools/gopls@latest` | The official Go LSP. No startup args; reads `go.mod` / `go.work` itself.                                                                                                                                                   |

### Binary discovery: ecosystem-idiomatic first, then PATH

Per-language `DiscoveryStrategy`:

- **`NodeModules`** (TS/JS): ancestor walk for
  `node_modules/.bin/<bin>` (Node's own resolution), then a bounded
  downward scan (depth 4, skipping `node_modules` / `.git` /
  dot-dirs) for per-package install layouts, then `$PATH`. First
  match wins — a project-pinned copy always beats a global install.
- **`CargoHome`** (Rust): `$CARGO_HOME/bin` (GUI-launched processes
  often don't have it on `PATH`), then `$PATH`.
- **`PythonVenv`** (Python): ancestor walk for `.venv/bin/<bin>`,
  then `$PATH`.
- **`GoBin`** (Go): `$GOBIN`, then `$GOPATH/bin` (default
  `$HOME/go`), then `$PATH`.

Windows adjusts the filename per strategy (`.cmd` for npm wrappers,
`.exe` elsewhere). If nothing is found, the broker caches a
`NotAvailable` slot and the status bar shows a quiet pill whose
tooltip is the spec's copy-pasteable `install_hint` (the TS hint
adapts to the workspace's lockfile: pnpm / npm / bun).

### Container-backed LSP

When the workspace shell container is `Running`, the broker spawns
servers **inside** it via `docker exec` — the server sees the same
filesystem the build commands see, and a new contributor skips the
"install rust-analyzer locally" step.

Routing: the Tauri layer picks a primary target per-broker at
construction; with a `DockerExec` primary the broker keeps a host
fallback and retries per-language when the container can't
resolve / probe / spawn that server's binary:

| Workspace shape      | Container state | Primary   | Outcome                                                                                          |
| -------------------- | --------------- | --------- | ------------------------------------------------------------------------------------------------ |
| No container config  | n/a             | Host      | Host LSP.                                                                                        |
| Container configured | Not running     | Host      | Host LSP.                                                                                        |
| Container configured | Running         | Container | Container LSP when the binary probes OK; else automatic host fallback; else `NotAvailable` pill. |

The per-server fallback covers a custom image that dropped a server,
and binaries that aren't reachable inside the mount (e.g. a pnpm
monorepo whose hoisted `node_modules` sits above the bound folder).

Key mechanics (implementation in `moon-core/src/lsp/spawn.rs` +
`server.rs`):

- `LspSpawner::DockerExec` wraps the command as
  `docker exec -i <container> …` — `-i` without `-t`, because a TTY
  would mangle the raw LSP framing.
- A `PathTranslator` rewrites every URI crossing the boundary so the
  server sees `/workspace/<basename>` paths while the UI stays in
  host-path land. It also nulls `initialize.processId` under a
  mount: the server's parent-PID watchdog would always fail in the
  container's PID namespace and suicide the server.
- `probe(bin)` runs `<bin> --version` through the same pipeline as
  the real spawn; first successful route wins, cached per-language.
- In-container binary resolution mirrors the host discovery
  strategies, rooted at the bind mount; `CargoHome` / `GoBin`
  specs just use the basename since `moon-base` ships those servers
  on `$PATH`.
- The in-container mount convention (`/workspace/<basename>`) is
  defined in one place shared with terminals, so the two can't
  drift.

Lifecycle: every mutating container command resets the broker (next
`lsp_open` rebuilds against the new state — cheaper and more
deterministic than mutating in place). Folder-switch teardown is
detached (`tokio::spawn`ed shutdown) so the switch doesn't block up
to several seconds on dying servers (test plan 0076), and the
frontend re-fires `lspOpen` for the new folder's open buffers so the
fresh server doesn't start with an empty docs map (otherwise
`didChange` for unknown files is silently dropped and diagnostics
never flow).

Image responsibility: `moon-base` pre-installs only servers with no
per-project install convention (`rust-analyzer`, `gopls`). `tsgo`
and `ty` are deliberately not baked in — their per-project installs
are first-class and should win.

Non-goals for this slice: remote (SSH) LSP — `DockerExec` doesn't
generalise; containers the user starts outside moon-ide.

### Client capabilities are minimal

We only advertise what's wired. Adding a capability is localised:
flip the flag in `server::initialize`, add the Tauri command, add the
CM adapter.

### Diagnostics: push and pull, both feed one event

LSP 3.17 has two delivery modes and we support both: **push**
(`publishDiagnostics`, used by rust-analyzer) and **pull**
(`textDocument/diagnostic` fired by us after every
`didOpen` / `didChange` — `tsgo` is pull-only, which is why this
exists). A push-only server returns `MethodNotFound` for the pull
and we drop it at debug; a pull-only server just never pushes. Both
feed the same `LspServerEvent::Diagnostics`, so the frontend can't
tell the modes apart. The `result_id` round-trip (server replies
`Unchanged`) isn't threaded through yet — full pull every time;
revisit if latency matters.

### Stale-diagnostics refresh on off-disk changes

Pull diagnostics only fire on open/change, so off-editor rewrites
(`git checkout`, external saves, coder tools touching unopened files)
leave the server on a stale view. Two closures:

- **Server-driven (steady state).** We advertise
  `didChangeWatchedFiles` dynamic registration + diagnostics
  `refreshSupport`. Servers register their watch globs; the
  frontend's `fs:changed` listener forwards each batch and the
  broker fans out per-server, filtered through that server's globs.
  Servers respond with `workspace/diagnostic/refresh`, which makes
  the broker re-pull every open buffer on that server. The watcher
  carries no per-path change kind, so we send `Changed` for
  everything — every server we wire invalidates caches on `Changed`
  regardless; extend the payload if fidelity ever bites.
- **Focus-driven (cold-start safety net).** A `git checkout` while
  the IDE was closed leaves no watcher trace, so every window
  focus-gain re-pulls all open buffers on all servers (debounced
  250 ms). Push-only servers no-op the pull cheaply.

Server→client requests (`client/registerCapability`,
`workspace/configuration`, …) are auto-replied and forwarded to the
notification pump, which pattern-matches the methods it reacts to.

### Go-to-definition is Ctrl/Cmd-click + a link-preview hover

The VS Code-style "hold modifier, see underline, click to jump" UX
lives in `src/lib/editor/lspGotoDefinition.ts`: modifier tracking on
the window (so focus changes mid-hold don't drop it), cheap-cached
definition probes with an epoch guard against stale responses, and
`workspace.jumpTo` on click. We advertise `linkSupport: true` and
take `targetSelectionRange` over `targetRange` so the caret lands on
the identifier, not inside the body. External targets
(`node_modules/`, TS lib) surface a toast; a read-only viewer is a
later deliverable.

### F2 rename

Wired through `src/lib/editor/lspRename.ts` + the
`prepare_rename` / `rename` server methods:

- F2 fires `prepareRename`; servers that decline (punctuation,
  keywords) get a quiet flash, no panel.
- The same extension is wired into the diff view's editable
  right-hand pane and is reachable from the editor's right-click
  menu (**Rename symbol**), so F2 / context-menu rename behave
  identically in plain editing and while viewing a working-tree
  diff.
- A docked panel with prefilled input; Enter fires `rename`, Escape
  or editing the buffer behind the panel dismisses.
- The returned `LspWorkspaceEdit` applies to open buffers through
  `workspace.updateText` (buffers stay dirty — review-then-save,
  like VS Code) and to closed files through the `WorkspaceHost`
  (which runs the normal format-on-save pipeline), followed by
  `notifyFilesChanged` so other servers re-publish.
- The translator drops resource ops (`CreateFile` / `RenameFile` /
  `DeleteFile`) — a pure-identifier rename never needs them, and
  applying fs mutations with no confirmation surface is a trap.
- Out of scope: cross-folder rename. The LSP is rooted at the
  active folder, so its plan is folder-scoped by construction.

### Navigation history (Alt+Left / Alt+Right)

Position-aware, browser-style history on `WorkspaceState`. Entries
are `{ folder, path, line, character }` — folder-tagged so history
walks across bound folders. VS Code semantics: genuine navigations
(clicks, file switches, `jumpTo`) **push** and truncate the forward
stack; every other selection change **updates the tip in place**, so
Alt+Right returns to where you actually ended up. Stepping through
history suppresses its own pushes. Cross-folder restores switch the
active folder first and bail gracefully (with a flash) if the folder
was unbound; `removeFolder` prunes stale entries.

Cross-folder goto-definition: a target outside the active folder's
root comes back as `externalUri`; the frontend walks the bound-folder
list (longest prefix first) and rewrites it into a
`(folder, relative)` jump when a bound folder contains it. Only
genuinely-external targets keep the toast.

Caret hand-off for "open file at position" callers goes through a
one-shot `pendingJumps` map keyed by `(folder, path)`, consumed by
the editor once the target buffer renders.

## Events

- **`lsp:diagnostics`** — `{ path, producer, diagnostics }`. Full
  replacement **per producer** (the server's slot key), so two
  servers reporting on the same file don't clobber each other; the
  editor consumes the flat union. Push and pull reports both become
  this event.
- **`lsp:status`** — `{ languageId, status, detail? }` on every
  server-state transition. Crash detection is push: a liveness flag
  flipped by either I/O loop fans out `crashed` immediately, the
  broker evicts dead slots so the next request re-spawns, and the
  frontend re-opens the active buffer so the new server has the live
  text.
- **`logs:entry`** (cross-cutting) — the broker logs routing
  decisions, discovery hits/misses, status transitions, and child
  stderr into the shared `LogSink` under `lsp.<language_id>`, so the
  bottom-panel logs view answers "why didn't this server come up?"
  (test plan 0069).

## Frontend architecture

- **`state.svelte.ts`** — single source of LSP state:
  `diagnosticsByProducer` (per-path, per-producer), a recomputed
  flat `diagnostics` union for the editor, `lspStatuses`, and the
  `lspOpen` / `lspScheduleUpdate` (150 ms debounce) / `lspClose`
  wrappers.
- **`editor/lsp.ts`** — CM adapters: lint gutter, hover tooltip,
  and the completion source. Completion replaces the whole
  identifier under the caret — the range is extended forward past
  any word characters that follow the caret, so accepting an item
  mid-word (caret after `Ob` in `ObjectId`) rewrites the token
  rather than inserting into it. It honours the server's
  `textEdit` range and chases lazy-resolved auto-import edits via
  `completionItem/resolve`: the primary insertion lands immediately
  and the import block follows in a second transaction once the
  resolve returns. Two undo units is deliberate — VS Code / Helix /
  Zed do the same, because making every accept wait on the resolve
  round-trip feels laggy.
- **`editor/lspGotoDefinition.ts`** / **`editor/lspRename.ts`** —
  see their sections above.
- **`editor/lspWorkspaceEdit.ts`** — shared applier for any
  `LspWorkspaceEdit` (rename, quick-fixes, future fix-alls):
  open buffers via `workspace.updateText`, closed files via
  read → apply → write + `notifyFilesChanged`.
- **`editor/lspLanguage.ts`** — path → language-id mapping; also
  the feature flag (`null` = no LSP here).

### Diagnostic quick-fixes (lint tooltip)

The lint tooltip shows the diagnostic plus action buttons from two
sources:

1. **LSP quickfixes** — prefetched per diagnostic via
   `lsp_code_action`, routed to **the producer that emitted the
   diagnostic** (fanning out to every co-tenant would yield
   unrelated noise). Replies are filtered to entries with a
   non-empty `edit` (pure `Command` entries are dropped — we don't
   run `workspace/executeCommand`), cached, and re-rendered into the
   tooltip when the prefetch lands. For oxlint this surfaces the
   rule's autofix plus disable-for-line / disable-for-file
   insertions; tsgo and rust-analyzer quickfixes come through the
   same path.
2. **"Fix in coder"** — always present, no IPC: opens the coder
   panel, attaches the squiggled range as a selection chip, and
   seeds a short composer draft the user edits before sending. The
   escape hatch when the linter has no programmatic fix.

Both apply through the shared workspace-edit applier.

### Linter co-tenants

Some linters speak LSP — today `oxlint --lsp`. It runs through the
same broker but in a parallel `lint_servers` slot map, **alongside**
the language server on the same file: a `.ts` open spawns both
`tsgo` and `oxlint`, each publishing diagnostics under its own
`producer`. Design points:

- **Separate slot map** because the routing key (`"oxlint"`)
  differs from the file's `languageId` (`"typescript"`, which
  oxlint still needs to pick its parser).
- **Narrow surface**: only the diagnostics-producing calls
  (open / update / close / watched-files / refresh / shutdown) fan
  out to lint servers; hover / completion / rename never do.
- **Independent lifecycle**: own status pill, own crash recovery,
  own lockfile-aware install hint. `lspSlotCoversFile` maps a slot
  key to the file-language ids it governs so restart / crash
  recovery re-opens the right buffers (filtering by file language
  would match zero files for a linter slot).
- **Workspace folders per nested config**: oxlint anchors config
  discovery on `workspaceFolders`, not a per-file walk-up, so a
  monorepo with nested `.oxlintrc.json` files needs each containing
  directory advertised as a workspace folder. The broker does a
  gitignore-aware bounded scan (depth 5) and passes the result at
  spawn; language servers get just the root. Relatedly,
  `workspace/configuration` is answered with one empty object per
  requested item rather than `null` — the spec-correct "no LSP
  overrides, use your disk config" reply that oxlint requires to
  honour the on-disk config.
- **`update` routes through `open`** in the broker, so a respawned
  server's first debounced update becomes its `didOpen` instead of
  vanishing — without this, diagnostics freeze on the pre-crash
  snapshot until a tab switch.

Adding another linter is the same template: a new spec, a new
languages set, a branch in `lint_spec_for` and `lspSlotCoversFile`.
No generic registry until the team asks.

## Non-goals of stage 1

- Find-references, code actions beyond quick-fixes — stage 4+.
  (Goto-definition and rename have since shipped.)
- Workspace symbols (needs a palette split).
- Signature help, selection range, semantic tokens.
- Snippets in completion (`snippet_support: false`; needs a
  frontend snippet renderer).
- Progress reporting — the status pill covers "is it ready?".
- Incremental document sync.
- Crash recovery beyond re-spawn-on-next-request.

## When to update this doc

- New language server: add a spec constant, extend
  `LspBroker::spec_for`, mirror the language id in
  `lspLanguage.ts`, note it here, add a test plan.
- New capability: extend `moon-protocol::lsp`, add the Tauri
  command, the CM adapter, the capability advertisement, document
  it here, add a test plan.
- Wire-shape change: bump `moon_protocol::PROTOCOL_VERSION`, update
  this doc and the affected test plan.
