# ADR 0011 — Rename `moon-agent` → `moon-remote`

Date: 2026-05-05
Status: accepted

## Context

The repo has had a `crates/moon-agent/` since Phase 0. It's a
placeholder for the **future remote-host `WorkspaceHost`** story
(SSH / Codespaces, where the host and the workspace don't share a
filesystem) — a tiny binary that links `moon-core` in agent mode and
serves JSON-RPC over a Unix socket. See
[`architecture.md`](../architecture.md#components) and the docstring
on [`crates/moon-agent/src/main.rs`](../../crates/moon-agent/src/main.rs).

It has never been built into anything beyond a stub `main.rs` that
prints a "JSON-RPC server is not implemented yet" error.

[ADR 0010](0010-coder-rewrite-not-acp.md) introduces a new crate
`crates/moon-coder/` that owns the in-process **AI coding agent**.
That makes "moon-agent" a confusing name to keep around: in the AI
ecosystem (`AGENTS.md`, agent skills, agent-client-protocol, ACP
agents, Claude Code / Cursor / pi as "agents") "agent" overwhelmingly
means "an LLM-driven agent", not "a process on the remote side of a
JSON-RPC link".

## Decision

Rename:

- `crates/moon-agent/` → `crates/moon-remote/`
- `[[bin]] name = "moon-agent"` → `name = "moon-remote"`
- All doc/spec references to "moon-agent (future remote-host
  agent)" → "moon-remote (future remote-host runtime)".

The `agent` word is reserved for the AI agent: `moon-coder` is the
crate, "Coder" is the panel name in the UI, and the AI agent
context (system prompt, tools, sessions) is what people mean when
they say "the agent" in moon-ide chat from now on.

## Why this is OK now and not later

`moon-remote` has no callers, no published binary, no shipped
behaviour. The rename is a 5-line cargo change + spec edits. Per
[`AGENTS.md`](../../AGENTS.md#no-premature-migrations) "no premature
migrations" — until the final roadmap phase ships, schemas / crate
names / binary names can be renamed, restructured, or deleted
freely.

## Why not delete it instead

Deleting now ("we'll bring it back when remote hosts are real")
loses the docstring + spec breadcrumbs that explain why
[`WorkspaceHost`](0002-workspace-host.md) is shaped the way it is —
a trait-based fs/process boundary specifically so a remote variant
is cheap to add later. The stub crate is a 40-line file that costs
nothing to keep, and its existence anchors the architectural shape
in code, not just in docs.

## Consequences

- `Cargo.toml` workspace `members` list updated.
- The single binary in `crates/moon-remote/` is `moon-remote`. No
  release artifact, no install path, no scripts that reference the
  old name (verified by `rg moon-agent` returning only spec/doc
  copy that this commit also rewrites).
- `architecture.md`, `README.md`, the new `coder.md`, and the
  phase-06 roadmap reference `moon-remote` everywhere.
- Future ADRs talking about "agent mode" of moon-core (the
  in-container / remote variant) should say "remote mode" instead
  to keep the AI/transport vocabulary clean.
