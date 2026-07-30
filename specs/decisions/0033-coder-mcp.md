# ADR 0033 — MCP support via a meta-tool surface

Date: 2026-07-16
Status: accepted (partially supersedes the "no MCP" posture in
[ADR 0010](0010-coder-rewrite-not-acp.md))

## Context

ADR 0010 ruled MCP out "until a real need shows up". It showed up:
the team wants the coder to drive a browser against running web
apps, and Playwright's MCP server is the established way to get
that. Building a bespoke playwright integration would be more code
than a minimal MCP client, and the next server request (docs
lookup, project-specific tooling) would restart the argument.

## Decision

- **Minimal hand-rolled MCP client, stdio transport only**
  (`crates/moon-coder/src/mcp.rs`): newline-delimited JSON-RPC 2.0,
  `initialize` handshake, `tools/list`, `tools/call`. No SDK dep —
  same bet as the inference client in ADR 0010. HTTP/SSE transports
  wait for a server we actually want that needs them.
- **Meta-tool surface, not direct tool exposure.** Enabled servers
  are advertised through exactly two tools: `mcp_list_tools(server)`
  and `mcp_call(server, tool, args)`, with the enabled-server list
  (id + description) embedded in their descriptions and an `enum`
  on `server`. Per-server tool schemas only enter the context when
  the model asks. Chosen over direct per-tool exposure (Claude
  Code's `mcp__server__tool` convention) to keep the advertised
  tool list stable and small regardless of how many servers get
  enabled; the cost is one `mcp_list_tools` round-trip per server
  per session.
- **Curated presets + per-workspace custom servers.** One preset
  today: `playwright` (`npx -y @playwright/mcp@latest --browser
chromium` — the server's default `chrome` channel wants real
  Google Chrome at its standard install path, which isn't
  installable on every distro; Playwright's bundled Chromium via
  `npx playwright install chromium` is). Custom
  servers (label, command, args, run target, model-facing
  description) persist per workspace on
  `WorkspaceSession.coder_mcp`, alongside the enabled-id set.
  Per-workspace because enabling playwright is a statement about
  one project's needs, and the enable toggle follows the provider-
  lock precedent.
- **Per-server run target** (`host` | `container`). Playwright
  defaults to host — driving a browser needs one installed, and
  moon-base ships none. `container` spawns via `docker exec -i`
  when the workspace shell container is `Running`, with host
  fallback (same probe as `bash`).
- **Lifecycle:** servers spawn lazily on first use and stay alive
  for the IDE process — playwright's value is a browser session
  that persists across calls. Killed on disable/remove and on IDE
  exit (`kill_on_drop`); a crashed server respawns on the next
  call.

## Rejected alternatives

- **Direct exposure of every enabled server's tools** — best model
  ergonomics for one small server, but the tool list (and its token
  cost) grows with every enabled server, and schema changes
  server-side would churn the advertised list mid-session. Can be
  revisited per-server if the indirection measurably hurts.
- **`rmcp` (official Rust SDK)** — pulls a dependency tree for a
  protocol subset we can hold in ~400 lines; we use none of the
  server-side, sampling, or resource surfaces.
- **Global enable set** — flipping playwright on for one repo
  shouldn't advertise it in every workspace; per-workspace matches
  the provider lock and hub binding.
- **Tool-result images** (playwright screenshots) — deferred. The
  pi JSONL tool-result shape is text-only today; image blocks
  render as a placeholder. The accessibility snapshot (text) is
  playwright MCP's primary interface anyway.

## Amendment 2026-07-30 — playwright in the container, shared output dir

Three changes after the preset met real use:

- **Playwright defaults to `container`, not `host`.** The original
  reason was "moon-base ships no browser", but the cost turned out
  to be one `npx playwright install chromium` on first use, cached
  in the container's home. In exchange the browser joins the
  sandbox the rest of the coder's tools already run in (a
  container-mode session no longer reaches host resources through
  the playwright back door) and sits next to the dev servers it
  exercises. Chromium dies with a container Recreate; respawn is
  on-demand, bounded by the npx cache.
- **Screenshots were landing in `$TMPDIR`** — a host `/tmp` path
  the coder's container-mode tools can't resolve (and vice versa).
  Fixed at the time with a pinned `--output-dir`; superseded by
  the `roots` capability below, which is where the server actually
  gets its output root from.
