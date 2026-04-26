# Devcontainers

STATUS: planned — designed for Phase 2.

## Goal

The user opens a folder. If the folder has a `.devcontainer/devcontainer.json`, moon-ide offers to run the workspace inside that container. Once accepted, **all** code execution — file ops, terminal, LSP, build, run, ACP agents — happens inside the container. Only ports the user explicitly forwards are reachable from the host. The host's network stays clean.

## Why this is a first-class concern

This is a vital requirement for the team. It also locks in the host/agent split, which is what makes remote SSH / Codespaces-style modes cheap to add later. Designing for it now is much cheaper than retrofitting.

## Spec we follow

The [Dev Containers Specification](https://containers.dev/) (`devcontainer.json`). We support a useful subset:

- `image` or `dockerFile` / `build`
- `workspaceFolder`, `workspaceMount`
- `forwardPorts`, `portsAttributes`
- `runArgs`
- `mounts`
- `remoteUser`, `containerEnv`
- `postCreateCommand`, `postStartCommand`, `postAttachCommand`
- `features` (later — likely v2)

## End-to-end flow

```
1. UI: "Open folder in container?"
   |
2. core.devcontainer.parse(.devcontainer/devcontainer.json)
   |
3. core.devcontainer.start():
   - shell out to `devcontainer up --workspace-folder <path>`
     (v0; replaced by direct bollard later)
   - record container id + workspace folder inside container
   |
4. core.agent.inject():
   - docker cp moon-agent into container at /usr/local/bin/moon-agent
   - docker exec moon-agent --listen unix:///tmp/moon-agent.sock
   - forward that socket to host (docker exec ... | nc, or named pipe)
   |
5. core.workspace.activeHost = RemoteHost(socket)
   - all subsequent fs.*, term.*, lsp.*, acp.* calls flow through it
   |
6. UI updates: status bar shows container, file tree reloads,
   open editors reload via remote fs
```

## Port forwarding

Three rules:

1. **Nothing auto-forwards.** Listening on a port inside the container does not, by itself, expose it to the host.
2. **`forwardPorts` from devcontainer.json** is honored on container start, but every forwarded port is shown in the UI with a "stop forwarding" affordance.
3. **On-demand forwarding** is the primary UX: a "Forward port" command in the palette (and a status-bar shortcut) prompts for the in-container port and an optional host port.

Implementation: forwarded ports are tracked by the core. We use `docker run -p` mappings where possible; for runtime forwarding we use `socat` or a small Rust forwarder that bridges a host TCP listener to `docker exec ... -i` or to the container's network namespace. Decide at Phase 2 implementation time.

## Process injection model

We inject `moon-agent` into an arbitrary base image rather than requiring users to install it. Approach:

- Build `moon-agent` as a static binary (`x86_64-unknown-linux-musl`).
- `docker cp` it into the container at startup.
- Exec it inside the container as the `remoteUser`.
- It listens on a Unix socket inside the container; we forward that socket out.

This means moon-ide has zero requirements on the container image other than "Linux glibc or musl x86_64".

## Lifecycle

- Container created by moon-ide is named `moon-ide_<workspace-hash>`.
- On UI close: container keeps running by default (matches VSCode behavior). A "stop on close" toggle can land later as a per-workspace flag — Phase 2 will decide where it lives (most likely a key inside `.devcontainer/devcontainer.json` or a moon-specific `.editorconfig` extension; per [ADR 0006](decisions/0006-no-settings-file.md) there is no `settings.json`).
- On reopen: detect existing container, reattach, re-inject agent if the binary version changed.

## Failure cases

- Docker not running: surface clear error, offer "open locally instead".
- Container build fails: stream build logs into a panel, retry button.
- Agent crashes: core auto-restarts; UI shows transient banner.
- Container stops while UI is open: UI enters "disconnected" mode, no crashes; reconnect button.

## Security

- The forwarded socket is mounted/forwarded only to the moon-ide process; not exposed to other host users.
- Forwarded ports bind to `127.0.0.1` by default; require an opt-in toggle to bind to `0.0.0.0`.
- We do not pass host secrets/env to the container unless explicitly listed.

## Out of scope (for now)

- Podman support (Phase 2.5)
- Multi-container `docker-compose`-based devcontainers (later)
- Remote (non-local) docker hosts (later)
