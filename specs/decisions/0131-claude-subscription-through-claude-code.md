# ADR 0131: Claude subscription sessions are driven by Claude Code

Supersedes the "driving `claude -p` as the loop" rejection in
[ADR 0101](0101-chatgpt-subscription-provider.md) for Claude only.

## Context

The team wants agents on their Claude Pro/Max plans, as ChatGPT plans
already work (ADR 0101). Anthropic bills any third-party tool that
signs in with a Claude account per token from "extra usage", never
from the plan, and forbids tools from offering that sign-in. The plan
covers Anthropic's own clients, `claude` among them, including its
headless `-p` mode.

## Decision

- **A provider kind `claude_code`.** A session using it runs no
  moon-ide inference: each turn is a `claude -p` process in streaming
  JSON mode, resumed by Claude Code's own session id (kept in the
  session header). Its output is translated into moon-ide's events and
  session records, so the desktop and the companion render it like any
  other session; steering writes to its stdin, stop interrupts it.
- **It runs where agents' commands run**: in the workspace's dev
  container (`docker exec`), so its file and shell tools are
  container-bound like moon-ide's own. A workspace without a running
  container runs it host-side, as `bash` does.
- **Not in `moon-base`.** Claude Code ships almost daily and updates
  itself; it is installed on first use into the container's persisted
  volume, with its config (sessions included) there too.
- **Sign-in is `claude setup-token`**, a long-lived token for Claude
  Code itself, kept in moon-ide's secrets and passed per process as
  `CLAUDE_CODE_OAUTH_TOKEN`; it is never written into the container.
- **Plan usage** comes from Claude Code's own `rate_limit_event`
  messages (five-hour and weekly windows), shown as the ChatGPT plan
  gauge is. moon-ide calls no Anthropic endpoint itself.
- **What Claude Code owns in those sessions:** the system prompt,
  compaction, model choice and its sub-agents. moon-ide's own tools
  (workers, `ask_user`, review notes, the browser tab) come later,
  over MCP. It runs with permissions bypassed: moon-ide's own agent
  asks before nothing either, and the container is the boundary.

## Rejected

- **The subscription token in moon-ide's own loop.** Billed as extra
  usage at API rates, which an API key already gives, and forbidden
  for the tool that offers it.
- **Baking `claude` into `moon-base`.** Always stale, and puts a
  proprietary CLI in the team's public base image for everyone.
- **Running `claude` beside the IDE process instead of in the
  container.** Its tools would act outside the container.
- **The Agent SDK.** TypeScript/Python, and its docs require an API
  key; the CLI's stream-json mode is the same protocol without the
  runtime.