- **Container-local paths in tool results are rewritten to host
  paths** (`/workspace/<folder>/…` → the folder's host path) so
  the model can hand a reported screenshot path straight to
  `read_file`.
- Preset also gained `--headless` (the coder drives via snapshots;
  a headed window on the dev's display is noise, and containers
  have none).

## Amendment 2026-07-30 (2) — no run-target knob; tool-result images land

- **The `runs: host|container` field is gone.** Every server
  follows the workspace: `docker exec` when the shell container is
  running, host otherwise — the same probe as `bash`, and the only
  answer that ever made sense. The per-server option was removed
  before anything used the host leg; `#[serde(default)]` absorbs
  the stale key in existing `session.json` files.
- **"Tool-result images — deferred" is resolved.** MCP `image`
  content blocks (playwright screenshots) now reach the model as
  typed image blocks instead of a text placeholder, and
  `read_file` on an image file (png / jpg / gif / webp, ≤ 10 MB)
  returns the pixels the same way — the binary-file hard error
  only remains for non-image binaries. Mechanism: a tool result's
  `"images"` key is stripped from the JSON text the model reads
  and re-attached as typed blocks (Anthropic `tool_result` nested
  blocks; OpenAI-compat `image_url` parts on the tool message —
  accepted by OpenRouter et al. though strictly outside OpenAI's
  schema); images persist as pi `image` content blocks so a
  reopened session re-sends them. Compaction and the summary
  model see `[N tool-result image(s)]`, never the pixels. The
  panel and the companion render tool-result images as thumbnails
  (full-size on click); the JSON fallback strips the
  base64-bearing `images` key so expanding an `mcp_call` row
  doesn't dump megabytes of text.

  Note on playwright specifically: it returns an inline image
  block only when _it_ picks the filename. Asked to save under an
  explicit `filename`, it reports a path and no pixels — the
  model gets the image by `read_file`-ing that path, which is
  half of why the `read_file` half of this work matters.

## Amendment 2026-07-30 (3) — the `roots` capability

The client now advertises `roots` and answers the server→client
`roots/list` request with **every bound folder, active folder
first**, in the spawn target's path space (container paths for a
container spawn). This replaces relying on the server's process
cwd, which is where playwright's output dir and file-access
sandbox were previously coming from _by accident_.

Verified against a real `@playwright/mcp`: it requests `roots/list`
when the capability is advertised, and `roots[0]` overrides the cwd
fallback for both its output dir and its sandbox.

Consequences and limits:

- **Only `roots[0]` scopes playwright's file access** (`[output
dir, first root]`). Advertising the full set is still correct
  protocol behaviour and costs nothing, but a sibling bound folder
  stays unreachable — writing into it fails with `File access
denied: … outside allowed roots`. That's playwright's own
  single-workspace-root limit, not something the client can fix.
  Active-folder-first ordering is what makes the common case
  right.
- One connection is shared per workspace, so `roots[0]` is the
  folder that spawned the server. A session in a _different_
  folder therefore inherits the first folder's sandbox; a
  disable/enable respawns it. Not keyed per folder because two
  playwright servers fight over the browser profile (`Browser is
already in use … use --isolated`), which costs more than the
  wart.
- **No playwright-specific argv or argument rewriting remains.**
  The earlier `--output-dir .moon/playwright` pin and the
  bare-`filename` rewrite are both gone: with roots declared,
  placement is already deterministic, so the client no longer
  carries per-server path policy. Artefacts land in the server's
  own default, `<roots[0]>/.playwright-mcp`, and a model-supplied
  `filename` lands exactly where the model asked (playwright
  resolves bare names against the workspace root).

  Accepted cost: `.playwright-mcp/` is untracked in a repo that
  doesn't ignore it, so it shows in `git status`. Note the
  `.moon/`-is-already-excluded argument for keeping the pin does
  **not** hold — `add_dir_to_git_exclude` only runs on the
  worktree-creation path, so `.moon/` is excluded in repos where a
  worktree session was created and nowhere else. If artefact noise
  becomes annoying, the fix is generic (exclude moon-managed dirs
  when a folder is bound), not a per-server flag.

- Inbound requests we don't implement now get a JSON-RPC `-32601`
  reply instead of silence, so a server waiting on one can't
  hang. `listChanged: false`: roots are read at handshake, and a
  bound-folder change mid-session is covered by respawning.
