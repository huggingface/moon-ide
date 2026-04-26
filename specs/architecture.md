# Architecture

STATUS: partial — Phase 0 implements the in-process variant; the host/agent split lands in Phase 2.

## Goal

A desktop IDE where every workspace operation works the same whether the workspace lives on the user's host machine or inside a devcontainer (and later, on a remote SSH host or in a browser-served container).

## High-level shape

```
+----------------------------------+        +----------------------------+
|  Host machine                    |        |  Devcontainer (optional)   |
|                                  |        |                            |
|  +---------+    +-------------+  | sock   |  +----------------------+  |
|  | Svelte  |<-->| moon-core   |<-+--JSON--+->| moon-agent           |  |
|  |  UI     |    |  (local)    |  |  RPC   |  | (moon-core in agent  |  |
|  +---------+    +-------------+  |        |  | mode)                |  |
|                  |               |        |  +-+--------+--------+--+  |
|                  | Tauri IPC     |        |    |        |        |     |
|                  v               |        |    v        v        v     |
|                                  |        |   LSPs   PTYs/dev   ACP    |
+----------------------------------+        |   git    servers    agents |
                                             +---+--------+--------+-----+
                                                 |
                                       explicitly forwarded ports
```

## Components

- **Svelte UI** (`src/`) — runs in the Tauri webview. Owns rendering, layout, keymap, and editor state. Calls into the core via Tauri commands.
- **`moon-core`** (`crates/moon-core/`) — the workspace brain. Owns the workspace registry, the `WorkspaceHost` abstraction, the LSP multiplexer, the git layer, the ACP host, and the cross-repo indexer. Linked into both the Tauri app and the in-container agent.
- **`moon-agent`** (`crates/moon-agent/`) — a tiny binary that links `moon-core` in agent mode. Injected into a devcontainer, listens on a Unix socket, serves JSON-RPC.
- **`moon-protocol`** (`crates/moon-protocol/`) — Serde-typed JSON-RPC schema. Single source of truth. TS types are generated from it (or kept in lockstep manually for now).

## The non-negotiable invariant

**Nothing in the UI directly touches git, LSP, fs, the terminal, or ACP.** Every such call is a JSON-RPC method on the core. The core picks the active `WorkspaceHost` and routes the call.

This is what makes devcontainer support cheap. It is also what will make remote SSH and Codespaces-style modes cheap when we add them. Violating it is the single biggest architectural risk.

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

Two implementations:

- `LocalHost` — uses `tokio::fs`, `tokio::process`, `notify`, `portable-pty` directly.
- `RemoteHost` — JSON-RPC client to `moon-agent` over a forwarded socket. Same shape, same return types.

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
- Devcontainer image build/cache strategy (Phase 2).
