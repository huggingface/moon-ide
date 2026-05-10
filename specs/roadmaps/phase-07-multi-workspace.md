# Phase 7 — Multi-workspace

The "command centre" plural form. Today moon-ide is one window with
one workspace ([Phase 2.5](phase-02.5-multi-folder.md)) holding N
folders. This phase grows that to N **named** workspaces, each in
its own OS window, persisted independently, swapped via a workspace
picker.

The motivating use case: the team manages several platforms — one
window per platform, each window holding the folders that belong to
that platform. A "huggingface" window with `moon-ide` /
`moon-landing` / `moon-base` open at once, a separate "gitaly"
window with a different folder set, etc. Cross-workspace state
(theme, AI creds, sign-in) stays per-machine; per-workspace state
(open tabs, splits, container, coder history) is isolated by
window.

This is **not** the same thing as cross-folder search — that's still
[Phase 7's "multi-repo coordination" sibling concern](../roadmap.md#phase-7--multi-repo-coordination)
and gets its own roadmap doc when it grows. The two ship
independently.

## Acceptance

Per sub-phase. Land in order; stop at each gate.

### 7.1 — Registry takes a real id (no UI change)

- `WorkspaceRegistry` carries a `WorkspaceId` field instead of
  reading a constant inside `snapshot()`. The id is what the
  registry reports to anything downstream (compose project
  name, container labels, state dir).
- Pure structural prep so a later phase can swap in user-set
  slug ids by changing the constructor call site, not the
  registry.

### 7.2 — Workspace catalog plumbing (no UI change)

- `AppState` grows `workspaces: Vec<WorkspaceMeta>` (id, name,
  `last_active_at`). The catalog lives at
  `<XDG_CONFIG_HOME>/moon-ide/state.json` alongside the
  existing app state.
- `WorkspaceMeta` is the new protocol type.
- No new IPC. No frontend reads the catalog yet. Pure
  forward-compat data.

### 7.3 — workspace_id wired into the runtime (no UI change)

- During the original 7.x rollout, a `WorkspaceRegistryMap`
  briefly held N registries inside one Tauri process and
  every command threaded a `workspace_id` parameter through
  to look one up. **The 7.7 pivot to process-per-workspace
  removed both** — see [7.7](#77--process-per-workspace).
  Each process owns exactly one `Arc<WorkspaceRegistry>`,
  command signatures don't carry `workspace_id`, and the
  frontend resolves the id once at boot via `app_info`.

### 7.4 — workspace_id IPC threading (superseded by 7.7)

- Originally: every command grew an explicit `workspace_id:
WorkspaceId` argument and the frontend wrapped
  `@tauri-apps/api/core` in a `wsInvoke` helper that
  auto-injected the current id.
- **Superseded by 7.7**: with one process per workspace, the
  id is implicit. Every command on the surface lost its
  `workspace_id` parameter; `wsInvoke` was deleted; plain
  `invoke` is used everywhere.

### 7.5 — Per-workspace session.json (no UI change)

- `AppState.last_session` (the per-workspace folder list /
  active folder / open tabs blob originally living in the
  global state) moves to
  `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/session.json`. The
  global slot goes away — per AGENTS.md "no premature
  migrations", we wipe it; the user re-opens their folders
  once.
- `moon_core::session` module with `load(workspaces_dir,
workspace_id)` / `save(workspaces_dir, workspace_id,
session)`; on parse failure logs and falls back to a default
  empty session, never crashes.
- `session_load` / `session_save` Tauri commands. Both take
  no `workspace_id` argument after 7.7 — they use the
  process's own workspace.
- Frontend's `restoreAppState` fetches the session in parallel
  with `app_state_load`; persist tick saves the session blob
  through the new IPC instead of stuffing it into the
  `AppState` payload. `AppState` loses its `last_session`
  field entirely.
- Bootstrap reads
  `<workspaces_dir>/<process-workspace-id>/session.json`
  before the first paint so the initial `workspace_active`
  call already sees the restored folder list — same UX as
  before, different storage layout.

### 7.6 — workspace_create / \_delete / \_rename IPC (no UI change)

- IPC: `workspace_create(slug, name)`,
  `workspace_delete(slug)`, `workspace_rename(slug, name)`.
  Slug validated against `[a-z0-9_-]` (the charset Docker
  compose project names accept, so `moon-ws-<slug>` is always
  valid).
- `workspace_create` writes the new catalog entry, or — if
  the slug is already in the catalog — returns the existing
  entry with `last_active_at` bumped (idempotent
  create-or-switch). The frontend's `Ctrl+Shift+N` flow
  pairs it with `window_open` so typing an existing name
  focuses / re-opens that workspace instead of erroring.
  `workspace_delete` removes the catalog entry, the
  per-workspace state dir, and the compose project
  (`docker compose down`); after 7.7 it also refuses if a
  sibling process holds the workspace's instance lock or if
  the caller's own process is the workspace's owner.
- The create / delete / rename surface is in place for 7.8's
  picker to call.

### 7.7 — Process per workspace

After 7.1–7.6 landed in their original "single Tauri process,
N webview windows" form, the next round of integration testing
made the boundary obvious: every backend singleton (coder, LSP
broker, fs watcher, format-on-save shell resolver) was pinned
to whichever workspace booted first, so the second window's
coder operated on the first window's folder set. Threading
`workspace_id` through every singleton would have meant
either making the singletons registry-map-aware (a wide
refactor across moon-coder + moon-core LSP) or accepting that
"multi-window" was actually "single-workspace with extra
chrome".

Instead, **one OS process per workspace**. Multi-window
becomes multi-process; the OS handles everything we'd
otherwise have to coordinate in-app:

- The CLI grows `--workspace <slug>` (parsed in
  `lib::run` before Tauri starts). Bare `moon-ide` spawns
  a child process with the most-recently-used slug from
  the catalog and exits before any window is created.
  An empty catalog drops into preboot mode.
- The backend's `AppState.workspaces` collapses back to
  `Arc<WorkspaceRegistry>` — one workspace per process, no
  map. Coder, LSP, fs watcher, shell resolver attach to
  it cleanly. `WorkspaceRegistryMap` is removed from
  moon-core; no migration code (no installed base).
- Every `workspace_id` parameter is removed from Tauri
  commands. The frontend's `wsInvoke` wrapper goes away;
  every IPC is a plain `invoke`. `currentWorkspaceId()`
  reads the answer once at boot from a new `app_info`
  command (CLI arg → constant for the rest of the
  process's life).
- **Single-instance lock** + **focus IPC** lives at
  `<workspaces_dir>/<id>/instance.sock` (Unix domain
  socket on Linux/macOS; Windows is deferred). The owning
  process binds the socket on startup and listens for
  one-byte focus messages; a sibling launcher trying to
  bind the same slug detects the conflict, sends focus,
  and exits. Stale sockets (left by a crash) are
  auto-recovered with a probe-then-unlink dance.
- `window_open(slug)` becomes "focus existing process or
  spawn `moon-ide --workspace <slug>` child". The picker
  and the WorkspaceCreate modal both go through it.
- `window_close()` exits the calling process. The OS
  reaps the rest. Tauri's `ExitRequested` hook still runs
  the graceful `compose stop` path on the way out and
  cleans up the focus socket.
- `workspace_delete` refuses if a sibling process holds
  the workspace's instance lock. The caller's own
  workspace is also refused (don't delete what your
  window is showing).

### 7.8 — Creation + picker UX

- Empty-workspace UX (workspace mounted, but zero folders
  bound) shows the workspace name, a single "Open folder…"
  button, and shortcut hints for `Ctrl+Shift+O` /
  `Ctrl+Shift+N` / `Ctrl+Shift+A`. Hardcoded shortcuts; no
  config surface.
- Empty-catalog UX (preboot mode — no `--workspace` CLI arg
  and the catalog is empty) shows a dedicated landing card
  asking for the first workspace's name. Submitting creates
  the workspace, spawns a child `moon-ide --workspace
<slug>`, and exits the preboot process. No other chrome
  renders in preboot mode.
- `Ctrl+Shift+N`: in-window modal asking for a workspace
  name. Submitting calls `workspace_create` (slug
  auto-derived from name) then `window_open(slug)`.
  Create-or-switch semantics: typing an existing name
  focuses / re-opens that workspace instead of erroring.
  In preboot mode, the calling process additionally exits;
  in workspace mode it stays where it was.
- `Ctrl+Shift+O`: workspace picker palette listing every
  catalog entry, most-recently-active first, with live
  filter. Selecting a row calls `window_open(slug)` —
  focus existing process or spawn fresh. Each row has a
  "Forget" affordance that calls `workspace_delete`,
  hidden for the caller's own workspace; the backend
  refuses for any workspace whose instance lock is live.
- `Ctrl+Shift+A`: same folder picker the welcome screen's
  "Open folder" button uses.
- `Ctrl+Shift+W`: `window_close()` (exits the process).

### 7.9 — Restore-most-recent

- On launch, a bare `moon-ide` (no `--workspace` CLI arg)
  re-execs itself with the slug whose `last_active_at` is
  largest in the catalog, then exits. The launcher never
  shows a window of its own; the second process owns the
  user-visible window.
- Empty catalog → preboot mode (above).
- Activity tracking bumps `last_active_at` on
  `workspace_create`, `window_open` (focus or spawn), and
  every `session_save` tick. Best-effort: a write failure
  is logged via `tracing::warn!` but doesn't abort the
  calling command — the picker's recency sort just stays
  slightly stale.
- "Forget workspace" lives in the picker (per-row button).
  The caller's own workspace is hidden from the action;
  the backend refuses for any workspace currently held by
  a sibling process's instance lock.
- **Out of scope for Phase 7.9** (deferred): CLI handoff
  (`moon /path` opening a folder in an existing process).
  The user can launch via the app shortcut and use
  `Ctrl+Shift+A`.
- **Out of scope for Phase 7.9** (deferred): Windows
  support for the focus socket (named pipes). The team
  develops on Linux; the rest of the IDE works on macOS,
  the focus socket needs a small platform shim before
  it does too.

## Data model

### Global (per-machine, in `AppState`)

```rust
pub struct AppState {
    // ... existing fields (theme, slack, next_edit, coder, ...) ...

    /// 7.2+: known workspaces keyed by id (slug, e.g.
    /// `"huggingface"` / `"gitaly"`). The id is the
    /// `--workspace <slug>` argument the IDE process was
    /// launched with; preboot mode (no slug) renders a
    /// landing card to add the first entry.
    pub workspaces: Vec<WorkspaceMeta>,
}

pub struct WorkspaceMeta {
    /// User-set slug. Validated against the same charset
    /// `moon-ws-<id>` already enforces — Docker compose
    /// project names accept `[a-z0-9_-]`, the picker rejects
    /// the rest.
    pub id: String,
    pub name: String,
    /// Bumped on every command that touches the workspace.
    /// Drives the picker's "recent" sort and the launch-restore
    /// pick.
    pub last_active_at: i64,
}
```

`WorkspaceMeta` deliberately doesn't carry the folder list — that
goes in the per-workspace `session.json` so opening the picker
doesn't have to read every workspace's folder set.

### Why slug ids and not UUIDs

The id is what shows up in `docker ps`
(`moon-ws-huggingface`), in the per-workspace state dir
(`<…>/workspaces/huggingface/`), in the focus socket file
name (`<…>/workspaces/huggingface/instance.sock`), and in
the `moon-ide --workspace huggingface` CLI line OS process
listings show. A UUID makes all of those cryptic for no
upside — the user already names their workspaces in 7.8's
creation UI, and the slug of that name is a perfectly fine
stable id. Collisions are handled at creation time (the
picker rejects a name whose slug is already taken).

### Per-workspace (in `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/`)

- `session.json` — folders, open tabs, splits, focused folder,
  active SCM tab, coder panel state. Replaces the
  `AppState.last_session` path from Phase 2.5.
- `compose.yaml` — already exists (Phase 2).
- `bound-folders.json` — already exists (Phase 2).
- `instance.sock` — Unix domain socket bound by the owning
  process for single-instance enforcement and focus IPC.
  Auto-cleaned on graceful exit; stale files (left by a
  crash) are recovered by a probe-then-unlink dance the
  next time the workspace is opened.

## Out of scope (deliberately deferred)

- **Visual workspace differentiation in OS Alt-Tab** (coloured dot
  on the window icon). The team can disambiguate by title; revisit
  if anyone asks.
- **Per-workspace AI model preference**. Today's coder model lives
  in `AppState.coder` (per-machine). Whether it should be per
  workspace is a Phase 12+ question; until somebody asks, the
  per-machine default holds.
- **Cross-workspace coder context** ("the coder in workspace A can
  see workspace B's open tabs"). No.
- **Workspace cloning / templating**. The user creates each
  workspace from scratch.
- **Sub-workspaces / nested workspaces**. No.
