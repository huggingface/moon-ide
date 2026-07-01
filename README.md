# moon-ide

A team-specialized IDE built from scratch by assembling best-in-class components behind a Rust core that runs identically on the host or inside the workspace's container.

## Vision

- Built-in support for TS / Rust / Go
- Native git-blame-on-hover
- First-class linters/formatters: oxlint, oxfmt, prettier, eslint (+ plugins)
- In-process coding agent ("coder"): Hugging Face Inference Providers via OAuth device-flow sign-in, container-aware tools, sessions backed by an HF private bucket
- Multi-repo workspaces with cross-repo agent queries
- Containerised dev shells as a first-class concept: terminal/LSP/lint/format/build run in a single per-workspace container, only explicitly forwarded ports cross to the host
- Innovative UIs (the web is the reason we picked Tauri)

## Stack

- Tauri 2 (Rust backend + webview UI)
- Svelte 5 + TypeScript + Vite frontend
- CodeMirror 6 editor
- `@pierre/trees` (vanilla mode) for the file tree

See [specs/architecture.md](specs/architecture.md) for the high-level design and [specs/](specs/) for everything else.

## Repository layout

```
.
├── src/                    Svelte 5 UI source
├── src-tauri/              Tauri shell (Rust main, capabilities, config)
├── crates/                 Modules
├── specs/                  Living design docs
├── AGENTS.md               Instructions for AI coding agents working in this repo
├── Cargo.toml              Cargo workspace root
└── package.json            Frontend deps + scripts
```

## Prerequisites

Supported hosts: **macOS on Apple Silicon** and **Linux** (x86_64 and arm64). Windows isn't supported.

Common to both:

- Rust 1.90+ (`rustup default stable`)
- Node 20+ (we use 24)
- Bun (preferred) or pnpm

### macOS (Apple Silicon)

```bash
xcode-select --install
brew install rust bun
```

### Linux

```bash
# Linux Mint / Ubuntu 24+
sudo apt install -y libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev \
    libayatana-appindicator3-dev librsvg2-dev libssl-dev pkg-config
```

WebKitGTK provides the webview the Tauri app loads at runtime, so this set is required at both build and launch time.

## Run

```bash
bun install
bun run build:bin
./target/release/moon-desktop
```

> **Phased delivery rule** — each phase ends with a hand-back to a human reviewer. AI agents do not start the next phase on their own. See [AGENTS.md](AGENTS.md#phased-delivery).

## Before wider release

- **Publish the `moon-base` Docker image to Docker Hub.** The workspace dev image (`huggingface/moon-base`) must actually exist on Docker Hub so a fresh clone can pull it instead of building locally. See [images/moon-base/README.md](images/moon-base/README.md) and [ADR 0007](specs/decisions/0007-compose-and-moon-base.md).
- **Improve the default model / provider onboarding.** Right now the flow assumes you connect to Hugging Face first, and the default model choice after connecting could be better. Ideally:
  - Pick a sensible default model automatically after connecting to HF.
  - Let the editor be used without connecting to HF at all, as long as a model provider is supplied another way.
  - Rework the flow around "set the LLM provider" — connecting to HF becomes one option that's triggered (e.g. via the cloud icon) only when an HF provider is chosen.
