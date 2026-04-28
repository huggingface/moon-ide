# Containerised dev shells

STATUS: planned — designed for Phase 2.

## Goal

The user opens a workspace; moon-ide provisions a single Docker
container that hosts the dev environment for **the whole
workspace** (not for any single project inside it). All project
tooling — terminals, LSPs, linters, formatters, build commands —
runs inside that container; the IDE shell and workspace-level
features (Slack, agents, the Tauri host) stay on the host.

The goal is to make `bun dev`, `cargo run`, etc. work the same
on every team member's machine without polluting the host's
ports, polluting the host's `apt` / `pip` / `npm` global state,
or rediscovering the same Docker-in-Docker incantations every
six months.

## Why this is a workspace concern, not a project concern

moon-ide is a command center. A workspace can hold one project
or fifteen — sibling repos in the same org, an active feature
branch with its dependent libraries checked out alongside, a
target repo plus the tooling repo that builds it. Cross-repo
search, cross-repo agent queries, a single shell that can `cd`
between them — all of these assume a shared dev environment.

If we anchored the container to a single project (the
`devcontainer.json`-per-repo model), opening four sibling repos
would mean four containers fighting for ports, four DinD
daemons, four toolchain installs to keep in sync. One container
per workspace makes the multi-project case the simple case.

When two projects in the same workspace genuinely cannot share
toolchains (Python 3.10 vs. 3.12, Rust nightly vs. stable, GPU
pinning), that is the moment to graduate to multi-container —
deferred to a sub-phase that lands behind a concrete request.
See [Out of scope](#out-of-scope-for-phase-2-and-when-to-revisit).

## Single container, single image

### The `moon-base` image

moon-ide publishes its own base image to Docker Hub
(`huggingface/moon-base:<tag>`). Teams `FROM moon-base` to add
their own toolchain on top.

What `moon-base` ships:

- **Debian stable** as the OS layer. Not Microsoft's
  `mcr.microsoft.com/devcontainers/...` family — those are
  fine, but for a long-lived workspace container we'd rather
  control the base ourselves.
- **Docker-in-Docker, pre-configured.** `fuse-overlayfs` as the
  storage driver, `iptables-legacy` as the active alternative,
  `containerd-snapshotter: false` for Docker 29+. Every team
  that has ever shipped a devcontainer with nested Docker has
  fought this fight; we fight it once, in this image. (Source
  of the recipe: the cloud-dev skill at `~/code/claude-skills`,
  cross-checked against moon-landing's working setup.)
- **Polyglot toolchain**: bun, node (current LTS), rustup with
  the stable toolchain, `uv`-managed Python, `hf` CLI, `gh`,
  the usual `git` + `ripgrep` + `fzf` + `bat`. No language
  locks the version itself — teams add `rustup toolchain
install nightly` or `uv python install 3.10` in their own
  Dockerfile when they need to.
- **WebKitGTK dev libraries** so a fresh moon-ide checkout is
  buildable inside its own container (the bootstrap concern
  from [ADR 0005](decisions/0005-bootstrap.md)).
- An entrypoint that does **nothing** — moon-ide attaches with
  `docker exec` and runs commands on demand. No daemonised
  language servers waiting in the dark.

Versioning: `moon-base:<commit-sha>` for reproducibility,
`moon-base:latest` for human convenience, `moon-base:major.minor`
when breaking changes ship. The `compose.yaml` we generate
points at a specific tag, not `latest`.

### Distribution

`moon-base` ships as a **multi-arch manifest** to **Docker
Hub** (`huggingface/moon-base`):

- `linux/amd64` — Linux contributors and CI smoke tests.
- `linux/arm64` — the team's primary target (Mac-majority,
  Apple Silicon).

Built natively on both architectures via GitHub Actions
(`ubuntu-24.04` + `ubuntu-24.04-arm` — both free for public
repos, so no QEMU emulation in the path). Two parallel build
jobs feed a `docker buildx imagetools create` step that
publishes the combined manifest. Docker pulls the right arch
automatically based on the host.

Builds run only on changes to `moon-base`'s sources (its
Dockerfile, the bootstrap scripts, the workflow itself) — not
on every moon-ide commit. The "infrequent build" model is
fine because `moon-base` deliberately doesn't track moon-ide
versions; it tracks toolchain versions. Concretely, the
release-gate smoke test (`bun install && cargo check` inside
the freshly built image) only runs when `moon-base` itself
changes — moon-ide commits land without it. The contract is
"`moon-base` builds moon-ide cleanly when published", not
"every moon-ide commit re-validates `moon-base`".

