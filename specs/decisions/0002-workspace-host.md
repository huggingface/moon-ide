# ADR 0002 — `WorkspaceHost` is the I/O boundary

Date: 2026-04-26
Status: accepted

## Context

Devcontainer support is non-negotiable. If we let UI code or business logic call `tokio::fs::read` (or `git2::Repository::open`, or spawn a process directly), we will spend the rest of the project's life retrofitting "but what if it's in a container?" into every call site.

## Decision

All workspace I/O in `moon-core` goes through a single `WorkspaceHost` trait:

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
- `RemoteHost` — JSON-RPC client to `moon-agent` running inside a container.

The Tauri-exposed commands (and later the JSON-RPC server) always look up the active host and route through it. UI never knows or cares which it is.

## Consequences

- LSP, git, ACP, lint, terminal, search — all of them must use `WorkspaceHost` for any I/O that touches the workspace. They can spawn local processes (e.g. running an LSP binary that lives on the host or in the container) but the choice is made by the host.
- Phase 0 ships only `LocalHost`. The trait still exists; this is intentional pre-investment.
- Process supervision (long-running children) is owned by `WorkspaceHost::spawn`, not by ad-hoc `tokio::spawn` calls.

## Non-goals

- This trait is not a generic VFS abstraction. It is specifically the contract between "the workspace" and "the things acting on it".
- We do not expose `WorkspaceHost` to plugins. Plugins talk to a curated subset via the protocol.
