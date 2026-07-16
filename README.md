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

## Language support

### LSP

Full details in [specs/lsp.md](specs/lsp.md). The short version:

- **Detection is by file extension**, mapped to an LSP language id in `src/lib/editor/lspLanguage.ts`. Each language is wired to exactly one server (there is no server registry or configuration):

  | Language                                                  | Server          | Install                                      |
  | --------------------------------------------------------- | --------------- | -------------------------------------------- |
  | TypeScript / JavaScript (`.ts`, `.tsx`, `.js`, `.jsx`, …) | `tsgo`          | `bun add -D @typescript/native-preview`      |
  | Rust (`.rs`)                                              | `rust-analyzer` | `rustup component add rust-analyzer`         |
  | Python (`.py`, `.pyi`)                                    | `ty`            | `uv add --dev ty`                            |
  | Go (`.go`)                                                | `gopls`         | `go install golang.org/x/tools/gopls@latest` |

  JS/TS files additionally get **oxlint** (`oxlint --lsp`) as a linter co-tenant running alongside `tsgo`. Other file types (Svelte, CSS, HTML, JSON, Markdown) have **no LSP yet** — syntax highlighting only. `svelte-language-server` and friends are on the roadmap (see [specs/roadmap.md](specs/roadmap.md)).

- **Servers spawn lazily**, one process per `(workspace, language)`, on the first open of a matching file. Nothing runs for languages you don't touch.
- **Binary discovery is ecosystem-idiomatic first, then `$PATH`**: `node_modules/.bin` for `tsgo`/`oxlint`, `.venv/bin` for `ty`, `$CARGO_HOME/bin` for `rust-analyzer`, `$GOBIN`/`$GOPATH/bin` for `gopls`. A project-pinned copy always beats a global install. If nothing is found, a status-bar pill shows a copy-pasteable install hint.
- **Container routing**: when the workspace shell container is running, servers spawn _inside_ it via `docker exec` (so they see the same filesystem the build sees), with automatic per-language fallback to a host server when the binary isn't available in the container.
- Debugging "why isn't my server up?": the bottom-panel Logs view has a per-server `lsp.<language>` source with discovery and routing decisions.

### Format on save

Full details in [specs/editorconfig.md](specs/editorconfig.md) and [ADR 0013](specs/decisions/0013-format-on-save-file-based.md). Formatting runs on **every editor save** (`Ctrl+S`) — hardcoded on, no toggle. Coder file edits defer the same pipeline to the end of the agent turn. Two stages:

1. **`.editorconfig` normalization** (in-memory, always): line endings, trailing whitespace, final newline.
2. **Formatter chain** (against the on-disk file):
   - If the project has a **lint-staged config** (`.lintstagedrc.json` or `package.json#lint-staged`) with a rule matching the file, those commands run in order — that's the per-repo source of truth (this repo uses oxfmt, prettier, and rustfmt this way).
   - Otherwise a **language-default fallback** fires: `rustfmt --edition <detected>` for `.rs`, `ruff format` for `.py`/`.pyi` (preferring the project's `.venv/bin/ruff`), `gofmt -w` for `.go`. No fallback exists for other extensions — a file with no lint-staged rule and no fallback just gets the editorconfig pass.

   A missing formatter binary logs a one-time warning and the save proceeds with the normalized bytes.

Like LSP, the formatter chain runs inside the workspace shell container when one is up.

## `moon-base` docker image

Used for workspace containers, if not wanting to run dev processes on host machines.

```
docker build -t moon-base:dev images/moon-base/
```

## Before wider release

- **Publish the `moon-base` Docker image to Docker Hub.** The workspace dev image (`huggingface/moon-base`) must actually exist on Docker Hub so a fresh clone can pull it instead of building locally. See [images/moon-base/README.md](images/moon-base/README.md) and [ADR 0007](specs/decisions/0007-compose-and-moon-base.md).
- **Improve the default model / provider onboarding.** Right now the flow assumes you connect to Hugging Face first, and the default model choice after connecting could be better. Ideally:
  - Pick a sensible default model automatically after connecting to HF.
  - Let the editor be used without connecting to HF at all, as long as a model provider is supplied another way.
  - Rework the flow around "set the LLM provider" — connecting to HF becomes one option that's triggered (e.g. via the cloud icon) only when an HF provider is chosen.
