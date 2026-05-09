# ADR 0005 — Self-hosting / bootstrap requirement

Date: 2026-04-26
Status: accepted

## Context

Moon IDE is written in Rust + TypeScript + Svelte and uses native binaries
(Tauri shell, `cargo`, `bun`/`node`, `tsgo`, `oxlint`, `oxfmt`,
`prettier`) plus platform-specific system libraries (the macOS WebKit
framework on Apple Silicon Macs, `libwebkit2gtk-4.1` on Linux).

Eventually the team will use Moon IDE to develop Moon IDE itself. That
means a fresh checkout, opened with the IDE, must be a fully working dev
environment. Most of the toolchain runs inside the workspace's container
(Phase 2 — see [`containers.md`](../containers.md)); the parts that
genuinely have to live on the host (the platform-native Tauri binary
build chain, principally) are explicit and minimal. The IDE has to
support the languages it is itself written in.

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

### Container image for Moon IDE itself

Moon IDE is itself a workspace; per
[`containers.md`](../containers.md) opening it generates a
workspace `compose.yaml` (added in Phase 2) referencing the
moon-published `moon-base` image, which carries:

- A modern Linux base (Debian stable) with `libwebkit2gtk-4.1-dev`,
  `libsoup-3.0-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`,
  `librsvg2-dev`, `libssl-dev`, `pkg-config`.
- `rustup` with the workspace's `rust-version`.
- `bun` (or `node` LTS as fallback) for the frontend toolchain.
- `oxlint`, `oxfmt`, `prettier`, `tsgo` cached as dev dependencies of
  the JS workspace — no global installs.
- A non-root `dev` user with passwordless sudo so the contributor
  can install extra tooling as they go. The container itself runs
  unprivileged with the default Docker capability set; project
  side-services (databases, caches, …) come up as siblings on the
  host's daemon via compose `include:` rather than nested inside
  the workspace — see
  [ADR 0008](0008-host-shared-daemon.md).

Forwarded ports are explicit: only Vite (1420) and the Tauri devtools
port. Everything else stays inside the container.

### Per-host bootstrap

The moon-ide binary is host-native on every platform — it links
against a platform-provided webview (WebKitGTK on Linux, WKWebView on
macOS) at startup. That makes the moon-ide _build chain_ host-resident
too: a Tauri build inside a Linux container can't produce a macOS
`.app`, and even a Linux binary built inside the container has to find
WebKitGTK on the host at launch. What the container _does_ cover is
everything around that build — Rust + JS toolchain for project
tooling, lint, format, tests, sidecar processes.

**macOS contributors (Apple Silicon — the team's primary platform).**
Host needs: Xcode Command Line Tools (`xcode-select --install`),
`rustup`, `bun`, Docker Desktop with VirtIO file sharing enabled (for
the bind-mount performance story in
[`containers.md`](../containers.md#failure-modes)). macOS provides
WebKit, so no extra package is needed for the webview itself. The
container handles project tooling (`bun run check`, lint, format,
tests) and, when wanted, the cross-built Linux artefact.

**Linux contributors.** Host needs: WebKitGTK dev libraries (the
`libwebkit2gtk-4.1-dev` apt set listed in
[the README](../../README.md#linux)) — required at both build time
and runtime — plus `rustup`, `bun`, Docker Engine + Compose v2. The
container handles project tooling. WebKitGTK doesn't move inside the
image because the binary loads it at launch on the host.

**Windows host.** Not supported. Surface if/when somebody picks it
up — see [`containers.md`'s out-of-scope](../containers.md#out-of-scope-for-phase-2-and-when-to-revisit).

### What this implies for the IDE features

- Rust support cannot be a second-class citizen; Phase 4 (LSP) ships
  `rust-analyzer` integration alongside the TS/Svelte stack.
- Toolchain bootstrap for **arbitrary project work** runs inside the
  active `WorkspaceHost`. moon-ide-on-moon-ide is the special case:
  the moon-ide Tauri build chain itself stays host-side on both
  platforms because the binary it produces is host-native. The
  per-host bootstrap above lists what that means in package terms.
- The terminal (Phase 3) defaults to a shell with the toolchain on
  `PATH` inside the container.
- The editor itself must respect the repo's `.editorconfig` so that
  editing moon-ide-in-moon-ide produces output identical to what
  `oxfmt` / `prettier` / `rustfmt` would emit. This is the gating item
  for Phase 1.5; see [editorconfig.md](../editorconfig.md).

## Consequences

- Phase 2 spec is upgraded: the workspace's container must produce a
  usable Rust + JS/TS dev environment, not just a terminal. The LSP
  and lint phases inherit this requirement.
- `moon-base`'s release CI runs a smoke test that does
  `git clone moon-ide && bun install && cargo check` inside the
  freshly built image — green is a release gate. Documented in
  [phase-02-containers.md](../roadmaps/phase-02-containers.md#bootstrap-concern).
- We resist adding a third primary language (Go, Python, etc.) until
  the existing ones feel right.
- The bootstrap test is concrete: open the moon-ide repo with moon-ide,
  accept the "Run this project in a container?" prompt, and you can
  edit, lint, format, build and run the app from the in-container
  tooling alone.
