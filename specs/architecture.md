# Architecture

STATUS: partial — Phase 0 implements the in-process variant; the local-container split lands in Phase 2 and the remote variant comes later.

## Goal

A desktop IDE where every workspace operation works the same whether the workspace lives on the user's host machine, inside a local container (Phase 2 — see [`containers.md`](containers.md)), or on a remote SSH / Codespaces-style host (later).

## High-level shape

```
+----------------------------------+        +----------------------------+
|  Host machine                    |        |  Container (Phase 2)       |
|                                  |        |                            |
|  +---------+    +-------------+  | docker |  +----------------------+  |
|  | Svelte  |<-->| moon-core   |<-+--exec--+->| project tooling      |  |
|  |  UI     |    |  (local)    |  |  +     |  | (LSPs, PTYs, build,  |  |
|  +---------+    +-------------+  | bind   |  |  lint, user's docker |  |
|                  |               |  mount |  |  via DinD)           |  |
|                  | Tauri IPC     |        |  +----------------------+  |
|                  v               |        |    workspace at /workspace |
|                                  |        +----------------------------+
+----------------------------------+                  |
                                          explicitly forwarded ports
```

For local containers, the host filesystem is the source of truth — moon-core reads/writes files directly on the host, and the container sees the same bytes through a bind mount. Process and PTY work shells through `docker exec`. A future remote variant (no shared filesystem) will reuse the same `WorkspaceHost` trait through a JSON-RPC channel to a `moon-agent` running on the remote machine.

## Components

- **Svelte UI** (`src/`) — runs in the Tauri webview. Owns rendering, layout, keymap, and editor state. Calls into the core via Tauri commands.
- **`moon-core`** (`crates/moon-core/`) — the workspace brain. Owns the workspace registry, the `WorkspaceHost` abstraction, the LSP multiplexer, the git layer, the ACP host, and the cross-repo indexer. Linked into both the Tauri app and the in-container agent.
- **`moon-container`** (`crates/moon-container/`, Phase 2) — the local-container `WorkspaceHost` impl: shells out to `docker compose` for lifecycle, `docker exec` for spawn / PTY, and bind-mount-aware path translation. See [`containers.md`](containers.md).
- **`moon-agent`** (`crates/moon-agent/`, future) — a tiny binary that links `moon-core` in agent mode for the **remote** host story (SSH / Codespaces). Listens on a Unix socket, serves JSON-RPC. Not used by Phase 2's local-container model — that one uses bind-mount + exec because the filesystem is shared.
- **`moon-protocol`** (`crates/moon-protocol/`) — Serde-typed JSON-RPC schema. Single source of truth. TS types are generated from it (or kept in lockstep manually for now).

## The non-negotiable invariant

**Nothing in the UI directly touches git, LSP, fs, the terminal, or ACP.** Every such call is a JSON-RPC method on the core. The core picks the active `WorkspaceHost` and routes the call.

This is what makes local-container support cheap (Phase 2). It is also what will make remote SSH / Codespaces-style modes cheap when we add them. Violating it is the single biggest architectural risk.

## `WorkspaceHost` (Phase 2)

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

Three implementations:

- `LocalHost` — uses `tokio::fs`, `tokio::process`, `notify`, `portable-pty` directly. Phase 0.
- `ContainerHost` — fs ops are direct host I/O (the container's bind mount makes host paths and container paths point at the same bytes); `spawn` / `open_pty` route through `docker exec`. Phase 2. See [`containers.md`](containers.md#workspacehostcontainerhost).
- `RemoteHost` — JSON-RPC client to `moon-agent` over a forwarded socket, for hosts where the filesystem isn't shared (SSH / Codespaces). Future.

Phase 0 ships only `LocalHost` and exposes it through Tauri commands directly. The trait still exists so the UI's call sites don't need to change later.

## Process model

- One Tauri webview process (the UI).
- One `moon-core` instance running in-process inside the Tauri app (local mode).
- LSP servers, PTYs, ACP agents, lint sidecars: child processes spawned via the active host. Long-running ones are managed by the core's process supervisor (Phase 4+).

## Threading

- All filesystem and process I/O is async (Tokio).
- The Svelte UI is single-threaded JS; long ops happen on the Rust side.
- Streams (file watchers, PTY output, LSP notifications) are pushed to the UI via Tauri events keyed by stable subscription IDs.

## Failure model

Any host operation can fail because the host is gone (container stopped, network blip). The UI must treat workspace I/O as fallible and surface a degraded state without crashing. The core auto-reconnects to the agent in remote mode.

## Open questions

- Whether to use `tower-lsp`'s client-side helpers or roll a thinner LSP client. Decide at Phase 4 start.
- Whether `moon-core` should own its own tokio runtime or share Tauri's. Currently shares Tauri's.
- `moon-base` image registry (GHCR vs. HF Hub) — currently leaning GHCR; finalised when Phase 2.0 publishes (see [ADR 0007](decisions/0007-compose-and-moon-base.md)).
