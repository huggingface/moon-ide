# ADR 0014 — Process per workspace

Date: 2026-05-10
Status: accepted; supersedes the original Phase 7.7 design in [phase-07-multi-workspace.md § 7.7](../roadmaps/phase-07-multi-workspace.md#77--process-per-workspace) (single Tauri process, N webview windows).

## Context

Phase 7 introduced multi-workspace support. The original 7.7
design kept moon-ide as one Tauri process with one webview
window per workspace. The frontend's `wsInvoke` wrapper
auto-injected a `workspaceId` into every IPC call, the backend
held an `Arc<WorkspaceRegistryMap>` keyed by slug, and a shared
`resolve_workspace(state, &workspace_id)` helper plus a
`workspace_id: WorkspaceId` parameter on every Tauri command
routed each call to the right registry.

Threading the id through the surface worked. The singletons
under it didn't:

1. **Coder.** `CoderHandle` captures one
   `Arc<WorkspaceRegistry>` at startup. Every coder panel,
   regardless of which window it lives in, runs against
   _that_ registry's folders. The "huggingface" window's
   coder was operating on the "default" workspace's tree
   because the bootstrap registry happened to win the
   capture.
2. **LSP.** `LspBroker` is one process-wide singleton that
   re-points on every active-folder change. Two windows
   editing files in different folders caused thrashing on
   every focus switch.
3. **Fs watcher.** Same shape — one watcher, one watched
   tree, repointed on focus.
4. **Format-on-save shell resolver.** Captures one
   registry at startup, same failure mode as coder.

Two paths out:

- **(C) Make every singleton registry-map-aware.** Touch
  `moon-coder`, `moon-core` LSP, `moon-core` fs, the
  shell resolver, and every command currently consuming
  them. Add per-window dispatch state, route events via
  `emit_to`, refactor the watcher to support N
  concurrent trees, etc. Wide blast radius across crates
  the team isn't otherwise touching.
- **(D) One OS process per workspace.** The OS already
  handles per-process isolation; Tauri is just a window
  in that picture. Each process owns one workspace,
  one registry, one coder, one LSP broker, one fs
  watcher. No cross-workspace state inside any one
  process — by construction.

Path D collapses the IPC surface (no more `workspace_id`
parameters) and removes the singletons-coordinating-
themselves problem entirely. The cost is an inter-process
focus protocol so `Ctrl+Shift+O → existing workspace`
brings the right window forward instead of spawning a
duplicate.

## Decision

**One OS process per workspace.** Multi-window is
multi-process; the OS is the coordinator.

Concretely:

- The IDE binary parses `--workspace <slug>` from
  `std::env::args()` in `lib::run` **before** any Tauri
  machinery starts. Bare `moon-ide` (no argument) spawns
  a child `moon-ide --workspace <slug>` for the slug whose
  `last_active_at` is largest in the catalog, then returns
  from `run()` — `tauri::Builder` is never touched, so the
  launcher process never creates a webview window. An empty
  catalog drops into preboot mode (a dedicated landing card;
  no workspace state is initialised).
- Backend state is collapsed: `AppState.workspaces` is
  `Arc<WorkspaceRegistry>` again — one workspace per
  process. `WorkspaceRegistryMap` is removed from
  moon-core. Coder, LSP, fs watcher, shell resolver
  attach to the single registry without ceremony.
- IPC is collapsed: every `workspace_id` parameter is
  removed from Tauri commands (`fs_*`, `lsp_*`,
  `search_*`, `container_*`, `project_compose_*`,
  `editorconfig_*`, `session_*`, `coder_*`, `terminal_*`,
  `workspace_*`). The frontend's `wsInvoke` helper is
  deleted; every call is plain `invoke`. The frontend
  asks the backend once at boot via a new `app_info`
  command for `(mode, workspace_id, workspace_name)`
  and caches that for the process's lifetime.
- Single-instance enforcement + cross-process focus IPC
  uses a Unix domain socket bound at
  `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/run/instance.sock`
  (the `run/` subdir is per [ADR 0026](0026-socket-dir-mount.md);
  earlier it sat directly under `<id>/`).
  The owning process binds the socket on startup and
  spawns a listener that focuses `main` on any received
  byte. A would-be sibling probes the socket; success
  means a live owner — send `focus`, exit. Failure with
  `ECONNREFUSED` (or analogous) means the file is stale
  — unlink, then bind ourselves. On graceful exit,
  `RunEvent::ExitRequested` removes the socket file.
- `window_open(slug)` becomes "focus existing process or
  spawn `moon-ide --workspace <slug>` child";
  `window_close()` calls `app.exit(0)`. Multi-window
  becomes multi-process.
- `workspace_delete` refuses if a sibling process holds
  the workspace's instance lock, and refuses for the
  caller's own workspace.

Per AGENTS.md "no premature migrations", we did **not**
write any compatibility shim for the prior in-process
multi-window design. The `workspaces: Vec<WorkspaceMeta>`
catalog format on disk is unchanged, so the user's
catalog survives the pivot; their last UI session
(`AppState.last_session`-style data per workspace) is
already per-workspace `session.json` from 7.5.

## Consequences

**What gets simpler:**

- Coder, LSP, fs watcher, format-on-save all "just
  work" per workspace — they target the process's
  single registry, no fanout, no thrash.
- No `workspace_id` plumbing on the IPC surface; the
  frontend type signatures lose a parameter; no
  `wsInvoke` indirection.
- Tauri events (`container:state`, `coder:event`,
  `compose:logs:line`, etc.) are naturally
  workspace-scoped because there's only one webview
  in the process.
- `AppState` shrinks: no `bootstrap_window_workspace`
  stash, no map-of-registries.

**What gets more complex:**

- The launcher dance. A bare `moon-ide` invocation
  spawns a child with `--workspace <slug>` and exits
  before Tauri starts. The launcher process is visible
  in `ps` for a fraction of a second but never creates
  a window, so the user only sees the child's splash
  → workspace, not a duplicate.
- Dev mode (`bun run dev`) can't fork. The vite dev
  server is supervised by the parent `tauri dev`
  process and gets torn down whenever this binary
  exits, so a forked child has nothing to load. Two
  knock-on simplifications in debug builds:
  - The launcher's "spawn child, exit" path is
    replaced by "bind the lock and run inline"; one
    workspace per `bun run dev` session.
  - `window_open(slug)` refuses with a clear error
    when asked to spawn a sibling. The frontend
    surfaces the error as a toast that tells the dev
    to quit and re-`bun run dev`; the catalog's
    `last_active_at` is bumped on the way out so the
    next launch lands on the workspace the dev just
    asked for. Multi-workspace flow testing
    therefore happens against a release build, which
    matches the production behaviour anyway.
- The focus socket is platform-specific (Unix domain
  sockets on Linux/macOS). Windows would need a
  named-pipe shim. The team develops on Linux and
  the rest of the IDE works on macOS, so we ship
  Linux/macOS now and defer Windows. A second
  process per `Ctrl+Shift+O` invocation on Windows
  isn't catastrophic — just suboptimal — until the
  shim lands.
- "Workspace switching" in the UI is now spawning a
  new OS process. Cold-start cost (Tauri webview
  init, Rust state hydration, maybe a docker-compose
  reconciliation) is paid every time the user
  switches to a workspace whose process isn't
  already running.

**What is no longer a thing:**

- Phase 7.7's "Known limitation: coder pinned to
  bootstrap workspace". Gone — every process has its
  own coder.
- Phase 7.7's "fs watcher / LSP broker thrash on
  rapid window switches". Gone — separate processes,
  separate watchers.
- The `default` workspace as a forced bootstrap
  fallback. Preboot mode handles "no workspaces
  yet" cleanly; the user names their first
  workspace explicitly.
- The `bootstrap_window_workspace` AppState slot,
  the `BOOTSTRAP_WINDOW_LABEL` constant, the
  `ensure_workspace_registry` helper, and the
  `window_workspace` IPC command. All removed.

## Alternatives considered

- **Path C (make singletons registry-map-aware).**
  Rejected: wide refactor across `moon-coder`,
  `moon-core` LSP, fs watcher, format-on-save shell
  resolver, plus per-window event dispatch. The
  payoff (saving one OS process per workspace)
  doesn't justify the touch radius for a team-of-N
  internal IDE.
- **Tauri's `tauri-plugin-single-instance`.** It
  enforces "one process for the whole binary",
  which is the opposite of what we want — we
  explicitly want N processes, one per workspace,
  with focus IPC keyed on the slug. The plugin's
  socket is binary-wide; ours is workspace-wide.
- **Keep the `default` workspace as a hard-coded
  bootstrap.** Was the original 7.x shape. Rejected
  by the user explicitly: in normal flow there
  should not be a `default` workspace unless the
  user typed it themselves. Preboot mode replaces
  it.
- **DBus / Tauri's IPC plugin / a HTTP loopback for
  focus.** Overkill for a one-byte "focus" message
  between processes that already share a writable
  state directory. A Unix domain socket file in
  the per-workspace state dir is the natural
  granularity — it lives where the workspace
  lives.
