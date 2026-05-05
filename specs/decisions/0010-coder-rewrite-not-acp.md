# ADR 0010 — Coder phase: in-process Rust loop, not ACP host

Date: 2026-05-05
Status: accepted

## Context

The Phase 6 plan in [`roadmap.md`](../roadmap.md), the "ACP-native"
goal in [ADR 0001](0001-stack.md), and the `agent-client-protocol`
crate listed in the stack all assumed the same shape: moon-ide hosts
ACP, the user picks an external agent binary (opencode / claude code /
cursor-agent / pi-coding-agent / …), tool calls travel over ACP, and
the IDE renders chat + tool stream + edit preview.

In practice that shape buys less than it looked like:

- The agents the team would pick (Claude Code, Cursor, opencode) are
  **not ACP-native**. Wrapping them costs us the audit surface and
  feature parity gap of every adapter, indefinitely.
- The agents that **are** ACP-native (notably
  [`pi-coding-agent`](https://github.com/badlogic/pi-mono)) are
  TypeScript / Node-first. Embedding them as a sidecar means shipping
  a Node runtime with a Rust IDE, and being subject to their release
  cadence.
- The whole point of routing tool calls through `WorkspaceHost`
  (so containerized workspaces get container-bound tools — see
  [ADR 0002](0002-workspace-host.md)) is best served when **moon-core
  owns the agent loop** and dispatches tools as moon-core methods.
  ACP-via-third-party agents have to be persuaded to use whatever
  fs/bash protocol _they_ understand, then bridged to ours.
- Moon-ide is for one team. The "pluggable agent" surface is a
  productized concern — see the scope-discipline section in
  [`AGENTS.md`](../../AGENTS.md#scope-discipline).

[`pi-coding-agent`'s shape](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent)
is the closer reference: a stateful loop with a typed event stream
(`agent_start`, `turn_start`, `message_start/update/end`,
`tool_execution_start/update/end`, `turn_end`, `agent_end`), tool
dispatch with `beforeToolCall` / `afterToolCall` hooks, and steering /
follow-up message queues. That's a few hundred lines of Rust on top
of `reqwest` + an SSE parser, not a port of the whole pi monorepo.

## Decision

Phase 6 becomes **"Coder" — an in-process coding agent owned by
moon-core**. The agent loop, the provider clients, the tool surface,
and the session store all live in a new crate `crates/moon-coder/`
([ADR 0011](0011-rename-moon-agent-to-moon-remote.md) frees the
`moon-agent` name by renaming the existing remote-host stub to
`crates/moon-remote/`).

Concretely:

- **No ACP host.** The `agent-client-protocol` dependency is dropped.
- **Provider day-1**: HF Inference Providers via the OAuth Device
  Authorization Grant (RFC 8628) for the HF Hub OAuth app (client ID
  `7977dff4-917a-4cf9-a726-dd45e25faa5f`) with `inference-api +
contribute-repos` scopes (deliberately weaker than `manage-repos`
  — see [`coder.md`](../coder.md#hf-oauth-device-authorization-grant-rfc-8628)). OpenAI-compatible HTTP adapter against
  `https://router.huggingface.co/v1`. Models carried verbatim
  (`Qwen/Qwen3.5-397B-A17B:scaleway`, `Qwen/Qwen3.6-35B-A3B:deepinfra`,
  …). OAuth tokens live in the OS keyring under
  `service=moon-ide`, `account=hf-oauth`. Refresh-token rotation is
  handled by an HTTP middleware on the inference client.
- **Tool surface**: `read_file`, `write_file`, `edit_file`,
  `list_dir`, `grep`, `bash`, all dispatched as moon-core methods and
  routed through the active `WorkspaceHost`. A workspace running in a
  container (Phase 2) gets container-bound tools without the agent
  knowing or caring. IDE-native tools (`goto_definition`,
  `find_references`, `git_status`, `git_diff`) follow as separate
  commits when proven needed.
- **Sessions**: append-only JSONL in `<workspace>/.moon/agent-sessions/`,
  mirrored to a per-user private HF bucket (`<user>/moon-ide-sessions`,
  one folder per workspace) via `hf-xet` + the `repo_type="bucket"`
  Hub APIs. Sync is on-by-default with a per-workspace opt-out toggle
  in the panel header, and a banner the first time a fresh workspace
  starts uploading. See [`coder.md`](../coder.md).
- **Custom providers** (OpenRouter, local OpenAI-compatible servers,
  Anthropic OAuth for Claude Pro/Max, …): structured config in
  `AppState.coder.providers[]` with per-provider keyring entries. Not
  in the initial sub-phases — added when concrete need lands.
- **Sub-agents**: a `spawn_subagent { model, system_prompt, task,
allowed_tools }` tool exposed to the parent loop. The sub-agent
  runs the full loop in isolation with its own message history and
  returns a single text result so the parent's context only sees the
  summary. Default sub-agent model is the configured "fast" default.
  Designed for cheaper-model research / simple-refactor tasks. Not in
  the initial sub-phases.

## Why this is OK now and not later

Per [`AGENTS.md`](../../AGENTS.md#no-premature-migrations) "no
premature migrations" — there is no shipped ACP integration. The
ACP framing exists in copy (this ADR's job to remove) but no code
references the `agent-client-protocol` crate. The pivot is
spec-only at the moment of this ADR.

## Why not embed pi-agent-core

We considered three embed shapes for pi's existing TS loop:

- **Node sidecar** (`pi --mode rpc` from the Tauri shell). Adds a
  Node runtime to a Rust IDE, ties our release cadence to pi's, and
  the bash / read / write tools still need a bridge to route through
  `WorkspaceHost`. The bridge is most of the work.
- **In-webview** (pi-agent-core is browser-compatible). Puts fs +
  bash tools in JavaScript, which violates the architecture
  invariant that the UI never directly touches fs / git / LSP /
  terminal. Hard no.
- **Rust port of just the loop**. The loop itself is a few hundred
  lines (provider streaming, tool dispatch, event vocabulary). The
  parts that look big in pi (interactive TUI, extension API, package
  manager, prompt templates, themes, OAuth flows for ~20 providers)
  are productized concerns we don't need.

The Rust port wins on every axis that matters here.

## What this means for what we don't ship

Pi's "aggressively extensible so it doesn't dictate your workflow"
philosophy is great for a generic agent CLI; it's noise for "one
team's IDE". We deliberately skip:

- **Extensions / plugin API** — pi's "register your own tool / sub-
  agent / plan mode / permission gate in TS" surface. No equivalent
  in moon-coder.
- **Themes for the agent panel** — uses the IDE theme, full stop.
- **Prompt templates as a first-class command surface** — a
  template is just a `.md` file, the user can paste it. We'll
  reconsider if a real workflow asks.
- **Package manager (pi-packages, npm/git installable bundles)** —
  hard no for a desktop IDE this size.
- **MCP** — same posture as pi: no MCP. If a real need shows up,
  we add one specific MCP-style hook then.
- **Plan mode / permission popups / sub-agents / built-in todos /
  background bash** in the initial slice. Sub-agents are the one
  item with a documented later path; the rest stay out unless
  somebody surfaces a concrete request.

The one extensibility surface we _do_ keep is **`SKILL.md` discovery**
(reading `AGENTS.md` and any `.moon/skills/` / `.cursor/skills*/` /
`.agents/skills/` files into the system prompt), because that's a
file convention, not code, and the moon-ide repo itself already
participates in it.

## Consequences

- New crate `crates/moon-coder/` (loop, providers, tools, sessions,
  sync). Renames the existing `crates/moon-agent/` placeholder per
  [ADR 0011](0011-rename-moon-agent-to-moon-remote.md).
- New top-level spec [`coder.md`](../coder.md) and a new
  [phase-06 sub-phase breakdown](../roadmaps/phase-06-coder.md).
- The ACP line in [ADR 0001](0001-stack.md), the ACP host references
  in [`architecture.md`](../architecture.md) and
  [`protocol.md`](../protocol.md), the "agent" entry in
  [`AGENTS.md`](../../AGENTS.md#cross-cutting-invariants), and the
  ACP-related copy in [`README.md`](../../README.md),
  [`containers.md`](../containers.md),
  [`slack-chat.md`](../slack-chat.md), and
  [`phase-11-slack-chat.md`](../roadmaps/phase-11-slack-chat.md) all
  point at coder instead. Phase 6 in the roadmap is rewritten.
- The cross-cutting invariant "the UI never directly touches git /
  LSP / fs / ACP / terminal" becomes "git / LSP / fs / coder /
  terminal".
- The "ACP-native" line in ADR 0001 is marked partially superseded
  by this ADR. We do not rewrite ADR 0001 — supersession is the
  ADR convention.

## Reopening criteria

Reasons we'd revisit and adopt ACP later:

- An ACP-native agent appears that the team specifically wants to
  use, _and_ embedding it into moon-coder's loop would cost more
  than wrapping ACP. Concrete request, not speculation.
- The team wants moon-ide-the-agent to be drivable by external
  clients (CI, another IDE, a Slack bot, …). At that point we'd
  wrap an ACP **server** _around_ moon-coder's loop — the inverse
  direction of what Phase 6 originally planned, and additive on
  top of what this ADR commits to.