#### Why Docker Hub, not GHCR or HF Hub

Trade-offs we walked through, with their rejection reasons:

- **GHCR**: ties the workspace's runtime base image to a
  GitHub/Microsoft account. We'd rather not bake that
  dependency into every team member's `docker pull` path
  for a foundational artefact.
- **Private Docker registry**: image needs to be public so
  contributors and downstream users can pull it without
  credential setup. Rules out anything self-hosted-and-gated.
- **Hugging Face Spaces "run locally"**: HF Spaces only
  ship `linux/amd64` artefacts; using them as the
  distribution channel would lock arm64 (the primary target)
  out of the picture.
- **Docker Hub**: vendor-neutral relative to our existing
  GitHub dependencies, public, multi-arch-native, idiomatic
  (`docker pull huggingface/moon-base` works without a
  registry prefix). Chosen.

### Why not devcontainer.json features

Devcontainer "features" (`docker-in-docker:2`, `github-cli:1`,
…) are nice for the per-project model, but they layer on top of
whatever base image the user picked, which means every team
relitigates the DinD setup separately. By baking DinD into
`moon-base`, every workspace inherits a working setup with no
configuration. Features for the rare cases that aren't covered
can come back as Dockerfile lines in the user's `FROM
moon-base AS team-dev` extension; we lose nothing.

### How teams extend it

```dockerfile
FROM huggingface/moon-base:1.0

