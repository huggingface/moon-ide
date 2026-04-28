# moon-ide

A team-specialized IDE built from scratch by assembling best-in-class components behind a Rust core that runs identically on the host or inside the workspace's container.

## Vision

- TypeScript-first (TS, Svelte, JSX/TSX, MD, JSON, CSS, HTML)
- Native git-blame-on-hover
- First-class linters/formatters: oxlint, oxfmt, prettier, eslint (+ plugins)
- ACP-native: pluggable coding agents (opencode, claude code, pi code, cursor-agent, custom)
- LSP nav (Ctrl+click, alt+left/right history)
- Multi-repo workspaces with cross-repo agent queries
- Containerised dev shells as a first-class concept: terminal/LSP/lint/format/build run in a single per-workspace container, only explicitly forwarded ports cross to the host
- Innovative UIs (the web is the reason we picked Tauri)

## Stack

- Tauri 2 (Rust backend + webview UI)
- Svelte 5 + TypeScript + Vite frontend
- CodeMirror 6 editor
- `@pierre/trees` (vanilla mode) for the file tree
- Rust workspace: `moon-core` (shared), `moon-protocol` (JSON-RPC schema), `moon-slack` (Slack chat panel client), `moon-agent` (future remote-host agent — not used by the local-container path)
- `gix` for git, `tantivy` for indexed search, `agent-client-protocol` for ACP

See [specs/architecture.md](specs/architecture.md) for the high-level design and [specs/](specs/) for everything else.

## Repository layout

```
.
├── src/                    Svelte 5 UI source
├── src-tauri/              Tauri shell (Rust main, capabilities, config)
├── crates/
│   ├── moon-core/          Workspace ops, LSP mux, git, ACP host, indexer
│   ├── moon-protocol/      JSON-RPC schema shared by both ends
│   ├── moon-slack/         Slack Web API client for the chat panel
│   └── moon-agent/         Binary for the future remote-host story (SSH / Codespaces)
├── specs/                  Living design docs
├── AGENTS.md               Instructions for AI coding agents working in this repo
├── Cargo.toml              Cargo workspace root
└── package.json            Frontend deps + scripts
```

## Prerequisites

The team's primary platforms are **macOS on Apple Silicon** and
**Linux** (x86_64 and arm64). Windows isn't supported — see
[`specs/containers.md`](specs/containers.md#out-of-scope-for-phase-2-and-when-to-revisit).

Common to both:

- Rust 1.90+ (`rustup default stable`)
- Node 20+ (we use 24)
- Bun (preferred) or pnpm
- Docker (Engine + Compose v2 on Linux, Docker Desktop on macOS — once Phase 2 lands the workspace's project tooling runs inside a container)

### macOS (Apple Silicon)

```bash
xcode-select --install
brew install rust bun docker
# or use Docker Desktop for Mac with VirtIO file sharing enabled
```

The macOS Tauri build chain is host-side — the macOS WebKit framework
shipped with Xcode CLT is what links against the app. The container
covers everything else (lint, format, tests, the cross-built Linux
artefact). See
[`specs/decisions/0005-bootstrap.md`](specs/decisions/0005-bootstrap.md#per-host-bootstrap)
for the per-host split.

### Linux

```bash
# Linux Mint / Ubuntu 24+
sudo apt install -y libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev \
    libayatana-appindicator3-dev librsvg2-dev libssl-dev pkg-config
```

(Once Phase 2 lands these libraries live inside the `moon-base` image,
not on the host — but until then the host-build path needs them.)

## Run

```bash
bun install
bun run dev
```

`bun run dev` boots the Tauri shell (Rust backend + the Vite-served Svelte UI as one window). On the first run, expect a noticeable Cargo build before the window appears.

`bun run fmt` / `lint` / `check` / `test` cover both the JS/TS and Rust sides; `:js` / `:rust` variants exist if you only want one. Code style and tooling rationale lives in [ADR 0004](specs/decisions/0004-code-style.md); a pre-commit hook auto-formats staged files.

## Status

Phases 0 (skeleton) and 1 (editor + navigation) are implemented. Phase 2 (containerised dev shells) is next — see [specs/roadmap.md](specs/roadmap.md) and [specs/containers.md](specs/containers.md).

> **Phased delivery rule** — each phase ends with a hand-back to a human reviewer. AI agents do not start the next phase on their own. See [AGENTS.md](AGENTS.md#phased-delivery).
