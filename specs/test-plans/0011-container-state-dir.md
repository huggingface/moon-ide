# Test plan 0011: Container state dir + multi-folder mounts

- **Date**: 2026-04-29
- **Phase**: Phase 2.0.5 — workspace ≠ folder (the
  [ADR 0007 amendment](../decisions/0007-compose-and-moon-base.md#amendment-2026-04-29--state-dir-and-multi-folder-mounts))

## What shipped

- Workspace container state moved out of any specific repo to
  `<dirs::data_local_dir>/moon-ide/workspaces/<id>/{compose.yaml,bound-folders.json}`
  (with `<id> = "default"` until multi-workspace lands).
- Compose project name is now the constant `moon-ws-default`
  rather than `moon-ws-<hash-of-folder>`. Survives folder
  switches and folder add / remove.
- `compose.yaml` is generated from the bound-folder list
  every time it changes. Volumes use absolute host paths,
  one entry per bound folder mounted at `/workspace/<basename>`.
  `working_dir: /workspace`. Includes (discovered project
  compose files) use absolute paths now too.
- Folder add (sidebar `+`, welcome) and folder remove (per-bar
  `×`) trigger `container_apply_bound_folders`. The backend
  rewrites both files; if the compose project happens to be
  `Running`, it follows up with `docker compose up -d --wait`
  so the dev container is recreated against the new mount
  list. `Absent` / `Paused` / `Stopped` / `Failed` are pure
  file-rewrites — no surprise daemon work while the user
  has them paused on purpose.
- Folder switch is a zero-cost UI swap; the container pip is
  no longer reset on each switch.

## How to test

Prerequisites:

- `bun install`
- Tauri dev deps per `README.md`.
- Docker Engine + Compose v2 on the host.
- A locally built `moon-base:dev` image
  (`docker build -t moon-base:dev images/moon-base/`).
- Have at least two real folders to point at — e.g.
  `~/code/moon-landing` (carries `docker-compose.yml`) and
  `~/code/moon-ide` (no compose).

### A. Fresh state dir on first opt-in

1. Wipe both the persisted UI state and any previous workspace
   state dir:
   - `rm -f ~/.config/moon-ide/state.json`
   - `rm -rf ~/.local/share/moon-ide/workspaces/`
     (paths assume Linux; adjust for `app_config_dir()` /
     `dirs::data_local_dir` on your platform).
2. `bun run tauri dev`. Expected: welcome screen.
3. Add one folder via "Open folder" (e.g. `~/code/moon-ide`).
4. From a host terminal (not the IDE):
   `ls ~/.local/share/moon-ide/workspaces/default/`. Expected:
   the directory is **absent** — the IDE doesn't materialise
   files until the user opts in.
5. Click the container pip → "Set up". Expected: status pip
   cycles through `setting up…` → `running`.
6. Re-run the `ls`. Expected: `compose.yaml` and
   `bound-folders.json` are now present.
7. `cat ~/.local/share/moon-ide/workspaces/default/bound-folders.json`.
   Expected: a JSON object with a `folders` array containing the
   absolute path of the folder you added.
8. `cat ~/.local/share/moon-ide/workspaces/default/compose.yaml`.
   Expected:
   - Header `name: moon-ws-default`.
   - `services.dev.working_dir: /workspace`.
   - One volume entry of the form
     `<absolute>:/workspace/<basename>`.
   - No `../` prefixes anywhere.
9. `docker compose ls`. Expected: a `moon-ws-default` project
   listed as running.

### B. Add a second folder while running

1. With the project still running from §A, add another folder
   via the sidebar `+` button (e.g. `~/code/moon-landing`).
2. Status pip should briefly show `setting up…` again as
   `compose up -d --wait` recreates `dev` with the new mount,
   then settle on `running`.
3. From a host terminal:
   `cat ~/.local/share/moon-ide/workspaces/default/compose.yaml`.
   Expected:
   - Two `<absolute>:/workspace/<name>` volume entries.
   - An `include:` block with the absolute path to the
     second folder's `docker-compose.yml` (if it has one).
4. `docker compose -p moon-ws-default exec dev ls /workspace`.
   Expected: both folder names visible as subdirectories.

### C. Switching active folder doesn't touch the container

1. With both folders bound and the project running, click
   between the two folder bars in the sidebar.
2. Status pip should stay `running` — no `setting up…`
   flicker, no transient `null` state.
3. Open a terminal and `docker events --filter
'event=stop' --filter 'event=start'` running in the
   background. Switching folders should produce **no** new
   events (folder switch is a UI-only swap).

### D. Remove a folder while running

1. Hover the second folder's bar; click `×`; confirm the
   prompt.
2. Status pip cycles `setting up…` → `running` as `dev` is
   recreated with the smaller mount list.
3. `cat ~/.local/share/moon-ide/workspaces/default/compose.yaml`
   shows just the remaining bind mount.
4. `docker compose -p moon-ws-default exec dev ls /workspace`
   shows just the remaining folder. (Project services from
   the removed folder may still be running — that's expected;
   the auto-`--remove-orphans` is deferred.)

### E. Add / remove while not running

1. Tear down the project: container popover → "Tear down".
   Status pip should go to `not set up`.
2. Add a folder. Expected: no daemon round-trip, no status
   change. `cat compose.yaml` should reflect the new bound
   folder anyway (the file is rewritten so the next "Set up"
   is correct).
3. Remove a folder. Same expectations.
4. Click "Set up" again. Expected: project comes up with the
   updated mount set.

### F. Restart-survives-restart

1. With the project running, close moon-ide.
2. From a host terminal: `docker compose ls`. Expected: the
   `moon-ws-default` project still listed (compose down on
   close is not the policy; the IDE pauses, deferred to the
   pause-on-close logic).
3. Relaunch `bun run tauri dev`. Expected: status pip ends
   on `running` after a moment (or `paused` if the IDE
   paused on close), without going through `setting up…`
   — the container survived the restart.

### G. Compose preview reflects current mounts

1. Open the container popover → "Inspect compose.yaml".
   Expected: the rendered preview matches what's on disk.
2. Add a folder. Re-open "Inspect". Expected: the preview
   now includes the new mount (the cache invalidates on
   bound-folder change).

### H. Stale `moon-ws-<hash>` projects from before 2.0.5

This is a one-time cleanup obligation, not a regression.
Anyone who used the IDE pre-2.0.5 will still have stale
`moon-ws-<hash-of-folder>` projects on the daemon:

1. `docker compose ls`. Look for any `moon-ws-` project
   whose name isn't `moon-ws-default`.
2. For each one:
   `docker compose -p <name> down --remove-orphans`.

Future work (not in this test plan): a "container management"
surface in the IDE that lists every `moon-ws-*` project and
offers bulk teardown — see
[`containers.md` § Inventory + GC](../containers.md#inventory--gc).

## Known limitations

- **Basename collisions**. Two bound folders with the same
  basename (e.g. `~/code/api/web` and `~/projects/api/web`)
  would both want `/workspace/web`. The frontend doesn't
  refuse the add today; the second folder's mount silently
  shadows the first under compose semantics. Cheap fix when
  the case actually shows up: append a numeric suffix
  (`web-2`) at registry time.
- **Multi-folder discovery is shallow per folder**.
  `discover_compose_files_for_folders` walks each folder and
  one level of subdirectories, no deeper. Project compose
  files in deeply nested layouts need a top-level
  `compose.yaml` that `include:`s them.
- **No automatic orphan removal**. Removing a folder leaves
  any project services it brought along running until the
  user explicitly tears down or rebuilds. Acceptable for now;
  visible in `docker compose ps`.
