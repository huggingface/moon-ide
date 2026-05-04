# Test plan 0031: Rust LSP runs inside the workspace container

- **Date**: 2026-05-04
- **Phase**: Phase 4 (LSP) / Phase 2 (containers) cross-cut

## What shipped

- When the workspace shell container is `Running`, language servers whose binary is reachable inside the container now run via `docker exec` (`rust-analyzer` system-wide, `tsgo` via the bind-mounted `node_modules/.bin/`); when the binary isn't reachable on that route, the broker transparently falls back to spawning on the host per-language. Decision is automatic.
- `moon-base` ships `rust-analyzer` out of the box (`rustup component add rust-analyzer` in the image), so a fresh container has working Rust diagnostics / hover / goto-def without the developer installing anything on the host.
- New `LspSpawner` (Local vs. DockerExec) + `PathTranslator` (Identity vs. HostMount) split in `moon-core::lsp` keep the LSP layer filesystem-agnostic; URIs get rewritten between host and container views in both directions.
- In-container binary resolution (`container_binary_path`) walks host ancestors for `NodeModules`-strategy servers and translates matches through the bind mount; paths that sit above the mount root automatically fall back to host LSP for that language.
- Every mutating `container_*` Tauri command now tears the LSP broker down so the next `.rs` / `.ts` open rebuilds against the current container state.
- Per-route probe: each route (container primary, host fallback) runs `<bin> --version` before committing to a spawn; any non-zero exit cascades to the next route rather than caching `Crashed` or `NotAvailable` prematurely.

## How to test

Prerequisites: `bun install`, a working moon-ide dev build (`bun run tauri dev`), Docker daemon reachable, `rust-analyzer` installed on the host (so the host-fallback path has something to fall back to), and the moon-ide repo itself as your workspace — it's a Cargo workspace with real cross-crate references and a `moon-base`-derived `compose.yaml`.

### Image rebuild

1. From the repo root: `docker build -t huggingface/moon-base:dev images/moon-base`. Confirm the build succeeds and the layer that runs `rustup component add rust-analyzer` exits zero.
2. `docker run --rm huggingface/moon-base:dev rust-analyzer --version`. Expected: a version string is printed, no errors.

### Container up → Rust LSP routes into the container

1. Open moon-ide, open the repo, click the status-bar pip → "Set up" (or "Resume" if a container is already present). Wait for `container:state` to settle on `Running`.
2. Open `crates/moon-core/src/host.rs`. Status pill should transiently say "rust: starting", then drop.
3. In a host terminal: `docker ps --format '{{.Names}} {{.Image}}' | grep moon-ws-`. Find the running dev container.
4. Still on the host: `docker exec <container> pgrep -af rust-analyzer`. Expected: a process tree with `rust-analyzer` running **inside** the container. Bonus: on the host, `pgrep -af docker.*exec.*rust-analyzer` should show the moon-ide-spawned `docker exec` parent.
5. Hover a symbol (e.g. a function in `host.rs` that calls into another crate). Tooltip appears within ~300 ms with the symbol's signature; fenced code in the tooltip is syntax-highlighted.
6. `Ctrl/Cmd`-click the same symbol. Editor jumps to the definition — including cross-crate jumps inside the Cargo workspace.
7. Introduce a type error (e.g. assign a `String` to a `&str`). Red gutter marker appears within a couple of seconds. Undo; it clears.
8. `Alt+Left` / `Alt+Right` walk the jump history correctly (same contract as test plan 0028).

### Container down → Rust LSP falls back to host

1. Click the status-bar pip → "Stop" (or "Teardown"). Wait for `container:state` to settle on `Stopped` / `Absent`.
2. The broker is torn down by `container_stop` / `container_teardown` (see `src-tauri/src/commands/container.rs`). Next `.rs` file open rebuilds.
3. Open any `.rs` file. Expected: same LSP experience (diagnostics, hover, goto-def), but now powered by the host `rust-analyzer`.
4. Verify on the host: `pgrep -af rust-analyzer` shows a process whose parent is the moon-ide shell (not `docker exec`).
5. Start the container back up ("Set up" / "Resume"). Open another `.rs` file. The broker rebuilds on the container path and you're back to in-container LSP.

### Custom-image probe fallback

