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
writes a default `compose.yaml`, pulls `moon-base`, and creates

- starts the container. Subsequent opens unpause an existing
  container; closing the window pauses it. The status pip cycles
  through `absent → creating → running → paused → running` cleanly.

What ships:

- New crate `moon-container` (`crates/moon-container/`) that
  shells out to `docker compose` via `bollard` or
  `tokio::process` (decide at implementation start; bollard
  preferred if it doesn't drag in too much).
- `moon-base` Dockerfile + GitHub Actions workflow publishing
  a multi-arch manifest (`linux/amd64` + `linux/arm64`, both
  built natively on `ubuntu-24.04` / `ubuntu-24.04-arm` —
  no QEMU) to **Docker Hub** at `huggingface/moon-base:<sha>`
  and `huggingface/moon-base:<major>.<minor>`. arm64 is the
  team's primary target (Mac-majority, Apple Silicon); amd64
  covers Linux contributors and CI. Builds run only on
  changes to `moon-base`'s sources, not on every moon-ide
  commit. Image contents per
  [`containers.md`](../containers.md#the-moon-base-image),
  distribution per
  [`containers.md`](../containers.md#distribution),
  registry rationale per
  [ADR 0007](../decisions/0007-compose-and-moon-base.md#registry-docker-hub).
- Container name derived from a stable hash of the workspace
  path: `moon-ws-<short-hash>`.
- Tauri commands `container_status` / `container_setup` /
  `container_pause` / `container_resume` / `container_rebuild`.
- Push events `container:state` / `container:logs` /
  `container:error`.
- Status-bar pip that surfaces state + logs panel. No
  redesign of the status bar itself.
- ADR [0007](../decisions/0007-compose-and-moon-base.md) for
  the compose-over-devcontainer.json + own-base-image calls.

What doesn't ship in 2.0:

- Routed execution. Until 2.1, terminals/LSPs/etc. still run
  on the host. 2.0's "running" container just sits there with
  `sleep infinity` and confirms the lifecycle works.
- Port forwarding UI (just whatever compose declares natively).
- Devcontainer.json reading.

Test plan: `0014-container-lifecycle.md` (TBD at implementation
start).

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

### 2.4 — Multi-service compose UI

**Status**: deferred until concrete request.

When a workspace's `compose.yaml` declares more than one
service (`shell` + `db` + `cache`), the IDE needs to know
which one new terminals attach to and which ones it just
keeps alive. The `x-moon.shell-service` extension key
([containers.md](../containers.md#x-moon-extension-keys-in-compose))
covers the data side; the UI side waits for a real workflow.

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
moon-base GitHub workflow runs a smoke test that does
`git clone moon-ide && bun install && cargo check` inside the
freshly built image — green is a release gate.
