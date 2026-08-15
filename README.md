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
- git 2.48+ on the host. Older gits can't open a repo once a coordinator worker worktree has been created in it (the `relativeWorktrees` repo extension) — Ubuntu 24.04 / Mint 22 ship 2.43, so grab the [git-core PPA](https://launchpad.net/~git-core/+archive/ubuntu/ppa): `sudo add-apt-repository ppa:git-core/ppa && sudo apt install git`

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
  | Svelte (`.svelte`)                                        | `svelteserver`  | `bun add -D svelte-language-server`          |

  TypeScript projects on `typescript@7+` work without `@typescript/native-preview`: discovery falls back to the project-local native `tsc` (the same binary as `tsgo`, renamed upstream), version-gated so typescript@6's JS-only `tsc` is never spawned. JS/TS files additionally get **oxlint** (`oxlint --lsp`) as a linter co-tenant running alongside `tsgo`. Other file types (CSS, HTML, JSON, Markdown) have **no LSP yet** — syntax highlighting only (see [specs/roadmap.md](specs/roadmap.md)).

- **Servers spawn lazily**, one process per `(workspace, language)`, on the first open of a matching file. Nothing runs for languages you don't touch.
- **Binary discovery is ecosystem-idiomatic first, then `$PATH`**: `node_modules/.bin` for `tsgo`/`oxlint`/`svelteserver`, `.venv/bin` for `ty`, `$CARGO_HOME/bin` for `rust-analyzer`, `$GOBIN`/`$GOPATH/bin` for `gopls`. A project-pinned copy always beats a global install. If nothing is found, a status-bar pill shows a copy-pasteable install hint.
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

## Phone companion & relay

A paired phone can drive coder sessions and review work over the [companion PWA](specs/companion.md). On a shared LAN nothing needs setting up: release builds of the IDE auto-spawn a local `moon-bridge`, and the command palette's "Companion: Pair a phone…" shows the QR.

When the phone and the IDE host don't share a network, run a **standing relay** on any always-on box behind a TLS front (design: [ADR 0035](specs/decisions/0035-public-relay-deployment.md)):

```bash
# build the relay binary + the PWA it serves
cargo build --release -p moon-bridge
bun run build:companion

# on the relay box (nginx or similar terminates public TLS and
# proxies WebSocket upgrades to this listener)
moon-bridge serve --bind 127.0.0.1:53180 \
    --advertise-url wss://bridge.example.com \
    --no-idle-exit --web-root /path/to/companion-dist
```

Notes:

- Build the binary on a machine whose glibc is **not newer** than the relay box's (e.g. inside the `moon-base` workspace container, Debian 12) — a binary built against a newer glibc refuses to start there.
- `--no-idle-exit` keeps the relay up with zero local workspaces (the local auto-spawned bridge must **not** set it); `--advertise-url` is what pairing QRs point phones at.
- An enrollment code prints at startup (120 s, single-use). Enter it in the IDE via command palette → "Companion: Connect to remote bridge…"; the IDE stores a token and reconnects on its own from then on. Each open workspace registers itself, so the phone sees them all.
- The keyring backend needs a Secret Service even headless — run under `dbus-run-session` with an unlocked `gnome-keyring-daemon` (see the ADR).
- Phone pairing QRs are minted on demand from any enrolled IDE's Companion panel; no relay restart needed.

## Headless IDE (`moon-remote`)

A remote machine can serve its workspaces to the companion with no desktop session ([ADR 0059](specs/decisions/0059-headless-enrolled-ide.md)): `moon-remote` is an enrolled IDE without a webview — same relay protocol, same keyring, same coder. First-time setup on the box:

```bash
cargo build --release -p moon-remote   # same glibc caveat as the relay
scp target/release/moon-remote box:~/bin/

# on the box
moon-remote login                      # HF device flow: prints a URL + code
moon-remote enroll --bridge wss://bridge.example.com --code XXXX-XXXX
moon-remote workspace-add --name myproject --folder ~/code/myproject
moon-remote model --standard moonshotai/Kimi-K3   # optional; empty = default
moon-remote serve --workspace myproject
```

Notes:

