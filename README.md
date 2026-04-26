# moon-ide

A team-specialized IDE built from scratch by assembling best-in-class components behind a Rust core that runs identically on the host or inside a devcontainer.

## Vision

- TypeScript-first (TS, Svelte, JSX/TSX, MD, JSON, CSS, HTML)
- Native git-blame-on-hover
- First-class linters/formatters: oxlint, oxfmt, prettier, eslint (+ plugins)
- ACP-native: pluggable coding agents (opencode, claude code, pi code, cursor-agent, custom)
- LSP nav (Ctrl+click, alt+left/right history)
- Multi-repo workspaces with cross-repo agent queries
- Devcontainers as a first-class concept: code/terminal/LSP/agent run in the container, only explicitly forwarded ports cross to the host
- Innovative UIs (the web is the reason we picked Tauri)

## Stack

- Tauri 2 (Rust backend + webview UI)
- Svelte 5 + TypeScript + Vite frontend
- CodeMirror 6 editor
- `@pierre/trees` (vanilla mode) for the file tree
- Rust workspace: `moon-core` (shared), `moon-agent` (in-container binary), `moon-protocol` (JSON-RPC schema)
- `gix` for git, `tantivy` for indexed search, `agent-client-protocol` for ACP

See [specs/architecture.md](specs/architecture.md) for the high-level design and [specs/](specs/) for everything else.

## Repository layout

```
.
├── src/                    Svelte 5 UI source
├── src-tauri/              Tauri shell (Rust main, capabilities, config)
├── crates/
│   ├── moon-core/          Workspace ops, LSP mux, git, ACP host, indexer
│   ├── moon-agent/         Binary run inside devcontainers
│   └── moon-protocol/      JSON-RPC schema shared by both ends
├── specs/                  Living design docs
├── AGENTS.md               Instructions for AI coding agents working in this repo
├── Cargo.toml              Cargo workspace root
└── package.json            Frontend deps + scripts
```

## Prerequisites

- Rust 1.90+ (`rustup default stable`)
- Node 20+ (we use 24)
- Bun (preferred) or pnpm
- Linux: webkit2gtk dev libs

```bash
# Linux Mint / Ubuntu 24+
sudo apt install -y libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev \
    libayatana-appindicator3-dev librsvg2-dev libssl-dev pkg-config
```

## Run

```bash
bun install
bun run tauri dev
```

`bun run fmt` / `lint` / `check` / `test` cover both the JS/TS and Rust sides; `:js` / `:rust` variants exist if you only want one. Code style and tooling rationale lives in [ADR 0004](specs/decisions/0004-code-style.md); a pre-commit hook auto-formats staged files.

## Status

Phases 0 (skeleton) and 1 (editor + navigation) are implemented. Phase 2 (devcontainer / remote split) is next — see [specs/roadmap.md](specs/roadmap.md).

> **Phased delivery rule** — each phase ends with a hand-back to a human reviewer. AI agents do not start the next phase on their own. See [AGENTS.md](AGENTS.md#phased-delivery).
