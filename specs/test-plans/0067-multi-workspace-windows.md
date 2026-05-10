# Test plan 0067: multi-workspace windows

- **Date**: 2026-05-10
- **Phase**: Phase 7 — sub-phases 7.6 → 7.9 (process-per-workspace shape)

## What shipped

- Multi-workspace as **multi-process**: each workspace runs in
  its own `moon-ide --workspace <slug>` OS process. Backend
  singletons (coder, LSP broker, fs watcher, format-on-save
  shell resolver) attach to the process's single workspace —
  no cross-workspace state in any one process.
- CLI: `--workspace <slug>` argument parsed in `lib::run`
  before any Tauri machinery starts. Bare `moon-ide` spawns
  a child process with the most-recently-used catalog slug
  and exits before creating a window — the user never sees
  a launcher splash. An empty catalog drops into preboot
  mode.
- Single-instance enforcement + cross-process focus over a
  Unix domain socket at
  `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/instance.sock`.
  Stale sockets (left by a crash) auto-recover on next launch.
- New IPC: `workspace_create` / `workspace_delete` /
  `workspace_rename` / `workspace_catalog`, with `[a-z0-9_-]`
  slug validation and best-effort name → slug derivation in
  `moon-protocol`. `workspace_create` is **idempotent
  create-or-switch**: an existing slug returns the existing
  entry (with `last_active_at` bumped) so `Ctrl+Shift+N`
  with a name that already exists focuses that workspace
  instead of erroring. Plus `app_info` returning the
  process's mode (`Workspace { id, name }` or `Preboot`) —
  the frontend's single source of truth for "which
  workspace am I" replacing the deleted `wsInvoke` /
  `currentWorkspaceId` threading.
- `window_open(slug)` focuses the sibling process holding
  that slug's instance lock, or spawns a fresh
  `moon-ide --workspace <slug>` child if there isn't one.
  `window_close()` exits the calling process.
- Welcome screen shows the workspace name + shortcut hints.
  Hardcoded keybindings: `Ctrl+Shift+N` (create workspace),
  `Ctrl+Shift+O` (picker palette), `Ctrl+Shift+A` (add
  folder), `Ctrl+Shift+W` (close current workspace process).
  "Forget workspace" lives in the picker, hidden for the
  caller's own row; the backend also refuses if any sibling
  process holds the workspace's instance lock.
- Preboot mode: an empty catalog renders a dedicated
  `PrebootLanding` card asking for the first workspace's
  name. Submitting creates the workspace, spawns its child
  process, and exits the preboot process.
- Title bar resyncs `<workspace> — <folder>:<branch>` per
  process via a Svelte `$effect`.
- Phase 7.9 launcher: bare `moon-ide` re-execs itself with
  the slug whose `last_active_at` is largest, then exits.
  `last_active_at` bumps on `window_open`, `workspace_create`,
  and every `session_save` tick.
- **Cross-process write coordination**:
  `moon_core::app_state::mutate(config_dir, F)` is the only
  way to write `state.json`. It takes an exclusive
  `flock(2)` on `state.json.lock`, loads the file, runs the
  caller's mutator, and atomically renames a temp file over
  the live one. Sibling `moon-ide` processes — and
  concurrent commands inside one process (`session_save`'s
  bump tick vs. `workspace_create` vs. slack writes) — are
  serialized through it. Without this, every writer's
  `load → modify → save` was a lost-update race: process A
  bumping `Hugging Face` could overwrite the entry process
  B had just appended for `Boardgamers`, so window A's
  picker would never see the new workspace. Readers
  (`workspace_catalog`, `app_state_load`) keep using
  unlocked `load()` — the atomic rename guarantees they
  always see a complete document, never a torn write.

## How to test

Prerequisites: `bun install`, Docker daemon running (for any
container-touching steps — pure window/UX flow needs only
Tauri). Wipe `~/.config/moon-ide/state.json` and
`~/.local/share/moon-ide/workspaces/` for a clean preboot
test.

1. **Preboot first launch**. With a wiped state directory, run
   `bun run dev` (or `cargo tauri dev`). Expected: a window
   opens with the "Name your workspace" landing card; no file
   tree, no welcome screen, no menu items beyond what the
   landing renders. Type "Hugging Face" and submit.
   Expected: the preboot window closes and a fresh
   `Hugging Face` window opens (title bar reads
   `Hugging Face`, no folder yet).
2. **Add a folder**. In the `Hugging Face` window press
   `Ctrl+Shift+A`. Pick any folder. Expected: the title
   updates to `Hugging Face — <folder>:<branch>` once git
   resolves; the welcome screen is replaced by the editor
   shell.
3. **Create a second workspace from a running one**. Press
   `Ctrl+Shift+N`. Type "Gitaly". Submit. Expected: a fresh
   `Gitaly` process spawns (verify with `pgrep -af moon-ide`
   — two processes, two `--workspace` args). The original
   `Hugging Face` window stays where it was, untouched.
4. **Picker**. From either window press `Ctrl+Shift+O`.
   Expected: the palette shows both workspaces,
   most-recent-active first (`gitaly` if it was just
   created). The caller's own row has no Forget button. The
   other row shows Forget.