1. In another branch or scratch image, edit `images/moon-base/Dockerfile` to drop the `rust-analyzer` from the `rustup component add` line. Rebuild (`docker build -t huggingface/moon-base:dev images/moon-base`).
2. Point the workspace at the broken image (e.g. edit `compose.yaml` in the per-workspace state dir to use `:dev`, then "Rebuild" from the status-bar pip).
3. Open a `.rs` file. Expected:
   - Status pill shows `rust: notavailable` with tooltip `rustup component add rust-analyzer`.
   - Backend log (run the IDE with `RUST_LOG=moon_core=debug`) contains the "container probe failed, falling back to NotAvailable" info line.
   - Host-side LSP is **not** auto-used in this variant (the broker cached `NotAvailable` after the probe) — the user's next move is to fix the image or restart moon-ide with the container down so the routing table picks Host.
4. Undo the Dockerfile change, rebuild, and confirm the pill goes away.

### TypeScript routes through the container when node_modules is in the mount

1. With the container up, open any `.ts` file (e.g. `src/lib/state.svelte.ts`).
2. Hover / goto-def / diagnostics all work exactly as in test plan 0024.
3. On the host: `pgrep -af 'docker exec.*tsgo'`. Expected: one `docker exec -i <container> /workspace/moon-ide/node_modules/.bin/tsgo --lsp --stdio` process parented by the moon-ide shell.
4. Inside the container (`docker exec <c> sh -lc 'pgrep -af tsgo'`): the in-container `tsgo` Node process is running against the bind-mounted `node_modules`.
5. Stop the container. Reopen a `.ts` file. The broker rebuilds; TS LSP now runs on the host (`pgrep -af tsgo` shows no `docker exec` wrapper).

### Per-server host fallback (hoisted node_modules)

1. Create a throwaway workspace layout where `node_modules` sits at a parent of the active folder (e.g. a pnpm-hoisted monorepo with moon-ide-style tsgo). Active folder: `monorepo/packages/app`, node_modules at `monorepo/node_modules`. Bind mount is `/workspace/app`, so the `tsgo` shim is above the mount root.
2. With the container up, open a `.ts` file in that active folder. Expected:
   - Backend log (`RUST_LOG=moon_core=debug`): "lsp: container binary path unresolved …" followed by "lsp: primary (container) unavailable, retrying on host fallback".
   - TS LSP works — hover / diagnostics / goto-def — but powered by the host `tsgo`.
3. No status pill (the fallback succeeds silently). Only when the host also lacks the binary does the user see `NotAvailable`.

## What must keep working

- Every regression lane from test plan 0030 (Rust LSP on the host): diagnostics, hover, completion, goto-def, nav history. They should behave identically in both container-backed and host-only modes.
- TypeScript LSP (test plans 0024 / 0027 / 0028): unchanged.
- Markdown hover syntax highlighting (0025) and inline git blame (0029): unchanged; the LSP routing change is orthogonal.
- Container lifecycle (Phase 2 plans): setup / pause / resume / rebuild / teardown / bound-folder apply all succeed. New expectation: each of them now also tears the LSP broker down, which you'd only notice as a transient "rust: starting" pill the next time a `.rs` file gets focused.
- Terminals: `docker exec` terminals still attach correctly (same `container_name_for_workspace` helper). Host terminals unaffected.

## Known limitations

- Scope is Rust only. Python / Svelte / CSS / HTML / JSON stay on the host until each one gets wired in moon-core; the routing template is ready to reuse per-language (update `moon-base` to ship the server, update `LspBroker::spec_for`, done).
- Remote (SSH / Codespaces) workspaces aren't covered — `DockerExec` doesn't generalise; a `RemoteHost` spawner is future work.
- We don't route LSP into user-provided containers unrelated to the workspace shell. The broker only knows about moon-ide's own `moon-ws-<id>` compose project.
- `rust-analyzer` still runs with defaults (`checkOnSave`, `cargo.features`, proc-macro toggles, etc.). `workspace/didChangeConfiguration` plumbing is a later slice when a concrete need surfaces.
- Multi-folder workspaces still use one broker per active folder. `rust-analyzer`'s own multi-folder support via `workspaceFolders` is orthogonal; re-shaping our broker for that is out of scope here.
- The probe failure path ends in `NotAvailable` rather than a host re-try within the same broker. Restarting moon-ide with the container down is the escape hatch; changing that would need a second attempt at broker construction and is more machinery than the scenario warrants.

## Related

- Specs: `specs/lsp.md` (new "Container-backed LSP" section), `specs/containers.md` (moon-base tool inventory).
- ADRs: `0007-compose-and-moon-base.md`, `0008-host-shared-daemon.md`.
- Prior test plans: `0030-rust-lsp.md` (host-side Rust LSP), `0028-nav-history-positions-cross-folder.md`, `0024-lsp-typescript-stage-1.md`.
