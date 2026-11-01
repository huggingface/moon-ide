# ADR 0101 — ChatGPT subscription as a coder provider

Date: 2026-11-01
Status: accepted. Extends the provider list in
[`specs/coder.md`](../coder.md#custom-openai-compatible-endpoints-openrouter-anthropic-local).

## Context

A team member pays for ChatGPT Pro and wants moon-ide's own agent loop
(tools, workers, worktrees) to draw on that plan instead of metered API
billing. They drive the coder from the desktop, from remote workspaces
(ADR 0095) and from the companion app.

The same ask for a Claude Pro/Max subscription is not possible. Since
February 2026 Anthropic's terms forbid using subscription OAuth tokens
in any product other than Claude Code and claude.ai, and the server
rejects them. The only sanctioned use is running the `claude` CLI, and
then Claude Code owns the loop. Claude stays on an API key
(`kind=anthropic`).

OpenAI allows third-party harnesses on a ChatGPT plan. There are two
routes:

- **The official "Sign in with ChatGPT" plan-usage program.** This is
  partner-only for now (interest form, an `oaiapp_` client ID per
  partner).
- **The Codex login.** The Codex CLI's public OAuth client, calling the
  Codex backend's Responses endpoint. OpenCode, Pi and OpenClaw ship
  this route, and OpenAI has endorsed them.

## Decision

- **New provider kind `chat_gpt`.** It calls
  `https://chatgpt.com/backend-api/codex/responses` (OpenAI Responses
  shape, SSE, `store: false`) with
  `Authorization: Bearer <access>` and `chatgpt-account-id`. Usage
  counts against the plan's Codex limits. The model list comes from the
  backend's `/models` catalog.
- **Login is the Codex device-code flow, run by the workspace
  process.**
  - The picker shows a code and a link (`auth.openai.com/codex/device`),
    and the workspace process polls for the result.
  - Because the workspace process does the work, sign-in behaves the
    same for local workspaces, remote workspaces (the process is on the
    server) and headless `moon-remote`. The companion can pick the
    provider once it is signed in.
  - ChatGPT requires the user to enable device-code login for Codex in
    their security settings. A 404 from the usercode endpoint becomes
    an error that says so.
- **Tokens live in the existing per-provider keyring slot**
  (`coder-provider:<id>`), as a JSON bundle (access, refresh, expiry,
  account id).
  - Signing in on the desktop and on the server gives two independent
    sessions; nothing is copied between hosts.
  - Refresh tokens rotate and are single-use, and sibling workspace
    processes on one host share the slot. Before refreshing, a process
    re-reads the keyring and adopts a newer bundle if a sibling already
    rotated. If a refresh is rejected, it re-reads once more before
    reporting "signed out".
- **Reasoning is carried across turns.** The Codex backend keeps no
  state between requests (`store: false`), so the encrypted reasoning
  items and function-call item IDs are saved on the assistant message
  (a new `ThinkingBlock` variant) and replayed on the next request.
  They are only replayed when the next request uses the same model.
  Other routes ignore the variant.
- **A plan usage-limit 429 is not retried.** Retrying cannot succeed
  before the window resets, and the limit covers every model on the
  plan, so the error surfaces at once with the reset time. Other 429s
  and 5xx use the usual backoff.

## Rejected alternatives

- **Claude subscription via a spoofed Claude Code client.** It is
  prohibited, blocked on the server, and has gotten accounts banned.
- **Driving `claude -p` / Codex app-server as the loop.** It hands the
  harness to another product, which defeats the point.
- **Loopback PKCE login (`localhost:1455`).** The browser runs on the
  desktop, but for remote workspaces the process that needs the tokens
  is on the server. That would need a desktop-shell listener that
  forwards the code. Device-code needs none of this and also works for
  headless `moon-remote`.
- **Reusing `~/.codex/auth.json`.** Rotation means two consumers of
  one refresh token log each other out.
- **Waiting for the official program.** It is not available to us yet.
  If we are accepted, it is a different client ID and authorize
  endpoint behind the same provider kind.
