# Phase 2 — Containerised dev shells

The IDE provisions a single Docker container per workspace and
routes project tooling through it. Architecture lives in
[`containers.md`](../containers.md); this file owns the work
breakdown, sub-phase acceptance, and the open questions we
defer to specific milestones.

## Sub-phases

### 2.0 — Container plumbing

**Acceptance**: opening a workspace that has no `.moon/`
folder shows a "Run this project in a container? [Set up | Not
now]" prompt in a new "Container" status-bar pip. "Set up"
writes a default `<workspace>/.moon/compose.yaml` (with an
`include:` block for any sibling `docker-compose.yml` files
moon-ide spotted), pulls `moon-base`, and brings up the whole
compose project — the workspace's `dev` service _and_ the
included project services as siblings on the host's daemon.
Subsequent opens unpause the project; closing the window
pauses the project (compose-wide). The status pip cycles
through `absent → creating → running → paused → running`
cleanly. Concrete acceptance: opening moon-landing brings up
all eleven services (`dev` + the project's ten) on the host's
daemon with a single "Set up" click.

What ships:

- New crate `moon-container` (`crates/moon-container/`)
  ✅ — shells out to `docker compose` via `tokio::process`
  (bollard would have dragged in hyper/h2 transitively for one
  feature; the shell-out is one screen of code and stays
  honest about what compose actually accepts).
- Compose discovery + generation logic ✅
  (`crates/moon-container/src/{discovery,compose,project}.rs`):
  scans the workspace root and one level deep for
  `docker-compose.yml` / `compose.yaml`, writes
  `<workspace>/.moon/compose.yaml` with the discovered files
  in `include:` plus a `dev` service. Generation runs once on
  first opt-in; the file is user-owned thereafter.
- `moon-base` Dockerfile (✅ in tree) + GitHub Actions workflow
  (✅ [`.github/workflows/moon-base.yml`](../../.github/workflows/moon-base.yml))
  publishing a multi-arch manifest
  (`linux/amd64` + `linux/arm64`, both built natively on
  `ubuntu-24.04` / `ubuntu-24.04-arm` — no QEMU) to
  **Docker Hub** at `huggingface/moon-base`. Push events to
  `main` publish `:dev` (rolling) and `:sha-<long>` (immutable);
  PRs build and smoke-test without pushing. arm64 is the team's
  primary target (Mac-majority, Apple Silicon); amd64 covers
  Linux contributors and CI. Builds run only on changes to
  `moon-base`'s sources, not on every moon-ide commit. Versioned
  tags (`:0.1`, `:0.1.0`, `:latest`) wait until 2.0 ships
  end-to-end and we cut a real release. Image contents per
  [`containers.md`](../containers.md#the-moon-base-image),
  distribution per
  [`containers.md`](../containers.md#distribution),
  registry rationale per
  [ADR 0007](../decisions/0007-compose-and-moon-base.md#registry-docker-hub).
- Each per-arch CI job runs the ADR 0005 release gate before
  publishing: `bun install --frozen-lockfile` and
  `cargo check --workspace --locked` against a moon-ide checkout
  bind-mounted into the freshly built image. Red = no push.
- Compose project name derived from a stable hash of the
  workspace path: `moon-ws-<short-hash>`. The `dev`
  container's name follows compose's
  `<project>-<service>-<n>` convention.
- Tauri commands `container_status` / `container_setup` /
  `container_pause` / `container_resume` / `container_rebuild`
  / `container_teardown` / `container_render_compose` ✅
  (`src-tauri/src/commands/container.rs`). Each operates on the
  **whole compose project**, not just the `dev` service —
  pausing the workspace pauses included services with it.
- Push event `container:state` ✅ — broadcast after every
  lifecycle command. Logs/error events deferred to 2.4 with
  the per-service UI; 2.0's pip only needs state, and a single
  event keeps the channel narrow.
- Status-bar pip + popover that surfaces state and the action
  vocabulary appropriate to it ✅ (`src/lib/components/StatusBar.svelte`
  - `src/lib/components/ContainerPanel.svelte` +
    `src/lib/container.svelte.ts`). Includes an "Inspect
    compose.yaml" preview that calls `container_render_compose`
    so the user sees what "Set up" will write before they click.
    No redesign of the status bar itself.
- ADRs [0007](../decisions/0007-compose-and-moon-base.md)
  (compose + moon-base) and
  [0008](../decisions/0008-host-shared-daemon.md) (host-shared
  daemon, no nested Docker — the reason "Set up" creates
  sibling services rather than DinD'ing them).

What doesn't ship in 2.0:

- Routed execution. Until 2.1, terminals/LSPs/etc. still run
  on the host. 2.0's "running" container just sits there with
  `sleep infinity` and confirms the lifecycle works.
- Per-service UI (start/stop/restart `mongo` independently of
  the rest, per-service log tails, etc.). 2.0's status pip
  shows compose-project-level state only; richer per-service
  UI lands in 2.4.
- Port forwarding UI (just whatever compose declares natively).
- Devcontainer.json reading.

Test plan: `0014-container-lifecycle.md` (TBD at implementation
start). Includes opening moon-landing as a smoke target.

### 2.1 — Routed execution

**Acceptance**: with the container running, opening a terminal
runs inside the container (tested by `hostname` returning the
container's name); LSPs from Phase 4 (when they exist) launch
inside the container; lint/format sidecars from Phase 8 (when
they exist) run inside; `cargo build` / `bun install` run
inside; the host's `apt`/`bun`/`cargo` global state is
untouched after a workspace session.

What ships:

- `WorkspaceHost::ContainerHost` impl in `moon-container`:
  - fs ops delegate to `LocalHost` (bind-mount means same
    bytes — see [containers.md](../containers.md#workspacehostcontainerhost)),
  - `spawn` shells out via `docker exec`,
  - `open_pty` allocates a host-side PTY and bridges it to
    `docker exec -it`,
  - `watch` uses host-side `notify`.
- `PathMapping` helper in `moon-container::path` — translates
  host paths ↔ container paths everywhere a value crosses the
  boundary.
- `WorkspaceState`'s active host pointer flips to
  `ContainerHost` while the container is `running` and back
  to `LocalHost` while it isn't (paused workspaces fall back
  to host-side fs reads, container-side spawns are blocked).

What doesn't ship in 2.1:

- LSP / terminal / lint integration **as new features** — they
  arrive in their own phases (3, 4, 8). 2.1's job is to make
  the trait variant available so those phases inherit container
  routing for free.

Test plan: `0015-container-routed-exec.md` (TBD).

### 2.2 — Port forwarding UX

**Acceptance**: a "Ports" surface (sidebar or status-bar
section — pick at implementation) lists every active forward
declared in `compose.yaml` with state (✓/✕ on host port
availability), URL, and the originating in-container port.
Clicking a row opens the URL in the host's default browser.
Conflicts (host port busy) surface with a Retry button instead
of silent failure.

What ships:

- A docker events watcher that updates the forward map on
  start/stop.
- A small "Ports" UI region.
- Host-port conflict detection on container start.

What doesn't ship in 2.2:

- On-demand forwarding ("forward this port now without editing
  compose"). Defer until somebody actually wants it; the
  workflow today is "edit compose, rebuild" which is fine.

Test plan: `0016-container-ports.md` (TBD).

### 2.3 — Devcontainer.json interop

**Status**: design pending. Ship 2.0–2.2 first, then debate.

The question: a workspace contains a repo with an existing
`.devcontainer/devcontainer.json` (because that repo is shared
with non-moon-ide users — Codespaces, VSCode). What does
moon-ide do?

Options to debate:

- **A. Translate on open**: synthesise a compose service from
  `image` / `dockerFile` / `forwardPorts` / `mounts` /
  `remoteUser` / `containerEnv` and stitch it into the
  workspace's compose.yaml. Two-way sync is out of scope;
  devcontainer.json is read-only input.
- **B. Honour-as-is**: detect the file, run it via the official
  `devcontainer` CLI, and treat the resulting container as our
  workspace container. Loses control over name/lifecycle but
  matches user expectation if they're already a Codespaces
  team.
- **C. Don't translate**: assume the workspace owns
  `compose.yaml` and any per-repo devcontainer.json is for
  other tools. Surface it in the tree but otherwise ignore.

Decision deferred until we have a concrete repo to point at.
Likely outcome: A for the common subset, with a "this key
isn't supported, edit compose.yaml directly" warning for
anything weird.

### 2.4 — Per-service UI

**Status**: partially shipped (2.0.6 lands the project-side
surface). Remaining work for the workspace shell and the
deeper per-service controls lives behind 2.1–2.3.

Phase 2.0.6 ([above](#206--workspace-shell-vs-project-services-shipped))
lands the per-folder compose popover: each bound folder
gets its own status indicator + Start / Pause / Resume /
Rebuild / Stop affordances + a service list with state,
exit code, and health. That covers the "which services
are up and how do I drive them?" question for project
compose files.

What 2.4 still adds on top:

- Per-service drilldowns: log tails, one-click restart, an
  in-place "Pull latest" that recreates `mongo` (or
  whichever) without disturbing the rest of the project.
- A picker for "which service do new terminals attach to?"
  backed by the `x-moon.shell-service` extension key
  ([containers.md](../containers.md#x-moon-extension-keys-in-compose)).
  Today `dev` is the only service in the workspace shell
  project so this is academic; lands when 2.1+ adds
  alternate shell services or per-folder shell routing.
- Periodic / push-based status polling so a manual
  `docker compose stop` from the user's terminal updates
  the UI without a focus-driven refresh. Today
  per-folder snapshots come from the command response and
  the docker events watcher (Phase 2.2) will close the
  loop.

Test plan: `0017-container-services-ui.md` (TBD; folds in
the deltas above on top of `0012`).

## Out of scope (Phase 2, full list)

These all live in [containers.md's "Out of scope" section](../containers.md#out-of-scope-for-phase-2-and-when-to-revisit)
with their re-visit triggers; the short list:

- Per-project containers (when toolchains genuinely diverge).
- Auto-rebuild on `Dockerfile` mtime change.
- Podman / non-Docker engines.
- Remote (non-local) Docker hosts — revives the
  `moon-remote`-over-socket model from `architecture.md`.
- Cross-platform (macOS / Windows) verification.
- On-demand port forwarding without editing compose.

## 2.0.5 — workspace ≠ folder (shipped)

The original 2.0 wiring keyed compose state off the active
folder: state at `<workspace>/.moon/compose.yaml`, project
name hashed from the folder path, container recreated on every
folder switch. That conflation is incoherent the moment a
single moon-ide window holds multiple folders, so once
[Phase 2.5](phase-02.5-multi-folder.md) shipped multi-folder
UX we landed the corresponding container redesign on top of
it. The shape is locked in by the
[ADR 0007 state-dir + multi-folder amendment](../decisions/0007-compose-and-moon-base.md#amendment-2026-04-29--state-dir-and-multi-folder-mounts)
and described in
[`containers.md` § Multi-folder workspace](../containers.md#multi-folder-workspace-the-command-centre-ux);
the short version:

- Workspace state lives at
  `<dirs::data_local_dir>/moon-ide/workspaces/<id>/`
  (`compose.yaml` + `bound-folders.json`), with `<id>` =
  `"default"` until multi-workspace ships.
- Compose project name is `moon-ws-<id>`, decoupled from any
  folder path. The project survives folder switches and
  folder add / remove; only the contents of `compose.yaml`
  change.
- Bound folders mount at `/workspace/<basename>` with
  `working_dir: /workspace`; `include:` and `volumes:` are
  absolute paths.
- Folder add / remove regenerates `compose.yaml`; if the
  project is currently running, `docker compose up -d --wait`
  applies the diff. Pre-opt-in (no `compose.yaml` yet) and
  paused / stopped states are a no-op until the next explicit
  lifecycle action.
- `resetForWorkspaceSwitch` is gone — folder switches don't
  touch the compose project. Compose-preview cache
  invalidation moved to the bound-folder sync path so a
  re-open of "Inspect" reflects the new mounts.

What deliberately doesn't ship in 2.0.5:

- Cache volumes (`~/.cargo`, `~/.bun`, `~/.cache`) — lands with
  Phase 3 once routed terminals make caches matter; the 2.0
  `sleep infinity` `dev` container has no cache state worth
  preserving.
- Auto-pruning of project services orphaned by a removed
  folder. Visible enough via `docker compose ps`; surprise
  removal is worse than leftover noise. Add when somebody
  asks.
- Multi-workspace inventory UI. The naming scheme is forward-
  compatible (one `moon-ws-<id>` per workspace), so when
  Phase 7 grows multiple workspaces a single inventory pane
  drops in without re-keying anything.

Test plan: `0011-container-state-dir.md`.

## 2.0.6 — workspace shell vs project services (shipped)

The 2.0 / 2.0.5 model put one compose project per workspace,
with each bound folder's `docker-compose.yml` pulled in via
`include:`. Two real-world failure modes pushed back:

- A stalled project service (e.g. moon-landing's `gitaly`
  failing a volume permission check) blocked the whole
  `compose up --wait`, so the workspace shell — and any
  hope of opening a terminal — was held hostage to project
  health.
- Mental model mismatch: users think of "is the IDE
  running?" and "are this project's services running?" as
  two separate questions. Surfacing them as one status pip
  was confusing and made the popover do double duty for
  too many states.

The shape locked in by the
[ADR 0007 workspace-shell-vs-project-services amendment](../decisions/0007-compose-and-moon-base.md#amendment-2026-04-29--workspace-shell-vs-project-services)
and described in
[`containers.md` § Workspace shell vs project services](../containers.md#workspace-shell-vs-project-services):

- The workspace's generated `compose.yaml` is **dev-only**.
  No `include:`. "Set up" pulls the moon-base image and
  brings up `dev` — fast, doesn't touch any project image.
- Each bound folder's own root-level
  `docker-compose.yml` runs as a **separate** compose
  project named `moon-ws-<id>-<folder-slug>`. moon-ide
  shells out with `-f <user's file> -p <project name>`;
  it never modifies the user's file.
- Folder bars grow a small status indicator (visible only
  when the folder has a compose file). Click opens a
  per-folder popover with Start / Pause / Resume / Rebuild
  / Stop and a service list — same vocabulary as the
  workspace shell's pip but scoped to one folder.
- New Tauri commands `project_compose_status` /
  `project_compose_up` / `project_compose_pause` /
  `project_compose_resume` / `project_compose_rebuild` /
  `project_compose_down`, all keyed on `folder_path`.
  Lifetime-mutating commands emit
  `project_compose:state` events keyed on the same field
  so each folder bar updates independently.
- Networking: workspace shell and per-folder services run
  on separate compose networks. Cross-talk via host ports
  (`host.docker.internal:<port>`) for now; explicit
  external networks for users who need service-name
  resolution. Phase 2.2 formalises.

What deliberately doesn't ship in 2.0.6:

- Auto-cleanup of the previous unified
  `moon-ws-default`'s orphaned project containers. After
  upgrading, those leftovers are visible in
  `docker compose ls` and the user runs
  `docker compose -p moon-ws-default down --remove-orphans`
  once. Test plan documents the migration step.
- Sub-directory compose discovery for the per-folder UX.
  The folder runner uses only the root-level compose; a
  sub-directory compose belongs to its own bound folder
  if the user wants to manage it from moon-ide.
- Network-routing UX. Documented as "isolated by default,
  use host ports for cross-talk"; the formalised picker
  lands with Phase 2.2 ports.

Test plan: `0012-workspace-shell-vs-project-services.md`.

## Bootstrap concern

Per [ADR 0005](../decisions/0005-bootstrap.md), `moon-base`
must ship the toolchain a fresh moon-ide checkout needs:
`rustup`, `bun`, the WebKitGTK dev libraries, plus whatever
else moon-ide picks up between now and 2.0 landing. The
moon-base GitHub workflow encodes this as a release gate —
each per-arch run mounts the moon-ide checkout into the freshly
built image and runs `bun install --frozen-lockfile` followed
by `cargo check --workspace --locked`. A failing gate blocks
the digest push, so by construction `huggingface/moon-base:dev`
and the matching `:sha-<long>` always build moon-ide.