- Everything shares the desktop's dirs + keyring (`state.json`, per-workspace `session.json`, coder sessions), so a machine can flip between desktop and headless serving of the same workspaces — `model` edits the same picks as the desktop's picker, and a running `serve` re-reads them on restart.
- A Secret Service must be reachable: on a desktop box, the login session's keyring (auto-login + an empty-password login keyring make reboots hands-off); on a server, the `dbus-run-session` + `gnome-keyring-daemon` recipe from ADR 0035.
- For boot persistence, run `serve` from a `systemd --user` template unit (with `loginctl enable-linger`), e.g. `moon-remote@<workspace>.service` with `Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=%t/bus` and `Restart=always`.
- `workspace_launch` from the phone spawns a sibling `serve` process for stopped workspaces; a `workspace-add` while serving shows up on the phone after the unit restarts (Register refresh is a known gap).

Workspace management is CLI-first: `workspace-add` binds a folder,
`workspace-remove-folder` unbinds one, `mcp --workspace <slug>
[--enable playwright]` lists / toggles MCP servers, and `model`
sets the model picks. All of these edit the same per-workspace state
the desktop writes; stop the workspace's `serve` before mutating and
restart it after.

## `moon-base` docker image

Used for workspace containers, if not wanting to run dev processes on host machines.

```
docker build -t moon-base:dev images/moon-base/
```

## Before wider release

- **Scope companion pairing to IDEs, not the relay.** Today a relay is a single trust domain: phone tokens and IDE tokens are both relay-wide, so _any_ paired phone can drive _every_ enrolled IDE — and driving an IDE means running its coder, i.e. arbitrary commands on that host. That's the deliberate single-operator posture of [ADR 0031](specs/decisions/0031-remote-bridge-relay.md)/[0035](specs/decisions/0035-public-relay-deployment.md) ("enrollment is the boundary"), and it's wrong the moment a relay is shared. Before release, a phone token must carry an **allowed-IDE set** — natural default: only the IDE that minted its pairing QR — enforced by the relay on every `call`/`subscribe`/`workspaces` route, plus a management surface to grant/revoke bindings per phone. One relay then serves many IDEs and many phones, each phone bound to only the IDEs it was invited to.
- **Make the relay untrusted: end-to-end auth between phone and IDE.** Scoping (above) still trusts the relay to enforce it — today the relay terminates TLS, sees plaintext JSON-RPC, and could _originate_ commands to any enrolled IDE; a compromised relay box owns every IDE behind it. Before release the relay must become a blind pipe: pairing mints a device keypair (the QR already carries a secure channel for the phone's public key), the phone **signs every frame** (with a nonce / monotonic counter against replay), and the **IDE verifies** against the pinned device key before dispatching — relay-forged frames simply fail verification. Ideally the payloads are also **end-to-end encrypted** phone↔IDE (e.g. a Noise-style session over the relayed channel), which additionally stops the relay reading transcripts and file contents — downgrading ADR 0035's "the VPS is the trust boundary" consequence to "the VPS can deny service". Relay-side tokens remain as routing + DoS control, not as the security boundary.
- **Deal with old host gits (< 2.48) or ship our own.** Coordinator worker worktrees are created with `git worktree add --relative-paths` (git ≥ 2.48, enforced at creation time — the devcontainer's git usually satisfies it), which writes `extensions.relativeWorktrees` into the parent repo's config. From that moment an older git — for example the _host's_ `/usr/bin/git` 2.43 on Ubuntu 24.04 / Mint 22 while the worktree was created by the container's newer git — refuses to open the repo **at all**: `status`, `branch`, blame, commit, even `config --get` fail. We hit this in the field (host-side SCM silently dead on every repo a coordinator had touched; `remote_web_url` already sidesteps it by reading `.git/config` as a file). Before release either: detect an old host git up front and surface an actionable "upgrade git" error on folder bind (not a cryptic per-command failure), or bundle/download a modern static git binary and use it for all host-side git. Erroring early is the cheap first step; shipping our own git removes the failure class entirely.
- **Publish the `moon-base` Docker image to Docker Hub.** The workspace dev image (`huggingface/moon-base`) must actually exist on Docker Hub so a fresh clone can pull it instead of building locally. See [images/moon-base/README.md](images/moon-base/README.md) and [ADR 0007](specs/decisions/0007-compose-and-moon-base.md).
- **Make PORTS exposing more intuitive** - eg autodetects ports or w/e to prompt exposing them, to avoid debugging (why doesn't my webapp work?). And make them work without --host.
- **Improve the default model / provider onboarding.** Right now the flow assumes you connect to Hugging Face first, and the default model choice after connecting could be better. Ideally:
  - Pick a sensible default model automatically after connecting to HF.
  - Let the editor be used without connecting to HF at all, as long as a model provider is supplied another way.
  - Rework the flow around "set the LLM provider" — connecting to HF becomes one option that's triggered (e.g. via the cloud icon) only when an HF provider is chosen.
