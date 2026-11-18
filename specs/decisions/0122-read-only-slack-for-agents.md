# ADR 0122: Agents can read Slack, and only read it

## Context

Much of the context for a change lives in Slack: why something was
decided, who owns it, what happened during an incident. The Slack
chat panel removed in [ADR 0111](0111-coder-in-the-middle.md) was a
way for the _user_ to talk to a bot; nothing let an _agent_ look
something up. The ask is the opposite of that panel: no chat UI, no
posting, just lookup from tools.

## Decision

Two native coder tools, `slack_search` and `slack_read`, call
Slack's Web API from the IDE process with a user token.

- **Read-only is enforced by Slack.** The token is created with read
  scopes only (`search:read`, the four `*:history`, the four
  conversation `*:read`, `users:read`). There is no tool that writes,
  and a write would be refused anyway.
- **Everything the user can see**, private channels and DMs included:
  the user asked for search without restrictions, and `search:read`
  has no narrower form.
- **The token is per workspace**, in the OS keyring
  (`coder-slack:<workspace-id>`). It never reaches `session.json` or
  the dev container, where the agent's own `bash` could read it.
- **Setup is a pasted token** from a personal Slack app, checked on
  connect (identity, then one search to prove it is a user token with
  the scope). The settings modal hands out an app manifest with the
  scopes.
- **Advertised only when connected**, to sub-agents too, like
  `web_search`.
- **Slack text is untrusted input.** The tool descriptions say so, and
  results are bounded (matches cut short, a read capped) so a channel
  can't flood the context.

## Rejected alternatives

- **Slack's own MCP server** (`mcp.slack.com`). Needs an HTTP
  transport and an OAuth client in our stdio-only MCP client, a
  registered app with a client secret, and workspace-admin approval,
  to reach the same data.
- **A community stdio MCP server as a custom server.** No code, but
  its token would sit in `session.json` and in the dev container's
  process environment, readable by the agent's `bash`.
- **A machine-wide token.** Workspaces map to different teams and
  clients; one workspace's agents should not read another's Slack.
- **Narrowing scopes to leave DMs out.** Possible only with Slack's
  newer search API and its per-surface scopes; not wanted here.

## Not covered

Files' contents, canvases, reactions, and anything that writes. If an
agent's findings ever go back to Slack, the user posts them.
