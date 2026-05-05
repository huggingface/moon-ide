# ADR 0001 — Stack

Date: 2026-04-26
Status: accepted (the "ACP-native" goal and `agent-client-protocol` line are superseded by [ADR 0010](0010-coder-rewrite-not-acp.md); everything else stands)

## Context

We need to pick a stack for a team-specialized IDE built from scratch with these priorities:

- TS / Svelte / TS-adjacent languages first-class
- Native-feeling perf
- Innovative UI surface
- Multi-repo + cross-repo agent queries
- ACP-native
- Devcontainer support as a first-class concept

## Decision

- **Shell**: Tauri 2 (Rust backend, webview UI, Linux-first build).
- **Frontend**: Svelte 5 + TypeScript + Vite.
- **Editor**: CodeMirror 6.
- **File tree**: `@pierre/trees` (vanilla mode entry).
- **Diff view**: `@codemirror/merge` — CodeMirror's official side-by-side merge view. Reuses the editor's existing language / theme / editorconfig extensions on both sides; the right (working tree) side is editable so the user can fix things up inline. We tried `@pierre/diffs` first (Shiki-rendered, prettier) but found the Shiki cold-start dominated open-tab latency on larger files, and once we wanted edit-in-place that pushed us to a CM-native solution anyway.
- **Terminal**: xterm.js front, `portable-pty` back.
- **Git**: `gix` (gitoxide).
- **Indexing**: `tantivy`.
- **ACP**: `agent-client-protocol` crate.
- **Container runtime**: Docker first; podman parity later.

## Consequences

- We get web-tech UI flexibility for the "innovative UI" goal while keeping perf-critical work in Rust.
- Svelte 5 + TS matches the team's existing skills.
- We commit to writing some things from scratch (LSP multiplex, ACP host, devcontainer driver) but we get to design them with a clean host/agent split from day one.
- Choosing Pierre Trees in vanilla mode means we can keep using Svelte without a React adapter.
