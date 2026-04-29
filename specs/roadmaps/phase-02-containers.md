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

- New crate `moon-container` (`crates/moon-container/`) that
  shells out to `docker compose` via `bollard` or
  `tokio::process` (decide at implementation start; bollard
  preferred if it doesn't drag in too much).
- Compose discovery + generation logic: scan the workspace
  root and one level deep for `docker-compose.yml` /
  `compose.yaml`, write `<workspace>/.moon/compose.yaml`
  with the discovered files in `include:` plus a `dev`
  service. Generation runs once on first opt-in; the file
  is user-owned thereafter.
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
  `container_pause` / `container_resume` / `container_rebuild`.
  Each operates on the **whole compose project**, not just
  the `dev` service — pausing the workspace pauses included
  services with it.
- Push events `container:state` / `container:logs` /
  `container:error`.
- Status-bar pip that surfaces state + logs panel. No
  redesign of the status bar itself.
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

**Status**: planned, lives behind 2.0–2.3.

Multi-service compose itself works from 2.0 (the `include:`
model puts the project's services alongside `dev` from day
one). What 2.4 adds is the UI surface: a per-service status
panel listing every service in the compose project (`dev`,
`mongo`, `redis`, …) with state, log tail, and one-click
restart. Includes:

- A "Project services" subsection in the sidebar showing
  every compose service with state and a small log tail.
- Per-service `start` / `stop` / `restart` actions.
- Per-service "Pull latest" (in-place image refresh + recreate
  for a single service, without disturbing the rest of the
  project) — the case for `mongo` getting a new tag without
  rebuilding everything.
- A picker for "which service do new terminals attach to?"
  backed by the `x-moon.shell-service` extension key
  ([containers.md](../containers.md#x-moon-extension-keys-in-compose)).
- Surfacing service-level health checks in the UI (compose
  already exposes `healthy` / `unhealthy`; we just have to
  display it).

Test plan: `0017-container-services-ui.md` (TBD).

## Out of scope (Phase 2, full list)

These all live in [containers.md's "Out of scope" section](../containers.md#out-of-scope-for-phase-2-and-when-to-revisit)
with their re-visit triggers; the short list:

- Per-project containers (when toolchains genuinely diverge).
- Auto-rebuild on `Dockerfile` mtime change.
- Podman / non-Docker engines.
- Remote (non-local) Docker hosts — revives the
  moon-agent-over-socket model from `architecture.md`.
- Cross-platform (macOS / Windows) verification.
- On-demand port forwarding without editing compose.

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