5. **Focus existing process**. From the `Hugging Face`
   window, press `Ctrl+Shift+O`, select `Gitaly`. Expected:
   the existing `Gitaly` process's window comes to the
   front (no second `Gitaly` process spawns; verify
   `pgrep -af moon-ide` count stays the same).
6. **Spawn fresh after close**. Quit the `Gitaly` window
   (window-close button or `Ctrl+Shift+W`). Verify the
   `Gitaly` process exits (`pgrep -af moon-ide` shrinks).
   From `Hugging Face` press `Ctrl+Shift+O` → `Gitaly`.
   Expected: a brand-new `Gitaly` process spawns and
   restores whatever folders were bound there.
7. **Restore most recent on bare launch**. Quit every
   moon-ide window. Verify all `moon-ide` processes are
   gone. Run `bun run dev` again with no `--workspace`
   argument. Expected: a transient launcher process
   spawns a `moon-ide --workspace <slug>` child for the
   most-recently-active slug (whichever you last had
   focused) and exits before any window is shown — so
   the user sees exactly one splash screen, the child's,
   transitioning into the restored workspace. Only one
   `moon-ide` process ends up running.
8. **Catalog write coordination**. From the `Hugging Face`
   window press `Ctrl+Shift+N` and create `Boardgamers`.
   Without quitting `Hugging Face`, switch back to it and
   press `Ctrl+Shift+O`. Expected: `Boardgamers` shows up
   in the picker — the catalog write from the
   `Boardgamers` process and any periodic
   `bump_last_active` tick from `Hugging Face` are
   serialized through `app_state::mutate`'s flock, so
   neither clobbers the other.
9. **Forget workspace**. Open the picker from
   `Hugging Face`. Click Forget on the `Gitaly` row.
   Expected: the row disappears; if a `Gitaly` process is
   still running the action is refused with a clear toast
   (close `Gitaly` first), otherwise the compose project
   `moon-ws-gitaly` is gone from `docker ps`,
   `~/.local/share/moon-ide/workspaces/gitaly/` no longer
   exists, and the `gitaly` row stays gone after refresh.
10. **Forget current**. Try forgetting the caller's own
    workspace. Expected: no Forget button on that row at
    all (the picker hides it).
11. **Stale socket recovery**. Crash a workspace process
    (e.g. `kill -9 $(pgrep -f 'moon-ide --workspace gitaly')`).
    Verify `instance.sock` is left behind under
    `~/.local/share/moon-ide/workspaces/gitaly/`. Open
    `Gitaly` again via the picker. Expected: a fresh
    process spawns cleanly (no "address in use" error);
    the stale socket file is unlinked and a new one is
    bound.
12. **Container set-up / shutdown still works**. In any
    workspace window, set up the container (`Ctrl+P`
    "compose: up" or whichever surface the team uses),
    open a host terminal and a container terminal, edit
    a file, save it. Quit the window. Expected: the
    graceful `compose stop` path still runs on
    `ExitRequested`; `docker ps` shows the workspace's
    services going down.

## What must keep working

- Single-workspace Phase 2.5 flow inside any one process:
  open folder, add a second folder, switch active folder,
  close folder.
- Container set-up / teardown / pause / resume.
- Coder panel + format-on-save in any workspace process —
  these now reliably target the process's own workspace
  (the prior limitation where they pinned to the bootstrap
  workspace is gone).
- LSP, fs watcher, and git status in any active window.

## Known limitations

- **`bun run dev` is single-workspace per session.**
  Vite is supervised by the parent `tauri dev` process,
  so forked children can't load the frontend. In debug
  builds the launcher binds the most-recently-used slug
  inline (or preboot for an empty catalog), and
  `Ctrl+Shift+N` / `Ctrl+Shift+O` show an error toast
  asking the dev to quit and re-run `bun run dev` —
  the picked / created workspace is bumped to the top
  of `last_active_at` on the way out, so the next
  launch restores it. **Multi-workspace flow testing
  must use a release build** (`cargo tauri build`,
  then run the produced binary). Do steps 3–10 below
  against the built binary, not `bun run dev`.
- **Linux/macOS only for the focus socket** (Unix domain
  sockets). Windows users would currently spawn a second
  process per `Ctrl+Shift+O` invocation. The team develops
  on Linux; a Windows shim using named pipes is a future
  phase if anyone asks.
- **No CLI handoff** (`moon /path/to/folder` opening a
  folder in an existing process). Out of scope for Phase
  7.9 — defer to a follow-up phase that adds the CLI
  binary plus a per-workspace folder-add IPC.
- Per-process events (`container:state`, `coder:event`,
  `compose:logs:line`) are emitted to that process's
  webview only — naturally workspace-isolated now since
  there's only one webview per process.

## Related

- Specs: [phase-07-multi-workspace.md](../roadmaps/phase-07-multi-workspace.md), [phase-02.5-multi-folder.md](../roadmaps/phase-02.5-multi-folder.md)
- ADRs: [0007 — workspace state dir](../decisions/0007-workspace-state-dir.md)
- Prior test plans: [0010-multi-folder-workspace.md](0010-multi-folder-workspace.md), [0011-container-state-dir.md](0011-container-state-dir.md)
