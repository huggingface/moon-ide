# Architecture

STATUS: partial — Phase 0 implements the in-process variant; the local-container split lands in Phase 2 and the remote variant comes later.

## Goal

A desktop IDE where every workspace operation works the same whether the workspace lives on the user's host machine, inside a local container (Phase 2 — see [`containers.md`](containers.md)), or on a remote SSH / Codespaces-style host (later).

## High-level shape

```
+----------------------------------+        +----------------------------+
|  Host machine                    |        |  Workspace `dev` container |
|                                  |        |  (Phase 2, unprivileged)   |
|  +---------+    +-------------+  | docker |  +----------------------+  |
|  | Svelte  |<-->| moon-core   |<-+--exec--+->| project tooling      |  |
|  |  UI     |    |  (local)    |  |  +     |  | (LSPs, PTYs, build,  |  |
|  +---------+    +-------------+  | bind   |  |  lint)               |  |
|                  |               |  mount |  +----------------------+  |
|                  | Tauri IPC     |        |    workspace at /workspace |
|                  v               |        +----------------------------+
|                                  |
|  Host's Docker daemon            |        +----------------------------+
|  also runs the project's         |        |  Project services          |
|  service containers as           +--------+  (postgres, redis, mongo,  |
|  siblings on the same compose    |        |   …) — pulled in via       |
|  network                         |        |   compose `include:`       |
+----------------------------------+        +----------------------------+
                                                            |
                                                explicitly forwarded ports
```

For local containers, the host filesystem is the source of truth — moon-core reads/writes files directly on the host, and the container sees the same bytes through a bind mount. Process and PTY work shells through `docker exec`. A future remote variant (no shared filesystem) will reuse the same `WorkspaceHost` trait through a JSON-RPC channel to a `moon-remote` runtime running on the remote machine.

## Components

- **Svelte UI** (`src/`) — runs in the Tauri webview. Owns rendering, layout, keymap, and editor state. Calls into the core via Tauri commands.
- **`moon-core`** (`crates/moon-core/`) — the workspace brain. Owns the workspace registry, the `WorkspaceHost` abstraction, the LSP multiplexer, the git layer, and the cross-repo indexer. Linked into both the Tauri app and the in-container runtime.
- **`moon-container`** (`crates/moon-container/`, Phase 2) — the local-container `WorkspaceHost` impl: shells out to `docker compose` for lifecycle, `docker exec` for spawn / PTY, and bind-mount-aware path translation. See [`containers.md`](containers.md).
- **`moon-coder`** (`crates/moon-coder/`, Phase 6) — the in-process AI coding agent: HF OAuth device flow + Inference Providers HTTP client, tool dispatcher routed through the active `WorkspaceHost`, append-only JSONL sessions synced to an HF private bucket via `hf-xet`. See [`coder.md`](coder.md). The "agent panel" in the UI is owned here, not by an external ACP binary.
- **`moon-remote`** (`crates/moon-remote/`, future) — a tiny binary that links `moon-core` in remote mode for the SSH / Codespaces host story. Listens on a Unix socket, serves JSON-RPC. Not used by Phase 2's local-container model — that one uses bind-mount + exec because the filesystem is shared.
- **`moon-bridge`** (`crates/moon-bridge/`, future) — a host-resident daemon that exposes the coder + git surface to a mobile companion app over LAN WSS. Discovers running workspace processes by enumerating their per-workspace `instance.sock` files and relays the JSON-RPC surface + event streams to the phone over the same remote-mode framing `moon-remote` uses. One daemon per host, not one listener per workspace process. `moon-bridge` and `moon-remote` are the same "headless `moon-core` serving JSON-RPC over a channel" shape from different fronts (LAN companion vs. remote-host client) and are expected to converge — so we build the network framing once, not twice. See [`companion.md`](companion.md) and [ADR 0023](decisions/0023-mobile-companion-bridge.md).
- **`moon-protocol`** (`crates/moon-protocol/`) — Serde-typed JSON-RPC schema. Single source of truth. TS types are generated from it (or kept in lockstep manually for now).

## The non-negotiable invariant

**Nothing in the UI directly touches git, LSP, fs, the terminal, the coder, or any LLM.** Every such call is a JSON-RPC method on the core. The core picks the active `WorkspaceHost` and routes the call.

This is what makes local-container support cheap (Phase 2). It is also what will make remote SSH / Codespaces-style modes cheap when we add them. Violating it is the single biggest architectural risk.

### Host-direct fs (one explicit exception)

`fs_read_file_host` / `fs_write_file_host` (free functions in `moon_core::host`) read and write the **host** filesystem with no `WorkspaceHost` in the loop. They exist for one user-visible affordance — `Ctrl+O` "Open File…" — when the picked path lives outside every bound folder. Phase 2's `ContainerHost` would refuse such a path (it's outside the bind mount) and a remote `WorkspaceHost` couldn't see it at all. Buffers loaded this way carry `OpenFile.isExternal` and skip LSP / editorconfig / git / session persistence; they aren't part of any project. Every other fs call still goes through the active host.

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
- `RemoteHost` — JSON-RPC client to `moon-remote` over a forwarded socket, for hosts where the filesystem isn't shared (SSH / Codespaces). Future.

Phase 0 ships only `LocalHost` and exposes it through Tauri commands directly. The trait still exists so the UI's call sites don't need to change later.

## Process model

- One Tauri webview process (the UI).
- One `moon-core` instance running in-process inside the Tauri app (local mode).
- LSP servers, PTYs, lint sidecars: child processes spawned via the active host. Long-running ones are managed by the core's process supervisor (Phase 4+).
- The coder loop (Phase 6) runs in-process inside `moon-core`; its tool calls go through the same `WorkspaceHost` as everything else, so a containerised workspace gives the agent containerised tools without extra plumbing.

## Threading

- All filesystem and process I/O is async (Tokio).
- The Svelte UI is single-threaded JS; long ops happen on the Rust side.
- Streams (file watchers, PTY output, LSP notifications) are pushed to the UI via Tauri events keyed by stable subscription IDs.

## Failure model

Any host operation can fail because the host is gone (container stopped, network blip). The UI must treat workspace I/O as fallible and surface a degraded state without crashing. The core auto-reconnects to the agent in remote mode.

## Open questions

- Whether `moon-core` should own its own tokio runtime or share Tauri's. Currently shares Tauri's.
- Whether `moon-base` becomes its own repo or stays in-tree (see [ADR 0007](decisions/0007-compose-and-moon-base.md#open-follow-ups)). Registry is settled: Docker Hub at `huggingface/moon-base`.

## Resolved

- **LSP client shape.** `tower-lsp` is a server framework and doesn't help a client. We use `lsp-types` for message shapes and roll a thin custom client — framing + actor tasks, ~300 LOC. See [specs/lsp.md](lsp.md).
