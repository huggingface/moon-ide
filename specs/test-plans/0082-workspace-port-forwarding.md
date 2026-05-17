# Test plan 0082: Workspace port forwarding

- **Date**: 2026-05-17
- **Phase**: Phase 2.2

## What shipped

- A bottom-panel "Ports" tab + a status-bar entry that lists
  workspace port forwards and their live state (live / proxy
  down / host port busy).
- Per-workspace forwards persisted in
  `WorkspaceSession.forwarded_ports`. Host port defaults to
  the container port; the user re-types it on conflicts.
- An IDE-managed `alpine/socat` proxy sidecar
  (`moon-ws-<id>-ports-1`) per workspace, recreated on every
  forward set mutation. The dev container, terminals, and any
  in-flight `bun dev` are untouched.
- Pre-flight `bind()` host-port conflict probe; conflicting
  entries surface in the UI without failing the whole apply.
- Auto-reapply after `container_setup` / `container_rebuild` /
  `container_apply_bound_folders`, so a fresh shell comes up
  with its forwards already wired.

## How to test

Prerequisites: a Docker daemon running on the host,
`alpine/socat:1.8.0.3` reachable (`docker pull alpine/socat:1.8.0.3`
to pre-warm), and a workspace with the dev shell up
(container pip green).

1. **Single forward against `bun dev`.**
   1. In a workspace bound to `~/code/moon-ide`, click the
      "ports" pill in the status bar. The bottom panel opens
      to the empty Ports tab.
   2. Open a container terminal (status-bar terminal pip,
      "container") and run `bun dev` inside the workspace.
      Note the dev-server port (`3000` for moon-ide).
   3. Back in the Ports tab: enter `vite` for the label,
      `3000` for container port, leave host blank, click
      **Add forward**.
   4. The row appears with a green dot. Click the **open**
      link. Expected: the host's default browser opens
      `http://localhost:3000` and renders the dev server.
   5. Run `docker ps --filter name=moon-ws-` from the host.
      Expected: a `<workspace-project>-ports-1` row in
      addition to `<workspace-project>-dev-1`. The dev
      container's `CREATED` column is unchanged from before
      step 1.4 — adding the forward did **not** recreate it.

2. **Removing a forward.**
   1. From step 1, the page is open in the host browser.
      Click the row's **×** to remove the forward, then
      click **Apply**.
   2. Refresh the browser tab. Expected: connection refused
      (the proxy is gone). Run `docker ps --filter
name=moon-ws-`. Expected: `<workspace-project>-ports-1`
      no longer present.
   3. The container terminal is still open and `bun dev`
      is still running. Type a key in the terminal —
      previous shell history is intact.

3. **Cross-workspace ports without collision.**
   1. Open two workspaces simultaneously (`bun dev --workspace
foo` + `bun dev --workspace bar` in separate processes,
      with two distinct `--workspace-id`s).
   2. In workspace A: declare `3000 -> 3000`, run a server
      inside on `:3000`.
   3. In workspace B: declare `3001 -> 3000`, run a server
      inside on `:3000`.
   4. Open `http://localhost:3000` and `http://localhost:3001`
      from the host. Expected: each hits its respective
      workspace's server.

4. **Host-port conflict.**
   1. With a forward declared `3000 -> 3000` and live, on the
      host run `python3 -m http.server 4242` in one terminal.
   2. In the Ports tab, change the host port to `4242` and
      click **Apply**.
   3. Expected: the row's dot turns red; an inline "Host port
      already in use: 4242" hint appears under the table.
   4. Stop the python server. Click **Apply** again. Expected:
      the dot turns green; `http://localhost:4242` reaches the
      dev server.

5. **Survives workspace rebuild.**
   1. Declare `3000 -> 3000`. Confirm it's live.
   2. From the container pip → "Rebuild container". After
      `--force-recreate` finishes and dev comes back up,
      check the Ports tab: the row's dot is green (a fresh
      sidecar was started). Run `docker ps`: a fresh
      `<project>-ports-1` row.

6. **Survives `down` + `setup`.**
   1. Declare `3000 -> 3000`. Container pip → Tear down.
      Expected: the sidecar is gone; the row in the Ports
      tab now shows a "proxy down" amber dot.
   2. Container pip → Set up. After it finishes, the row
      goes back to green (auto-reapply).

## What must keep working

- The dev container's "is it the same container?" identity is
  preserved across forward edits — verified by `docker ps`'s
  `CREATED` column being older than the most recent forward
  edit. Anything we did wrong here would surface as terminals
  vanishing on every port click.
- Per-folder `docker-compose.yml` services with `ports:`
  blocks of their own keep working unchanged. The proxy
  sidecar is namespaced under the workspace shell project
  (`moon-ws-<id>-ports-1`); it does not interact with the
  per-folder compose projects (`moon-ws-<id>-<slug>`).
- Workspace `teardown` removes both the dev container and
  the sidecar. `docker compose down` doesn't fail with "has
  active endpoints" on the workspace network.

## Known limitations

- Loopback-only. `127.0.0.1` is hardcoded; reaching a workspace
  dev server from another device on the LAN isn't supported
  yet (AGENTS.md "hardcode first").
- No `ss -ltn` auto-detection inside dev. The user types the
  forward.
- The Ports tab does not surface ports already published by
  per-folder `docker-compose.yml` files. Those forwards still
  work; they just live in the project compose, not the IDE's
  picker.
- Devcontainer.json `forwardPorts` interop is Phase 2.3.

## Related

- Specs: [`specs/containers.md`](../containers.md) §
  "Network and port forwarding"
- Roadmap: [`specs/roadmaps/phase-02-containers.md`](../roadmaps/phase-02-containers.md) § 2.2
- Source: [`crates/moon-container/src/port_forward.rs`](../../crates/moon-container/src/port_forward.rs),
  [`src-tauri/src/commands/ports.rs`](../../src-tauri/src/commands/ports.rs),
  [`src/lib/ports.svelte.ts`](../../src/lib/ports.svelte.ts),
  [`src/lib/components/PortsPanel.svelte`](../../src/lib/components/PortsPanel.svelte)