# Team-specific tooling
RUN apt-get update && apt-get install -y \
    awscli \
    redis-tools \
 && rm -rf /var/lib/apt/lists/*

# Team-specific user
ARG USERNAME=dev
ARG UID=1000
RUN useradd --uid ${UID} --create-home ${USERNAME}
USER ${USERNAME}
```

The team commits this `Dockerfile` (typically at
`<repo>/.moon/Dockerfile`); the workspace's `compose.yaml`
references it via `build.context`.

## Workspace config: `compose.yaml`

The source of truth is `<workspace>/.moon/compose.yaml` —
standard Docker Compose syntax. moon-ide reads it on every
workspace open, parses just enough to find the service it
attaches to, and otherwise lets compose own its semantics.

A minimal generated config:

```yaml
# <workspace>/.moon/compose.yaml
services:
  shell:
    image: huggingface/moon-base:1.0
    volumes:
      - ../:/workspace:cached
    working_dir: /workspace
    command: sleep infinity
    init: true
    privileged: true # required for DinD
    networks: [bridge]
    # Declared forwards — listed for IDE discovery, not auto-bound.
    # ports: ["3000:3000"]

networks:
  bridge:
    driver: bridge
```

What gets auto-generated vs. user-owned:

- **Generated on first opt-in**: the file above, with the
  `moon-base` image pinned to whatever tag the running
  moon-ide build was paired with. The user can commit it,
  gitignore it, or rewrite it as they please.
- **User-owned thereafter**: every line. moon-ide never writes
  back to `compose.yaml` after the first generation. Switching
  images, adding services, changing mount strategies — all
  manual.

### Why compose, not devcontainer.json native

Three reasons.

1. **Image control.** Compose makes "use any base image" the
   default; devcontainer.json's documented path nudges users
   toward Microsoft's image family.
2. **One layer of magic instead of two.** `compose.yaml` +
   `Dockerfile` is the boring industry-standard pair. Every
   team running anything in production already knows it.
   Devcontainer.json adds a translation step that eventually
   becomes compose anyway.
3. **Multi-service is free.** When a sub-phase needs to add
   `db` / `redis` / a GPU-pinned service, compose gains a
   service entry; no new abstraction.

Devcontainer.json **interop** (reading a repo's existing
`.devcontainer/devcontainer.json` and synthesising a compose
service from it) is a separate concern, deferred to Phase 2.3.
Decided at that phase, not now.

See [ADR 0007](decisions/0007-compose-and-moon-base.md) for the
full trade-off discussion.

## Lifecycle

| Event                                                          | What moon-ide does                                                                                                                                          |
| -------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| First workspace open (no container yet)                        | Pull image if absent → `docker compose -f <workspace>/.moon/compose.yaml create` → `docker compose start`. Run `postCreateCommand` if compose declares one. |
| Subsequent workspace opens                                     | Look up container by name (`moon-ws-<short-hash>`). Unpause if paused, no-op if running.                                                                    |
| Workspace close (window close, app quit, host suspend)         | `docker pause`. Container survives moon-ide restart, host reboot, suspend.                                                                                  |
| User clicks "Rebuild"                                          | `docker compose down` → recreate from current `compose.yaml`. Drops state inside the container.                                                             |
| Compose file edited externally                                 | Detected on next workspace open via mtime; prompt for "Rebuild now?" before reattaching.                                                                    |
| Container deleted out-of-band (`docker rm` from another shell) | Detected on next open via `docker inspect` failure; recreate transparently.                                                                                 |
| Image tag changed in `compose.yaml`                            | Treated as "Rebuild" — recreate from new image.                                                                                                             |

### Why pause, not stop

`docker pause` uses the kernel's cgroup freezer. Processes are
frozen in place — DinD daemon, language servers, the user's
shell history, the running `bun dev` waiting for a save — all
in memory. Resume is essentially instant (cgroup thaw, no
re-init).

`docker stop` sends SIGTERM, processes shut down, init runs
again on restart. Inner DinD has to come back up (~5–10 s).
We use `stop` only on rebuilds where we _want_ the inside
state thrown away.

## What runs where

| Concern                                                | Host (Tauri / moon-ide proper) | Container                  |
| ------------------------------------------------------ | ------------------------------ | -------------------------- |
| UI rendering, Tauri shell, IPC routing                 | ✓                              |                            |
| Slack panel, Slack API calls                           | ✓                              |                            |
| ACP / agent runtimes (Phase 6)                         | ✓                              |                            |
| Git operations on the workspace                        | ✓                              | (read-only via bind mount) |
| Project terminals                                      |                                | ✓                          |
| LSP servers                                            |                                | ✓                          |
| Linters / formatters (oxlint, oxfmt, prettier, eslint) |                                | ✓                          |
| Build commands (`cargo`, `bun`, `npm`, `make`)         |                                | ✓                          |
| `docker compose up` from the user's project            |                                | ✓ (via DinD)               |

The split lines up with "is this about the project, or is this
about the workspace?". Slack is workspace-level — credentials
live in the OS keyring, not in the container. Agents talk to
remote APIs; running them inside the container would force
every API call through the container's network namespace for
no upside. The user's own dev tooling is project-level — that's
where the container earns its keep.

### When the project _is_ moon-ide

moon-ide is itself a Tauri app, so its build matrix is
platform-coupled in a way most workspaces aren't. The shape:

- The moon-ide binary the contributor launches is **host-native**
  on every platform. The webview it loads at startup is a
  system library: WebKitGTK on Linux, WKWebView on macOS.
- The platform-native toolchain that produces that binary (Rust
  - bun + the platform's webview dev headers + the platform's
    linker) therefore lives on the host as well — putting it
    inside a Linux container can't produce a macOS `.app`, and a
    Linux binary built inside the container would still need
    WebKitGTK on the host to actually launch.
- Everything else — project tooling that doesn't care which
  platform binary the build pipeline targets (rust-analyzer,
  oxlint, oxfmt, prettier, `bun run check`, tests, the
  `bun install` step itself) — lives in the container.

Concretely:

- **macOS contributors**: host gets Xcode CLT + rustup + bun +
  Docker Desktop. macOS provides WebKit. The container handles
  project tooling and (if needed) the cross-built Linux artefact.
- **Linux contributors**: host gets WebKitGTK dev libraries
  (`libwebkit2gtk-4.1-dev` + the rest listed in the
  [README](../README.md#linux)) + rustup + bun + Docker Engine.
  The container handles project tooling. WebKitGTK can't move
  into the container because the binary needs it at launch on
  the host.

In practice the moon-ide repo's `package.json` and `Cargo.toml`
work the same on both — Tauri's CLI picks the platform target
automatically based on whoever's running the build. The
container is the equaliser for everything _around_ that build.

## Path mapping

The workspace root is bind-mounted at a fixed path inside the
container, by default `/workspace` (configurable via the
`workspaceFolder` key in `.moon/compose.yaml`'s top-level
`x-moon` block — see [Forward compatibility](#forward-compatibility)).

Path translation is a single function in `moon-container`:

```rust
struct PathMapping {
    host_root: Utf8PathBuf,    // e.g. /home/eli/code/hf
    container_root: Utf8PathBuf, // e.g. /workspace
}

impl PathMapping {
    fn to_container(&self, host_path: &Utf8Path) -> Result<Utf8PathBuf>;
    fn to_host(&self, container_path: &Utf8Path) -> Result<Utf8PathBuf>;
}
```

Used everywhere a value crosses the boundary: when we ship a
file path to a container-running LSP, when we receive a
diagnostic with a path back, when the user clicks an in-IDE
link to a path the bot generated, etc. Identity mapping when
no container is active.

Subprojects: a workspace at `/home/eli/code/hf` containing
`/home/eli/code/hf/moon-ide` and `/home/eli/code/hf/moon-bot`
maps to `/workspace/moon-ide` and `/workspace/moon-bot` inside.
The container sees one tree.

## Network and port forwarding

The container runs on its own bridge network — the whole point
of doing this is _not_ polluting the host's `localhost`. Ports
the user wants to reach from the host are declared explicitly
in `compose.yaml`'s `ports:` map.

Three rules:

1. **Nothing auto-forwards.** A service inside the container
   listening on `:3000` is not reachable from the host unless
   `compose.yaml` says so.
2. **Declared forwards bind on `127.0.0.1` by default.** The
   user opts in to `0.0.0.0` per-port if they want to reach
   the dev server from another device.
3. **Conflict detection.** If the host port is busy when the
   container starts, surface the error in the panel — don't
   silently rebind.

The IDE surfaces the live forward map in a small "Ports"
section of the status bar / sidebar (Phase 2.2):

```
● 3000 → http://localhost:3000   (bun dev)
● 5173 → http://localhost:5173   (vite)
✕ 5432 → host port busy          [retry]
```

Clicking opens the URL in the host's default browser.

## `WorkspaceHost::ContainerHost`

The trait already exists from Phase 0:

```rust
#[async_trait]
trait WorkspaceHost: Send + Sync {
    async fn read_dir(&self, path: &Utf8Path) -> Result<Vec<DirEntry>>;
    async fn read_file(&self, path: &Utf8Path) -> Result<Vec<u8>>;
    async fn write_file(&self, path: &Utf8Path, bytes: &[u8]) -> Result<()>;
    async fn watch(&self, path: &Utf8Path) -> Result<WatchStream>;
    async fn spawn(&self, cmd: SpawnCmd) -> Result<ProcessHandle>;
    async fn open_pty(&self, opts: PtyOpts) -> Result<PtyHandle>;
}
```

`ContainerHost` is a thin wrapper:

- **Filesystem ops**: identical to `LocalHost`. The bind mount
  means host paths and container paths see the same bytes; we
  read/write on the host side directly. No JSON-RPC, no
  agent process, no in-container fs daemon. The path
  translator only kicks in when we're shipping a path
  _through_ the container (e.g., to an in-container LSP).
- **`spawn`**: shells out to `docker exec <container> <cmd>`.
  stdio is forwarded through `docker exec`'s pipes. Working
  directory is translated via `PathMapping::to_container`.
- **`open_pty`**: `docker exec -it <container> <shell>` with a
  PTY allocated on the host side, attached via tokio's
  `portable-pty`. The container sees a real TTY.
- **`watch`**: same `notify`-backed watcher as `LocalHost` —
  the host can watch a bind-mounted directory; the container's
  writes are reflected instantly.

This deliberately doesn't use the moon-agent injection model
the old `specs/devcontainers.md` sketched. That model is
correct for **remote** hosts where there's no shared
filesystem (SSH, Codespaces) and is preserved as the future
`RemoteHost` variant. For local containers, bind-mount + exec
is simpler, faster, and avoids shipping a static-musl binary
into the user's container.

## Frontend ↔ backend boundary

New tauri commands (Phase 2.0):

| Command                                | Purpose                                                                                              |
| -------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `container_status`                     | `{ state: "absent" \| "creating" \| "running" \| "paused" \| "stopped" \| "error", error?: string }` |
| `container_setup`                      | First-time bootstrap: write default `compose.yaml`, pull image, create + start. Idempotent.          |
| `container_pause` / `container_resume` | Lifecycle hooks.                                                                                     |
| `container_rebuild`                    | `down` + recreate. Drops in-container state.                                                         |
| `container_open_dockerfile`            | Convenience: opens `<workspace>/.moon/Dockerfile` (or `compose.yaml`) in a new tab.                  |

Push events:

- `container:state` — emitted on every state transition for the
  status-bar pip.
- `container:logs` — streamed during `creating` / `rebuilding`
  so the user sees image pulls + build output.
- `container:error` — terminal failure; payload carries the
  failing step.

## Failure modes

| Scenario                                        | UI behaviour                                                                                                                                                                                                         |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Docker daemon not running                       | Banner: "Docker is not running. [Open Docker Desktop / install instructions]". Workspace opens host-side; container actions disabled.                                                                                |
| Image pull fails (network, auth)                | Logs panel + retry button. Workspace opens host-side until resolved.                                                                                                                                                 |
| Build fails (Dockerfile error)                  | Logs panel + "Edit Dockerfile" button. Container stays in `error` state.                                                                                                                                             |
| Container exits unexpectedly mid-session        | Status pip flips to `stopped`, terminals show "Container stopped" + reconnect button.                                                                                                                                |
| Bind-mount permission mismatch (uid/gid)        | Detected on first `read_file` failure; surface a one-time "Set container user to match host? [Yes / leave]" prompt that adds `user: "${UID}:${GID}"` to compose.                                                     |
| Docker version too old (no `compose v2`)        | Refuse to start; show required version in the banner.                                                                                                                                                                |
| Disk pressure (image pull no space)             | Surface Docker's error verbatim; we don't try to GC behind the user's back.                                                                                                                                          |
| Slow bind-mount I/O on Docker Desktop for macOS | Generated `compose.yaml` declares the workspace mount with `:cached` consistency. Recommend VirtIO file sharing in Docker Desktop's settings; surface a one-line tip in the status-bar pip on first slow `read_dir`. |

## Out of scope (for Phase 2 — and when to revisit)

- **Multi-service compose UI.** Compose already supports
  multiple services; what's missing is the IDE picker for
  "which service do new terminals attach to?". Add when a
  workspace genuinely needs `app` + `db` + `cache` and the
  current "everything in `shell`" stops working.
- **Devcontainer.json interop.** Read a repo's existing
  `.devcontainer/devcontainer.json` and translate it into a
  compose service at workspace open. Deferred to Phase 2.3 so
  we can debate the translation rules with concrete examples
  (see the open question in [Phase 2 roadmap](roadmaps/phase-02-containers.md)).
- **Per-project containers.** When two projects in the same
  workspace can't share a toolchain. Add when divergence
  appears; the architecture doesn't preclude it.
- **Auto-rebuild on Dockerfile change.** v1 prompts on next
  open. Auto-detect via fs-watch is a small follow-up.
- **Podman / non-Docker engines.** Should mostly work via
  `DOCKER_HOST`, but unverified. Address when somebody asks.
- **Remote (non-local) Docker hosts.** Different problem
  shape (no bind mount); revives the moon-agent injection
  model for that variant.
- **Windows host.** Nobody on the team uses Windows; Docker
  Desktop should mostly work but is unverified. Surface
  if/when somebody on the team or in a downstream user picks
  it up.

## Forward compatibility

### Multi-window

Phase 2 ships one window = one workspace = one container.
Two future shapes are explicitly preserved:

- **Multiple windows of the same workspace.** Container is
  keyed by workspace, not by window — the name pattern is
  `moon-ws-<hash-of-workspace-id>`. If a future phase opens
  the same workspace in a second window, both share the
  container by construction.
- **Multiple workspaces, multiple containers.** `AppState`'s
  `last_session` slot will grow into a list as Phase 7
  (multi-repo) lands. The persistence layer is structured
  so this is a UI/lifecycle change, not a data-model rewrite.

### Remote hosts

`ContainerHost` is the local variant. The `RemoteHost`
sketched in `architecture.md` (JSON-RPC over a forwarded
socket to a `moon-agent` running on a remote machine) is
deferred until somebody asks. It reuses the same
`WorkspaceHost` trait, so adopting it later is additive.

### `x-moon` extension keys in compose

Compose tolerates any top-level `x-`-prefixed key as
metadata. We use that to carry moon-ide-specific config
without inventing a parallel format:

```yaml
x-moon:
  shell-service: shell # which compose service do new terminals attach to
  workspace-folder: /workspace
  container-name: moon-ws-foo # override the default hash-derived name
```

`x-moon` keys are read but never written. The user owns the
file.
