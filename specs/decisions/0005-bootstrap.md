# ADR 0005 — Self-hosting / bootstrap requirement

Date: 2026-04-26
Status: accepted

## Context

Moon IDE is written in Rust + TypeScript + Svelte and uses native binaries
(Tauri shell, `cargo`, `bun`/`node`, `tsgo`, `oxlint`, `oxfmt`,
`prettier`) plus system libraries (`libwebkit2gtk-4.1`, etc.).

Eventually the team will use Moon IDE to develop Moon IDE itself. That
means a fresh checkout, opened with the IDE, must be a fully working dev
environment — including inside a devcontainer. The IDE has to support
the languages it is itself written in.

## Decision

### Languages we commit to running well in-IDE

The following toolchains are tier-1 — used by `moon-ide` itself, so we
own their UX end-to-end:

- **Rust** stable + nightly toggle, `rustfmt`, `clippy`, `rust-analyzer`.
  Includes Tauri-specific build (the workspace contains both a library
  and a Tauri app).
- **TypeScript** with `tsgo` (TS native preview) as primary, classic `tsc`
  as fallback for tooling that hasn't migrated.
- **JavaScript / JSX / TSX** via the same chain.
- **Svelte** with `svelte-language-server` and `svelte-check`.
- **JSON / JSONC**, **CSS / SCSS / Less**, **HTML**, **Markdown** —
  config and docs of our own repo.

These also drive Phase 4 (LSP) and Phase 8 (lint/format) priority.

### Devcontainer image for Moon IDE itself

The repo ships a `.devcontainer/devcontainer.json` (added in Phase 2)
that installs:

- A modern Linux base with `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`,
  `libgtk-3-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`,
  `libssl-dev`, `pkg-config`.
- `rustup` with the workspace's `rust-version`.
- `bun` (or `node` LTS as fallback) for the frontend toolchain.
- `oxlint`, `oxfmt`, `prettier`, `tsgo` cached as dev dependencies of
  the JS workspace — no global installs.
- A non-privileged user with sudo so the contributor can install extra
  tooling as they go.

Forwarded ports are explicit: only Vite (1420) and the Tauri devtools
port. Everything else stays inside the container.

### What this implies for the IDE features

- Rust support cannot be a second-class citizen; Phase 4 (LSP) ships
  `rust-analyzer` integration alongside the TS/Svelte stack.
- Toolchain bootstrap (rustup/bun install) must run **inside the active
  `WorkspaceHost`**, not on the host. A first-run task is acceptable
  but the host machine should not need any pre-installed Rust / Node /
  Bun for the dev workflow.
- The terminal (Phase 3) defaults to a shell with the toolchain on
  `PATH` inside the container.
- The editor itself must respect the repo's `.editorconfig` so that
  editing moon-ide-in-moon-ide produces output identical to what
  `oxfmt` / `prettier` / `rustfmt` would emit. This is the gating item
  for Phase 1.5; see [editorconfig.md](../editorconfig.md).

## Consequences

- Phase 2 spec is upgraded: the devcontainer must produce a usable Rust
  - JS/TS dev environment, not just a terminal. The LSP and lint phases
    inherit this requirement.
- We resist adding a third primary language (Go, Python, etc.) until
  the existing ones feel right.
- The bootstrap test is concrete: open the moon-ide repo with moon-ide,
  hit "Reopen in container", and you can edit, lint, format, build and
  run the app from the in-container tooling alone.
