# Coder — the in-process AI coding agent

The right-side coder panel is moon-ide's own coding agent. Unlike
the [Slack chat panel](slack-chat.md) (a thin chat client over
somebody else's bot), the coder owns its loop end-to-end: it talks
to LLMs directly, dispatches its own tool calls, and routes every
tool call through the active [`WorkspaceHost`](architecture.md#workspacehost-phase-2)
so a containerized workspace gets a containerized agent without
extra plumbing.

[ADR 0010](decisions/0010-coder-rewrite-not-acp.md) explains why
we don't host ACP. [ADR 0011](decisions/0011-rename-moon-agent-to-moon-remote.md)
explains why the crate is `moon-coder`. Sub-phase work breakdown:
[`roadmaps/phase-06-coder.md`](roadmaps/phase-06-coder.md).

## Why we own the loop

ACP is a protocol for driving somebody else's agent binary. The
agents the team would actually pick aren't ACP-native, and the ones
that are (pi-coding-agent) are TS-first — adopting one means a Node
sidecar or fs/bash tools in JS land, breaking the architecture
invariant. Owning the loop in Rust is a few hundred lines around
`reqwest` + an SSE parser + a tool dispatcher, and every tool is a
moon-core method that already respects `WorkspaceHost`.

## Loop shape

The vocabulary borrows from
[pi-agent-core](https://github.com/badlogic/pi-mono/tree/main/packages/agent)
but the wire shape is flatter:

```
prompt(msg)
├─ user_message               { id, text }
├─ assistant_message_start    { id }                        // fires on first content OR thinking delta
├─ assistant_thinking_delta   { id, delta }*                // optional, before/interleaved with content
├─ assistant_message_delta    { id, delta }*                // SSE chunks of the answer
├─ assistant_message_end      { id, text, thinking? }       // canonical full content + reasoning
├─ tool_call                  { id, name, args }*           // one per call (assembled)
├─ tool_result                { id, result, is_error }*     // when each tool finishes
│
├─ assistant_message_start    { id' }                       // next iteration if tools fired
├─ …
└─ turn_complete | aborted | error                          // exactly one of the three
```

Mirrored 1:1 in `src/lib/protocol.ts:CoderEvent`. Stable IDs let the
frontend reconcile deltas onto one bubble; `assistant_message_end`'s
`text` / `thinking` are authoritative if concatenated deltas drift.

Reasoning traces stream as `assistant_thinking_delta` (we accept both
`reasoning_content` and `reasoning` on the wire) and render as a
collapsible block that auto-collapses on `assistant_message_end`.
Models without a trace simply never emit thinking deltas.

Tool calls stream incrementally off the SSE wire but the loop only
fires `tool_call` once the call is fully assembled — partial JSON is
not useful to render. Tool calls in one assistant turn dispatch
sequentially; parallel dispatch lands when a real workload needs it.

Tool rows render collapsed by default; expanded bodies mount lazily
on first expand so a long tool-heavy session doesn't pay a
grammar-load + highlight pass per row on initial paint (test plan
0076). The transcript is windowed — only a slice of rows is in the
DOM, growing on scroll-up and capped at 300 mounted rows (test plan
0093 covers the mechanics). `Ctrl+F` only matches loaded rows.

Three control surfaces:

- **Abort** (`Esc`): cancels the in-flight HTTP / SSE / tool call.
  The session keeps the partial assistant message and completed tool
  calls. Aborting drops any queued steers. The cancel token is raced
  against _every_ network await in a turn — route resolution (which
  may trigger an OAuth token refresh), the HTTP send, the SSE read
  loop, and the 401-retry refresh — so Esc lands immediately
  regardless of which phase the turn is in. Inference and auth HTTP
  clients also carry a connect timeout so a black-holed endpoint
  can't park a turn even if nobody clicks stop.
- **Steer** (Enter while streaming / running tools): the composer
  stays editable mid-turn. The message is queued, shows up in the
  transcript immediately as a muted "queued" row, and the running
  loop drains the queue before its next LLM call — including one
  extra round-trip when a steer arrives during the final assistant
  message. Steers are persisted at drain time (the chat-completions
  shape forbids a user message between an assistant `tool_calls` and
  its results).
- **Go now** (a "go now" button on the queued row): the user typed a
  steer mid-turn but doesn't want to wait for the running turn to
  settle. Cancels the current turn (like abort) and lets the spawn
  loop drain that steer into a fresh turn immediately — no `Aborted`
  flash, just the old thinking fading into the new turn. The affordance
  lives on the queued message in the transcript, not in the composer,
  so it targets the exact steer it's attached to (`coder_drain_steer_now`
  by id). A stale click — the runner already drained the queue at its
  last iteration top — is a silent no-op. The loop-back mints a fresh
  `CancellationToken` for the drained turn: `CancellationToken` is
  one-shot, so the just-cancelled token can't be reused — the new
  `run_turn` would bail at its iteration-top guard before the steer
  drains, spinning the loop forever with `busy` stuck and stop dead.
- **Follow-up** (Alt+Enter while idle-but-just-finished): future —
  not implemented. Sending while idle starts a fresh turn.

Keymap matches the Slack panel (Enter sends, Shift+Enter newline).

## Authentication

### HF OAuth Device Authorization Grant (RFC 8628)

The IDE registers a public OAuth app on Hugging Face Hub with client
ID **`7977dff4-917a-4cf9-a726-dd45e25faa5f`**. No client secret.
Scopes asked upfront:

| Scope              | Used by     | What it gets us                                                                                                                                                          |
| ------------------ | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `inference-api`    | LLM calls   | Call HF Inference Providers' router endpoint                                                                                                                             |
| `contribute-repos` | Bucket sync | Create + push to the per-workspace trace bucket. The minimum scope that allows `create-repo` + `push`; deliberately not `manage-repos` (which would also grant deletion) |

`read-billing` and `email` are deliberately not requested.

### Flow

1. User clicks "Sign in with Hugging Face". `moon-coder` `POST`s
   `https://huggingface.co/oauth/device` with the client id + scopes.
2. The IDE shows the returned `user_code` in a modal with an "Open in
   browser" button for `verification_uri`.
3. Background poll against `https://huggingface.co/oauth/token`
   (`grant_type=…:device_code`) until it returns `access_token`,
   `refresh_token`, `expires_in`.
4. The triple persists to the OS keyring as one JSON blob under
   `service=moon-ide`, `account=hf-oauth` (same keyring backend as
   the Slack panel).
5. The IDE pulls the user's profile (`/api/whoami-v2`) for the panel
   header; cached in `AppState.coder.identity`.

### Refresh

The HTTP middleware refreshes the access token when
`expires_at - now < 60 s` and on a 401 (one retry), writing the new
triple back to the keyring. The refresh round trip is raced against
the turn's cancel token and capped by a 30 s client timeout, so a
stalled OAuth endpoint can't hang an in-flight turn (Esc aborts
immediately; otherwise the timeout surfaces a transport error). A
failed refresh drops the keyring entry, fires `coder:disconnected`,
and the panel returns to the sign-in empty state; sessions and the
bucket pointer stay.

### Disconnect

Explicit "Disconnect" (confirmed by a modal) drops the keyring entry
and clears in-memory identity. We don't revoke server-side — HF's
revoke endpoint isn't reliably documented for device-flow tokens, so
the confirm dialog links the user's HF settings instead. Mid-session
token rejection follows the same path plus a toast.

## Providers

### Day-1: HF Inference Providers (only)

One HTTP client, OpenAI-compatible schema, against
`https://router.huggingface.co/v1` (`POST /chat/completions`,
streaming SSE, bearer token). The model field carries HF's
provider-routing slug verbatim (`Qwen/Qwen3.5-397B-A17B:scaleway`);
the IDE doesn't parse it.

#### Default models

Hardcoded in `crates/moon-coder/src/defaults.rs`:

- **`standard`** → `Qwen/Qwen3.5-397B-A17B:scaleway` — the agent
  loop and every sub-agent.
- **`cheap`** → `Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway` —
  everything that doesn't need tool calls: session titles,
  branch/commit suggesters, compaction summaries, folder summaries.

These are fallbacks; the user picks freely at runtime.

### Model picker

A cog in the panel header opens a popover with:

- **Standard model** / **Cheap model** — wire model ids sent verbatim
  to the router, click-to-fill from the live `/v1/models` catalog.
  The Standard list is pre-filtered to tool-capable models.
- **Default provider** — UI hint used to auto-suffix the picked model
  with `:provider`; only applied when the catalog row actually has
  that route.
- **Bill to** — sent as `X-HF-Bill-To` on every inference request;
  sourced from `identity.orgs`.

Picks live in `AppState.coder.*` and hot-swap into the runner's
`CoderModels` snapshot on save; mid-turn changes apply to the next
round-trip. Per-session model in the JSONL header is informational
metadata only — the runner reads the active pick from `CoderModels`.

Why HF as primary: the team's home turf, one auth flow, billing via
the user's HF account, and provider-routed access to everything the
team uses. Other providers are additive.

### Custom OpenAI-compatible endpoints (OpenRouter, Anthropic, local)

`AppState.coder.providers` is a `Vec<CoderProviderConfig>`:

```rust
struct CoderProviderConfig {
    id: String,           // opaque "prov-<unix-ms>-<rand>"
    label: String,
    kind: ProviderKind,   // custom | open_router | anthropic
    base_url: String,     // OpenAI-compat /v1 root, or API host for anthropic
    standard_model: String,
    cheap_model: String,
    has_api_key: bool,    // server-set off the keyring
}
```

The HF route stays implicit (`active_provider: None`). Switching to a
user provider sets `active_provider = Some(id)`; `X-HF-Bill-To` is
suppressed off the wire when one is active. API keys live in the
keyring (`account=coder-provider:<id>`) and never round-trip through
the model-settings read. The 401-refresh behaviour stays HF-only — a
user provider that 401s surfaces the error.

`kind` discriminates the wire shape:

- `custom` — free-form OpenAI-compatible endpoint (vLLM, Ollama,
  llama.cpp, LiteLLM, …).
- `open_router` — same wire path as `custom`, but the picker knows
  the URL preset / key dashboard, and Anthropic prompt-cache markers
  fire on `anthropic/*` slugs.
- `anthropic` — Anthropic native (`/v1/messages`) via
  `crates/moon-coder/src/anthropic.rs`: different auth headers,
  system prompt as a top-level field, `tool_use` / `tool_result`
  content blocks, its own SSE grammar, and adjacent same-role
  messages merged (the API rejects consecutive same-role turns).

For the built-in presets the picker locks `base_url` and requires a
`standard_model` pick before saving (there's no per-preset hardcoded
default). A blank `cheap_model` falls back to `standard_model` on the
same provider.

#### Per-workspace provider lock

`active_provider` is global state, so flipping it in one workspace
bleeds into all others — the right default, but some repos want a
pin (e.g. one that depends on Anthropic prompt-cache quality). The
opt-out is `WorkspaceSession::coder_provider_lock` (`Hf` or
`User { id }`) in the workspace's `session.json`; the runner's
effective provider is the lock when set, else the global pick. The
picker shows the effective value with a "Locked to X" label. A stale
pin falls back to HF with a `tracing::warn!`.

#### Extended / adaptive thinking (native Anthropic)

Three contract points the native adapter must honour (details in
`anthropic.rs`):

1. **Request thinking** on the modern adaptive models
   (`type: "adaptive"`, `display: "summarized"`, no `budget_tokens`)
   so reasoning actually streams back; send no `thinking` object to
   any other model (Haiku in its cheap role wants none).
2. **Round-trip signed thinking blocks.** The API requires the
   unmodified signed block to be echoed back ahead of `tool_use`
   blocks on tool turns, or the next round-trip 400s. The blocks are
   carried opaque on the assistant message (`thinking_blocks`); only
   the native Anthropic path populates them, so the OpenAI-compat
   wire body is unchanged.
3. **Survive reload.** Signed blocks persist into the session JSONL
   as moon-specific fields on the pi `thinking` content blocks, so a
   session reopened mid-tool-loop replays without a 400.

`max_tokens` is model-aware (32 K for adaptive-thinking models, 8 K
otherwise). Thinking is incompatible with forced tool choice and
`temperature` / `top_k`; we send none of those.

#### Prompt caching (Anthropic, native or via OpenRouter)

Anthropic prompt caching is opt-in, so the IDE enables it on every
Anthropic-bound request: the native path marks blocks with
`cache_control: {type: "ephemeral"}` directly; the OpenRouter path
flips only the marked messages onto the blocks-array shape (other
providers see a byte-identical no-caching wire shape). Two
breakpoints per request: end of the system prompt (the big static
piece) and end of the last non-assistant message (so the next
round-trip's prefix lookup hits everything stable). 5-minute TTL —
the 1-hour tier costs 5× and buys nothing for an interactive loop.

Cache usage comes back on the streaming `usage` chunk and flows
through `TokenUsage` → the `ContextRing` tooltip. Anthropic's native
`input_tokens` excludes the cached portion, so the adapter rolls
cache reads/writes back into `prompt_tokens` before the runner sees
it — the ring denominator and compaction trigger need the full input.

### Later: Anthropic OAuth (Claude Pro / Max)

A second OAuth flow in its own keyring slot, when somebody wants
subscription billing. Not in the initial sub-phases.

## Tool surface

Every tool is a moon-core method dispatched through the active
`WorkspaceHost`. The schema is JSON-Schema in the LLM request; the
implementations are typed Rust:

| Tool         | Signature                                                                                                           | Notes                                                                                                                                                                                                                                                                        |
| ------------ | ------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `read_file`  | `(path, start_line?, end_line?) -> { content, start_line, end_line, total_lines, truncated, mtime_ms }`             | Lines prefixed `<line_no>\|<line>`; 1-based inclusive range, clamped to EOF with the effective range echoed back. Path routing per [§ Path resolution](#path-resolution-and-cross-folder-routing).                                                                           |
| `write_file` | `(path, content) -> { path, bytes_written, mtime_ms }`                                                              | Creates missing parent directories automatically (`mkdir -p`), sandboxed to the workspace root.                                                                                                                                                                              |
| `edit_file`  | `(path, find, replace, occurrence?) -> { path, bytes_written, mtime_ms, occurrence, total_matches, match_mode }`    | Exact match first, then escalating fuzzy stages (escape-leakage, backslash-run, indent-tolerant, whitespace-collapsing); `match_mode` reports which one hit. Non-unique match without `occurrence` errors with the matching line numbers.                                    |
| `list_dir`   | `(path) -> DirEntry[]`                                                                                              | Gitignore-aware, same walk as the file tree.                                                                                                                                                                                                                                 |
| `grep`       | `(pattern, case_sensitive?, max_matches?) -> { pattern, matches, count, truncated }`                                | `path:line: text` hits; long lines capped at 500 chars so one minified-bundle hit can't blow the context. Backed by the `ignore`/ripgrep dep.                                                                                                                                |
| `bash`       | `(cmd, timeout_ms?) -> { cmd, target, stdout, stderr, exit_code }`                                                  | `docker exec … bash -c` when the workspace shell container is `Running`, else host `bash -lc` — same probe terminals and LSP use, with a per-session force-host override ([ADR 0022](decisions/0022-coder-host-mode-override.md)). `target` echoes `"host"` / `"container"`. |
| `task`       | `(task, folder?, mode?, system_prompt?) -> { result, sub_session_id, tokens_used_estimate, mode, iterations_used }` | Delegates to a sub-agent — see [§ Sub-agents](#sub-agents). Parent-only; up to 4 run in parallel.                                                                                                                                                                            |
| `web_search` | `(query, max_results?) -> { query, results, count }`                                                                | Tavily SERP. Only advertised when a key is configured. See [§ Web search](#web-search).                                                                                                                                                                                      |
| `web_fetch`  | `(url) -> { url, markdown, truncated, bytes }`                                                                      | Jina Reader markdown extraction; `http`/`https` only, 200 kB cap. Always available.                                                                                                                                                                                          |
| `ask_user`   | `(questions[…]) -> { status, answers? }`                                                                            | Pause the turn for multiple-choice questions. Parent-only. See [§ Ask user tool](#ask-user-tool).                                                                                                                                                                            |
| `todo_write` | `(todos[], merge?) -> current full list`                                                                            | Session-scoped plan. See [§ Todo list tool](#todo-list-tool).                                                                                                                                                                                                                |

Possible later additions, as separate commits when proven needed:
LSP wrappers (`goto_definition`, `find_references`, `hover`),
read-only git wrappers, `apply_diff`, `editor_open`.

### Manual re-apply (recovery)

`write_file` / `edit_file` rows expose a tucked-away "re-apply to
disk" affordance: a cog inside the **expanded** tool body opening a
one-item menu (the extra click guards against an accidental
`write_file` clobber). It re-dispatches the recorded call's args
through the registry against the current file — the recovery hatch
for "I reset / clobbered that file and want the agent's edit back"
without re-running the turn. Scoped to the two file-writing tools;
re-running `bash` / read / network calls out of band has no
recovery value. Pure side-effect: nothing is appended to the
transcript or JSONL, and the same turn-end format-on-save pass
runs so the bytes match the original turn. A dispatch failure
(e.g. an `edit_file` whose `find` no longer matches) surfaces as a
flash. The call resolves against the active folder's **visible**
session, so the lookup is by `tool_call_id` within that
transcript.

### Todo list tool

`todo_write` keeps a small in-context plan, same shape as the
Cursor / pi-mono convention (`todos: [{ id, content, status }]`,
`merge: bool`, status vocabulary
`pending | in_progress | completed | cancelled`).

Semantics:

- **Session-scoped.** Each call persists the full post-merge list as
  a `TodosUpdate` record; replay seeds from the last one. Reset on
  new session; survives compaction (the plan is orthogonal to the
  folded prefix).
- The tool result is the current full list, mirrored to the
  frontend's per-folder `coder.todos` bucket.
- `merge: true` updates by id (unknown ids appended, unmentioned ids
  untouched); `merge: false` replaces wholesale; `todos: []` clears.
- "One item `in_progress` at a time" is prompt-enforced, not
  tool-enforced.
- Available to sub-agents too, as their own separate scratchpads.

UI: a `done / total` pill in the panel header (parent list only)
expanding to a popover, plus a per-call transcript body so the plan's
evolution is visible. The agent owns the list — the user can't tick
items by hand.

### Ask user tool

`ask_user` pauses the turn for one or more multiple-choice questions
and blocks until the user answers. Parent-only (sub-agents have no
panel to answer through). Args (`questions[]`, at least one):

| Field            | Type              | Notes                                                                  |
| ---------------- | ----------------- | ---------------------------------------------------------------------- |
| `id`             | `string`          | Stable per-question id; answers key back to it.                        |
| `question`       | `string`          | Question text.                                                         |
| `options`        | `[{ id, label }]` | At least 2 per question.                                               |
| `allow_multiple` | `bool`            | Multi-select with confirm when `true`; single-select submits on click. |

A custom free-form answer is always available — there is no
`allow_other` flag to opt into.

Flow: the loop emits a normal `tool_call` (the panel renders an
always-open interactive card instead of a collapsed row) and parks a
oneshot on the session's `PromptRegistry`, keyed by `tool_call_id`.
The user either answers the card (`coder_respond_to_prompt` →
`{ status: "answered", answers: […] }`) or skips by sending a normal
composer message (`{ status: "skipped" }` — the typed message
proceeds as a steer). Aborting the turn resolves the tool as
`Aborted` like any interrupted tool.

A parked prompt is a background turn the moment the user navigates
away; the answer routes to whichever runtime holds the prompt, and
reopening the session re-renders the card in its waiting state
rather than painting it errored. The session list and folder bar
show a "needs input" cue that takes precedence over "running".

Prompt guidance: not for clarification the agent could resolve by
reading files, and not as a "should I proceed?" confirmation — only
for genuine forks where the user's intent is the missing input.

### Web search

Two tools, both pure outbound HTTP from the IDE process (no
`WorkspaceHost` involved):

- **`web_search`** via Tavily — clean JSON shape, free tier covers
  interactive-editor usage, and it's the SERP API agent-trained
  models already know. Per-user key in the keyring
  (`account=coder-web-search:tavily`), set/cleared in the
  model-settings popover; the tool is only advertised when a key is
  configured.
- **`web_fetch`** via Jina Reader (`https://r.jina.ai/<url>`) — one
  `reqwest::get`, zero deps, good extraction quality. No key needed.
  200 kB cap with `truncated: true` so the model fetches a narrower
  sub-page instead of re-fetching.

Two tools rather than a "search and synthesise" black box: the agent
decides whether snippets suffice or a full read is worth the tokens,
and auto-compaction already handles the context-growth failure mode.
Neither tool is mode-gated — both are read-only against the world.
Errors surface as `is_error: true` tool results with the provider's
verbatim error body.

### Error model

Tools **throw** on failure (`Result::Err` becomes `isError: true` in
the tool result). Returning "ERROR: …" as content is banned — it
confuses the model (the
[explicit pi convention](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md#error-handling)).

### Permissions

No popups. The user's safety boundary is the workspace container
(Phase 2), not a per-call confirm dialog; the panel header shows a
"running on host" / "running in container" indicator so the boundary
is visible. If we ever add an explicitly destructive built-in (none
today), it gets a one-time confirm — but `bash` itself is not gated.

### What the LLM sees as system prompt

Concatenated, in this order:

1. Hardcoded base prompt (workspace-aware).
2. **`AGENTS.md`** from the active workspace root (case-insensitive,
   `CLAUDE.md` fallback, AGENTS.md wins when both exist). Verbatim up
   to a 20 KB cap with a `… (truncated)` sentinel.
3. **Skills** discovered from `skills/`, `.claude/skills/`,
   `.cursor/skills*/`, `.agents/skills/` (`SKILL.md` with `name` +
   `description` frontmatter per the
   [agent-skills standard](https://agentskills.io)). Only the
   descriptions are listed; bodies load via `read_file` on demand.

No skill installer — skills are file conventions.

## Sessions

### Multi-session per project

Every bound folder can host multiple concurrently-running sessions
([ADR 0016](decisions/0016-coder-concurrent-sessions.md)). Each
session has its own chat history, cancel handle, composer draft, and
context ring; switching folders or sessions never touches another
session's running turn.

Tools captured by a running turn close over the **session's bound
folder**, not the live active folder — switching folders mid-turn
cannot redirect tool calls. `abort` targets the active folder's
visible session only; sign-out is the one global cancel.

`coder:event` payloads are wrapped in a
`CoderEventEnvelope { folder, session_id, event }` so the frontend
routes updates to the right per-`(folder, session)` bucket.
Sub-agent events carry the **parent's** coordinates. A few
folder-scoped variants (`folder_summary_ready`, `hub_sync_*`) arrive
with an empty `session_id`.

`AppState.coder.last_session_by_folder` records the last-visible
session per project and is restored when the panel first mounts a
folder. Hydration is gated on a workspace-ready signal so it doesn't
race the launch-time tab-restore loop's active-folder switching. A
stale pointer (JSONL deleted out-of-band) falls through to the
sessions list and self-heals on the next open/send. Background turns
don't survive a process restart.

The session list paints per-row status: **needs input** (parked on
`ask_user`; takes precedence over running), **running** (busy), and
**finished** (turn ended while the user wasn't following; cleared on
open). Folder-bar glyphs mirror the same three states as rollups.
Reopening a still-running session preserves these states: the replay
batch's trailing `TurnComplete` terminator clears the pip, so `Replay`
carries an `in_flight` flag and the frontend re-asserts **running** /
**needs input** after applying the batch.

### On disk

Append-only JSONL at
`<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/<session-id>.jsonl`.
The slug is `<basename>-<8-char FNV-1a hex>` of the folder's absolute
path — deterministic, collision-free across same-basename folders.

The first line is a header; every subsequent line is one record in
**[pi](https://pi.dev)'s session-log wire shape**, so the file
uploads to a HF dataset and renders in the Hub's pi trace viewer.
The in-memory enum is `SessionRecord`; conversion happens at the
serialise/deserialise boundary in `sessions.rs`.

```jsonl
{"type":"session","version":3,"id":"sess-...","timestamp":"2026-05-18T12:14:33.421Z","cwd":"/workspace/moon-ide","title":"implement bucket sync","created_at_ms":1746440000123,"updated_at_ms":1746440045871,"model":"anthropic/claude-sonnet-4.5"}
{"type":"message","timestamp":"2026-05-18T12:14:34.001Z","message":{"role":"user","content":"do the thing","timestamp":1746440074001}}
{"type":"message","timestamp":"2026-05-18T12:14:38.220Z","message":{"role":"assistant","content":[{"type":"text","text":"sure…"},{"type":"toolCall","id":"call_1","name":"read_file","arguments":{"path":"…"}}],"provider":"anthropic","model":"claude-sonnet-4.5","stopReason":"toolUse","usage":{"input":1234,"output":56,"totalTokens":1290},"timestamp":1746440078220}}
{"type":"message","timestamp":"2026-05-18T12:14:38.310Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"read_file","content":[{"type":"text","text":"…"}],"isError":false,"timestamp":1746440078310}}
{"type":"compaction","timestamp":"2026-05-18T12:20:01.000Z","summary":"earlier turns: …","details":{"messages_compacted":42,"messages_kept":6}}
{"type":"message","timestamp":"2026-05-18T12:14:45.000Z","message":{"role":"custom","customType":"moon_title_update","display":false,"details":{"title":"add bucket sync upload task"},"timestamp":1746440085000}}
```

Schema `3` tracks pi's current format for the fields we use, **minus
its tree structure** — Moon sessions are linear (no in-place
branching), so pi's `id` / `parentId` entry linking would be dead
weight and we omit it. What we do carry from pi v3:

- A `timestamp` on every body line: ISO-8601 on the entry envelope
  (pi's `SessionEntryBase.timestamp`) and Unix-ms inside each
  message (pi's per-message `timestamp`). Both come from the moment
  the row is flushed in `append_record`. Records are not stamped
  in-memory, so the rare revert/rewrite path re-stamps surviving
  rows with the rewrite instant rather than preserving the
  originals. The `UserMessage` / `AssistantMessageEnd` events carry
  the same time as `created_at_ms` (live `now`, persisted on
  replay), and the panel reveals it as a wall-clock label next to
  the `you` / `coder` header on row hover (full date in the
  `title`).
- `stopReason` on assistant rows (`stop` | `length` | `toolUse` |
  `error` | `aborted`), normalised from the provider's finish/stop
  reason by `inference::normalize_stop_reason`.
- `toolName` on tool-result rows, so the viewer labels results
  without cross-referencing the assistant record.

Mapping notes (details live in `sessions.rs`):

- `User` / `Assistant` / `Tool` / `Compaction` map natively to pi
  rows. Fully-empty assistant turns (no text, no thinking, no tool
  calls) are not persisted and are dropped on load — re-inflating one
  trips Anthropic's non-whitespace-text 400.
- The per-assistant `provider` + `model` stamp reflects the route
  that actually served that round-trip, not the header's seed.
- `Usage` folds onto the prior assistant line's `usage` block on
  write and is re-emitted as a stand-alone record on load.
- Moon-specific records (`TitleUpdate`, `TodosUpdate`,
  `SubagentSpawned`, `SubagentFinished`, `Error`) ride in pi `custom`
  rows with `display:false` and a `moon_*` `customType`; the pi viewer
  skips them silently. `Error` is appended when a turn fails with a
  non-recoverable backend error (auth, decode, provider 400); without
  it the on-disk transcript trails off mid-tool-loop and the failure
  is invisible to anyone debugging from the JSONL after the fact. It
  doesn't shape the in-memory `messages` slice on reload — an error
  ended the turn, so it isn't history the next turn sends.
- Image attachments split the data-URL into pi's `data` + `mimeType`
  on write and re-prefix on load.

The header carries pi-required keys plus our extras; pi's schemas
tolerate unknown keys. The system prompt isn't persisted — reopening
re-adds the current default, so prompt updates apply retroactively.
`updated_at_ms` is frozen at first persistence; recency for the
sessions list comes from the file's mtime (free — every append
touches it — and it keeps appends O(1) and crash-safe).

Sessions deliberately don't live in the project tree: they're
personal history, not project artefacts, and putting them under VCS
would litter `git status` or force a blind `.gitignore` entry.

Lazy persistence: an empty session has no file on disk; the header is
written on the first record. A crash loses at most the in-flight
event.

### Interrupted tool calls (orphan recovery)

If a turn stops mid-tool (Esc, panel close, quit), the assistant's
`tool_calls` hit disk but the matching tool record didn't — and every
chat-completions API 400s on an unpaired `tool_calls`. Two recovery
points:

- **At abort time** (same process): the runner appends a synthetic
  `Tool` message carrying an "interrupted" payload for each orphan,
  persists it, and emits an errored `tool_result` so the panel flips
  the row.
- **At reload time**: `open_session` synthesises the same in-memory
  tool messages and errored events for any orphan it finds, but
  doesn't mutate the JSONL — load is read-only; the abort-time path
  is the canonical writer. Idempotent when abort-time recovery
  already ran.

Replay ships as **one batched `Replay` event**, not
one-emit-per-record — Tauri dispatch overhead made a 1000-row session
take seconds to open otherwise. Live turns stay one-event-per-emit.

### Revert, replay, and edit & resend

Every user message row reveals three hover affordances:

- **Replay from here** — drops that message and everything after it,
  then re-sends the same prompt verbatim (re-run the turn without a
  composer round-trip). No confirm (the same prompt fires again, so
  nothing is lost).
- **Edit & resend** — same truncation, but the dropped prompt loads
  back into the composer for the user to tweak before sending. No
  confirm (the text isn't lost).
- **Revert to here** — drops that message and everything after it
  from disk and memory, full stop. Modal-confirmed (it permanently
  rewrites the transcript with no re-send).

Revert and edit-and-resend route through
`coder_revert_to_message(user_ordinal)`; replay routes through
`coder_replay_from_message(user_ordinal)`, which is the same
truncation immediately followed by a `send` of the dropped prompt
(it auth-gates **before** the truncation, so a signed-out replay
fails clean without rewriting the JSONL). The anchor is the 0-based
ordinal among the transcript's user records, because row ids are
minted fresh on every replay and aren't reload-stable. Queued
(undrained) steers are skipped by the ordinal count since they
aren't on disk yet. All three are refused while a turn is in flight.
After truncation the backend drops the mounted runtime and re-opens
the session so the trimmed transcript repaints through the normal
replay path.

### Resume from a mid-turn agent response

A mid-turn assistant row (one whose `tool_calls` are non-empty and
are followed by `Tool` records before the next user message) reveals
a fourth hover affordance: **Replay from here**. This is a different
operation from the user-message replay above — it resumes the
tool-loop from that checkpoint rather than re-sending a prompt.

Routes through
`coder_resume_from_assistant(assistant_ordinal)`, where the anchor is
the 0-based ordinal among assistant records that have non-empty
`tool_calls`. The backend truncates the JSONL to keep everything up
to **and including** the target `Assistant` record, drops its `Tool`
records (and everything after), re-opens the session so the trimmed
transcript repaints, then re-dispatches the kept `Assistant`'s
`tool_calls` against the current workspace via the normal
`dispatch_tool_calls` path. The model is **not** re-prompted for that
round-trip — its existing tool calls execute fresh against current
workspace state, and the turn loop continues with the new results in
context. Auth-gates before the truncation (same posture as
`coder_replay_from_message`). Refused while a turn is in flight. No
confirm (tool calls re-execute fresh, nothing is lost). After the
re-dispatch, subsequent iterations make normal LLM calls with the
fresh tool results in `messages`.

### Auto-rename

After the first turn of a fresh session finishes — successfully,
aborted, or errored — a one-shot cheap-model call produces a 4-6 word
title, replacing the truncated-prompt fallback in memory, persisting
as a `TitleUpdate` record, and emitting `session_title_updated`.
"Any outcome" matters because long tool-heavy first turns are
routinely Esc'd; failures keep the truncated-prompt fallback. Runs
once per session.

### Sidebar UI

Two views sharing the right-side slot (`rightPanel.kind === 'coder'`):

- **Session view** — transcript + composer, with a sticky
  `← Sessions | <title> | +` strip. Default view when a session
  exists.
- **Sessions list** — a row per persisted session (title + relative
  time), with hover affordances for "open trace" and delete
  (confirmed), plus the per-row status cues described above.

`+` from either view opens a fresh empty session with focus in the
composer; empty sessions don't persist until the first message.

### Open the raw trace in the editor

Session rows, the session header, and the sub-agent pop-out all
expose a `</>` button that resolves the JSONL's absolute host path
(`coder_session_jsonl_path`) and opens it via the same host-direct
file mechanism `Ctrl+O` uses (test plan 0051) — the trace always
lives on the host's `XDG_DATA_HOME`, even for containerized
projects. The tab is editable (power-user inspection; a corrupted
line just gets skipped with a warn on next load) and doesn't
auto-tail. Empty sessions surface a toast instead of a phantom tab.

### Composer attachments

- `Ctrl+L` attaches the active editor selection (also works from
  diff panes and review sections): inserts an inline
  `@path:start-end` token at the caret, adds a chip above the
  textarea, opens the panel, focuses the composer. Chips dedupe by
  `(path, range)`; every press still inserts a fresh inline token.
- Chips and inline tokens stay in sync both ways: removing a chip
  strips its tokens; editing a token out of the prose drops the
  chip. Tokens delete as one atomic block on Backspace/Delete.
- Typing `@` opens an inline file picker backed by the same search
  the command palette uses. Picking inserts `@path` — **pointer
  only, no contents attached**: the model calls `read_file` if it
  needs the bytes, which keeps prompts bounded on big picks.
  `Ctrl+L` on a selection is the one "send me the bytes" gesture.

#### Wire shape (matches Cursor)

Prose stays intact with `@`-tokens inline; captured snippet contents
land in a trailing `<context>` block:

```
explain the difference between @src/lib/foo.ts:48-50 and
@src/lib/foo.ts:63-67

<context>
<code_selection path="src/lib/foo.ts" lines="48-50">
[lines 48-50 verbatim]
</code_selection>
<code_selection path="src/lib/foo.ts" lines="63-67">
[lines 63-67 verbatim]
</code_selection>
</context>
```

This is the wire shape Cursor ships, which models have seen in
training. The wrapper element is a sufficient delimiter — no fencing
needed. Empty draft + attachments is a valid send. The snapshot is
captured at attach time — later file edits don't change what the
agent sees. No implicit "active file" hint ships with a turn.

### Image attachments

Pasting images into the composer attaches them as thumbnail chips
(cap: 4 MB per image, 10 per send). On send they ship as a separate
`images` argument and go on the wire as OpenAI-compatible
`image_url` data-URL blocks. Persisted on the `User` record
(`skip_serializing_if` empty, so old transcripts keep their shape)
and replayed as clickable thumbnails.

### Compaction

See [§ Token accounting and auto-compaction](#token-accounting-and-auto-compaction).

## Bucket sync (HF buckets)

Buckets are HF Hub's S3-like object storage backed by Xet
([guide](https://huggingface.co/docs/huggingface_hub/guides/buckets)).
moon-coder uses one bucket per workspace, owned by the user or one of
their orgs, so traces render in the Hub's
[pi trace viewer](https://huggingface.co/docs/hub/en/storage-bucket-trace-viewer).

### Connect flow

A workspace starts unbound. The panel header's cloud-sync button
opens the settings modal; when unbound, a "Connect" CTA opens the
connect modal: **Owner** (user namespace or org), **Name** (defaults
to `<workspace-basename>-traces`, validated against Hub repo-name
rules), **Visibility** (private default). Create POSTs
`/api/buckets/<owner>/<name>`, writes a README, and persists the
binding onto `WorkspaceSession.coder_hub_bucket`. `409 Conflict` is
treated as success (adopt the existing bucket; `contribute-repos`
403s later if we don't actually own it).

After create, `autosync` defaults to **false** — connecting alone
never pushes anything. The header icon tints accent when bound.

### On-Hub layout

```text
<owner>/<name>
├── README.md                            ← generated once at create time
├── moon-ide-7e985eb6/                   ← one directory per bound folder
│   ├── sess-1779.....-abc.jsonl
│   └── sub-1779....-x.jsonl             ← sub-agents share their folder's directory
└── moon-landing-fa837b35/
	└── sess-1781.....-ghi.jsonl
```

The directory key is the same `<basename>-<fnv8>` slug used locally,
so local ↔ Hub paths line up 1:1. The bucket holds nothing else —
no manifests, no extra nesting. The README is a short generated
stub; the trace viewer keys off blob path + `.jsonl`.

### Sync paths

Two entry points, one implementation:

- **Manual** — a cloud-up icon per session row pushes that session
  immediately, regardless of `autosync`.
- **Autosync** — the runner's `TurnComplete` handler enqueues a sync
  with a 2 s debounce per `(workspace_id, session_id)`,
  fire-and-forget.

Upload: skip if the recorded uploaded byte-length matches the file
(nothing new); otherwise fetch a short-lived Xet write token, push
the bytes through `hf-xet`, bind the resulting hash at
`<folder-slug>/<id>.jsonl` via the batch API, and persist the new
`(bytes, at_ms)`. `HubSyncStarted` / `HubSyncFinished` events drive
the per-row idle / syncing / synced / failed decoration.

### Failure mode

A failed upload is a `tracing::warn!` plus a red cloud icon with the
error in its tooltip. Local JSONL stays the source of truth; the next
sync retries from scratch. No status-bar pip — a Hub blip shouldn't
drag the workspace status red.

### Disconnect

Clears `WorkspaceSession.coder_hub_bucket` (including the uploaded
cache) but never deletes the bucket — that's a web-UI action.
Reconnecting to the same bucket re-uploads everything once; Xet dedup
makes the bytes nearly free.

### What never leaves the host

- HF access / refresh tokens and provider API keys (keyring only).
- File contents the agent reads **do** end up in the JSONL and
  therefore in the bucket once a sync lands. NDA workspaces should
  leave autosync off (the default) and not click Upload on sensitive
  sessions.

## Token accounting and auto-compaction

### Token usage report

The runner emits `CoderEvent::TokenUsage` three times per
round-trip: pre-call (estimate anchored on the previous turn's exact
usage plus bytes/4 of what was appended since), mid-stream (throttled
estimate so long generations visibly fill the ring), and post-call
(exact provider figures off the streaming `usage` chunk, requested
via `stream_options: { include_usage: true }`).

```ts
{
  kind: 'token_usage',
  prompt_tokens: number,
  completion_tokens: number,
  total_tokens: number,
  context_window: number,
  source: 'provider' | 'estimate'   // estimate = bytes/4 fallback, tooltip shows ≈
}
```

`context_window` is sourced from each provider's models catalog,
cached in `CoderModels::context_windows` (primed at startup and
refreshed by picker fetches), falling back to a small static table
and finally 128k with a warn. The picker also exposes per-slug
**user caps** (`context_window_overrides`) that clamp the catalog
value — for models that advertise 1M but degrade past ~250k, capping
arms compaction earlier and keeps the ring honest.

The `ContextRing` in the panel header fills to
`prompt_tokens / context_window`: muted < 60 %, warning < 80 %,
danger ≥ 80 %; pulses during compaction.

### Auto-compaction

When `prompt_tokens / context_window ≥ 0.80`
(`COMPACT_THRESHOLD`), the next loop iteration compacts before
sending:

1. Keep the most recent 6 user turns (`RECENT_USER_TURNS_KEPT`); the
   oldest of those is the cut point.
2. Summarise the prefix with the **standard model** (intent,
   decisions, files touched, state, next steps). The prefix is
   **chunked** so no single summary call exceeds the standard
   model's own window — chunked summaries are merged with a final
   pass, recursing if needed. Per-chunk failures are tolerated; the
   pass only gives up if every call fails. (The standard model is
   used, not the cheap model, because the cheap model's actual
   per-route context window can be smaller than the standard
   model's — the catalog's max-across-providers lookup overestimates
   the cheap model's limit, which 400'd the summary call every
   turn and made compaction a silent no-op.)
3. Replace the prefix with one synthetic system message carrying the
   summary at `messages[1]` (the composed system prompt at
   `messages[0]` is recomposed fresh every turn anyway).

The on-disk JSONL is **not** rewritten — full history stays for
pop-out / audit. Instead a
`Compaction { summary, messages_compacted, messages_kept }` record is
appended so replay reaches the same compacted shape;
`messages_kept` is load-bearing (folding the whole prefix on reload
would drop the retained recent turns). After a fold the runner
re-anchors `last_usage` on an estimate of the compacted prompt so
the guard stays armed and re-fires if one pass didn't get under
threshold.

`compaction_started` / `compaction_complete` events render as an
interleaved transcript row — a "compacting…" pip flipping to a
`<details>` with the summary — which scrolls away naturally,
stacks on repeat compactions, and survives reopen via replay. If
every summary call fails, compaction is skipped and the agent
continues with the uncompacted prompt.

Sub-agents run the same pass at the same threshold against their own
history. Threshold, retained-turn count, and the summary prompt are
hardcoded ("hardcode first, configure later").

One edge case: if the retained recent turns alone exceed the window
(a burst of huge tool results), compaction mitigates but can't get
under the limit. When the only thing in the compactable prefix is a
prior compaction summary (all `System` messages — happens when the
kept turns contain exactly `RECENT_USER_TURNS_KEPT` user messages),
the pass bails instead of re-summarising the summary: re-summarising
a summary produces a same-size replacement, never gets under
threshold, and would spin the loop forever wasting one LLM
round-trip per iteration. Rare and self-limiting.

### Iteration cap and final wrap-up

The parent loop caps at `MAX_TURN_ITERATIONS` = 200 tool-call
round-trips per prompt. Hitting the cap appends a sentinel user
message and runs one final tools-disabled round-trip so the model
writes its best answer from what it gathered; the sentinel persists
and renders like any user message. Per-tool errors are unrelated —
they flow back as `is_error: true` results the model can recover
from.

## Sub-agents

The parent's loop exposes the `task` tool. One call dispatches one
sub-agent; the parent's tool call awaits the report. Multiple `task`
calls in one assistant message run concurrently (4-permit
semaphore). The wire name is `task` (what every agent product calls
this); Rust internals keep the `subagent` naming.

```jsonschema
task(
  task: string,                    // self-contained; sub-agent doesn't see the parent's transcript
  folder?: string,                 // basename of a bound folder, default = parent's active folder
  mode?: "research" | "agent",     // default = "agent"
  system_prompt?: string           // overrides the mode-default prompt
) -> {
  result: string,                  // the only string the parent's model sees
  sub_session_id: string,          // pop-out lookup key, stable across restarts
  tokens_used_estimate: number,
  mode: "research" | "agent",
  iterations_used: number
}
```

There is deliberately no per-call model selector — sub-agents inherit
the parent's standard model. (A `fast | large` selector existed and
was dropped: it biased the parent against delegating non-trivial
work.)

### Modes

- **`research`** — read-only intent: `read_file`, `list_dir`,
  `grep`, `bash`. The shell stays available for inspection commands
  (`git log`, `cargo check`, …); "no mutation via bash" is
  prompt-enforced, while `write_file` / `edit_file` are blocked at
  the dispatch boundary regardless.
- **`agent`** — full toolkit, same capabilities as the parent.
  (Renamed from `coder`: the parent model treated a `coder`
  sub-agent as less capable and hesitated to delegate; `agent`
  reads as "another instance of you".)

Top-level sessions are always `agent`; mode is a sub-agent concept.

### Folder targeting

Sub-agents target one already-bound folder (basename, or absolute
path as fallback); unbound targets error so the model can recover.
Since the parent's own fs tools reach any bound folder (see below),
sub-agents are for **delegation**, not access: context isolation
(read 30 files, report a paragraph), parallelism, or scoped
self-contained work.

### Freshness pre-fetch

Sub-agent spawn kicks off a best-effort, fire-and-forget `git fetch`
against the target folder's `WorkspaceHost`, throttled to one per
folder per 5 minutes. Rationale: the active folder has a periodic
auto-fetch (test plan 0064) but a sibling may be stale, and
review-style sub-agent prompts read `origin/main`. Fetch-only —
never pull/merge, so no working-tree mutation; failures are
swallowed quietly.

### Budget

Sub-agents share the parent's `MAX_TURN_ITERATIONS` cap (a tighter
50-iteration cap existed and bailed real refactors mid-flight).
Hitting the cap triggers the same tools-disabled final wrap-up,
prefixed so the parent knows the budget ran out. The old byte-budget
cap was removed when auto-compaction shipped.

### Persistence

Sub-agent JSONLs live at
`<sessions-dir>/<parent-folder-slug>/<parent-session-id>/<sub-id>.jsonl`
— keyed by the **parent's** folder (sub-agents belong to the project
that originated them), grouped per parent session, created lazily,
and deleted with the parent. `list_sessions` reads the slug dir flat
so only top-level sessions land in the picker; "open trace" routes
`sub-...` ids through a scan of parent subdirs. The header carries
`parent_session_id` + `parent_tool_call_id` + `subagent_mode` (and
`subagent_target_folder` when it differs from the parent's).

### UI

Sub-agents render as collapsed cards under the parent's `task` row:
folder, mode badge, status pip, result preview, token footer. Click
pops out a dedicated transcript view (same row components, different
rows source) with a back arrow. `SubagentSpawned` / `SubagentFinished`
records on the parent's JSONL rebuild the cards on reload, and the
sub-agent's own JSONL replays into the pop-out; a missing sub-agent
file degrades to card-only with a warn.

### Bound folders system-prompt section

The parent's system prompt (rebuilt every turn) lists every bound
folder with the absolute path the model should use — `/workspace/<name>`
when the workspace shell container is `Running`, real host paths
otherwise, probed the same way `bash` routes so the two can't drift —
plus a 2-3 sentence description per folder. Descriptions are
generated by the cheap model from each folder's manifest files
(AGENTS.md first, then README.md, Cargo.toml, package.json,
pyproject.toml), cached under
`<XDG_DATA_HOME>/moon-ide/folder-summaries/` keyed on a content
hash. Generation is detached and never blocks a turn — a missing
summary renders as `(summary still generating)`; a
`folder_summary_ready` event fires when one lands.

### Path resolution and cross-folder routing

`ToolRegistry::resolve_target` classifies each path argument as
`InWorkspace { folder, relative }` (dispatch through that folder's
`WorkspaceHost`) or `OutOfWorkspace { abs_path }`:

1. **Absolute path under a bound folder's root** → that folder
   (longest root wins when folders nest).
2. **Synthetic `/workspace/<name>/...`** → the bound folder with that
   basename; an unbound name errors with the list of bound folders so
   the model can self-correct. Works in host mode too, but the prompt
   only advertises it in container mode.
3. **Bare-basename relative** (`<sibling-name>/foo.rs`) → same
   routing as the synthetic form.
4. **`./<sibling-name>/foo.rs`** → opt-out; resolves inside the
   active folder (for directories that legitimately share a
   sibling's basename).
5. **Absolute path outside every bound root** → `OutOfWorkspace`,
   served by container-aware primitives that mirror `bash`'s target
   (`docker exec` when the container runs, host fs otherwise).
   Arbitrary-path read/write/list is the point — see
   [ADR 0025](decisions/0025-coder-arbitrary-path-fs.md). No
   format-on-save outside a project.
6. **Other relative paths** → active folder, keeping the historical
   `..`-escape rejection.

This routing applies to `read_file`, `list_dir`, `write_file`, and
`edit_file`. `grep` and `bash` stay scoped to the session's folder —
a sub-agent against the target folder is the way to search or run
commands elsewhere. Sub-agents share the registry, so the same rules
apply to them; depth=1 is enforced by omitting `task` from their
tool list.

### Project-bar git status: surgical refresh

Cross-folder edits need to reach the project bar's per-folder
`+N ~N -N` badges, but the fs-watcher only sees the active folder.
The frontend parses each parent `tool_call`'s `path` with the same
routing rule, collects target folders in a debounce window, and
refreshes only those on turn boundaries. Anything ambiguous (`bash`,
`grep`, sub-agent activity, missing path) fans out to refreshing
every bound folder — correctness over cleverness.

## Worktree sessions

A session can opt into running in its own **git worktree** — an
isolated working directory on a fresh branch, sharing the repo's
object store — so several agents work the same project at once
without stomping each other. The deliverable is the branch: each
isolated agent accumulates its work (staged, unstaged, or committed)
on its own branch, and the user turns each into its own commit / PR
through the normal SCM flow. There is no forced merge-back. Opt-in is
**per session** (a "new isolated session" affordance beside the
ordinary `+`); an ordinary session drives the folder's main working
tree as before. Full rationale and rejected alternatives:
[ADR 0028](decisions/0028-coder-worktree-sessions.md).

### The worktree is a bound folder

The worktree directory is registered as a first-class bound workspace
folder, so the file tree, SCM panel, per-folder git-change badges,
diff view, review comments, terminal, and LSP all light up for it
unchanged — it's just another repo root with its own `WorkspaceHost`
and git mutex. A folder `origin` discriminator marks it as a
session-worktree: it renders **nested under its parent** in the
folder bar with a branch glyph, its lifecycle is tied to its owning
session, and it is pruned (not merely unbound) when discarded.

### Ownership vs. routing

The session stays **owned by its parent folder** — its JSONL persists
under the parent's slug and it appears in the parent's session list,
where the user started it. Only **tool routing** diverges: the
session's fs / `bash` / `grep` / git tools run against the worktree's
host. Two optional header fields carry the binding:

```
worktree_root:   Option<String>   // absolute path of the worktree checkout
worktree_branch: Option<String>   // branch the worktree is on
```

At turn time the runner resolves `cx.folder` to
`folder_for_path(worktree_root)` when set, mirroring the
`subagent_target_folder` precedent. The fields elide when `None`
(ordinary sessions keep a byte-identical header); the schema version
bumps to `4`. A worktree session's **sub-agents inherit the routing**
— they default to the parent's active folder, now the worktree — so
all of one agent's parallel work lands on the one branch.

### Location, naming, lifecycle

Worktrees live outside any repo, under the per-workspace state dir at
`<workspaces_dir>/<workspace_id>/worktrees/<parent-slug>/<branch-slug>/`
— keeps the parent's `git status` clean and cleanup to one directory
walk. The directory basename embeds the parent slug so two worktrees
can't collide on the `/workspace/<name>` container mount. The branch
defaults to `moon/agent-<short-id>` at creation (no diff to summarise
yet) and is renameable; an AI suggestion can replace it after the
first turn.

An isolated session can either start a **fresh** `moon/agent-<id>`
branch off the parent's current `HEAD` (the default), or be based on
an **existing** branch — local, or a remote one DWIM-created locally
the way `git switch` does. The latter is how you set an agent working
on a colleague's branch: it's checked out only in the worktree, so the
parent's checkout (and every other agent) is left undisturbed. The
`BranchSwitcher` palette exposes this as a per-row "start isolated
agent" action (local branches and open-PR head refs); the coder
panel's worktree button covers the fresh-branch default.

Created on opt-in (`git worktree add`), re-bound at startup (the
folder's `origin` rides `session.json`), and **pruned** by the
worktree row's `×` — run against the parent repo and guarded by a
re-confirm when the worktree is dirty. Deleting the owning session
keeps the worktree (the branch is the deliverable you may still PR).
The **branch is never deleted by the IDE**: it's left in place for a
later PR.

#### The worktree button is context-aware

- **On the sessions list** it starts a fresh isolated session in a new
  worktree (a new `moon/agent-<id>` branch off `HEAD`).
- **Inside a session** it _moves that session_ into a worktree,
  conversation and all (`coder_move_session_to_worktree`): the header
  gains `worktree_root`, so from the next turn its tools run in the
  worktree, while the session stays in the same per-project list. The
  branch follows the user's intent — on a **non-default** branch the
  worktree checks out _that_ branch and the main tree is reset to the
  default branch (keep the same PR, free the main tree); on the
  **default** branch (or detached) it forks a fresh `moon/agent-<id>`.
  The switch-then-add runs atomically under the git lock and **refuses
  a dirty tree** (commit or stash first) rather than risk carrying
  uncommitted work to the wrong branch. The button disables once the
  session is already in a worktree. So "I want a new isolated session
  while in one" is `+` (new blank session) then the worktree button.

### A session is tied to a branch

Every session carries a branch — the deliverable. A worktree session's
branch is its checkout's; a main-tree session's is whatever `HEAD`
landed on the last time the user committed with it open (a fresh
"commit on new branch" or a plain commit; `committed_branch` on the
header, rewritten in place, most-recent wins, preferred over the
worktree's initial branch once set). A blank, never-committed
non-worktree session has no branch yet.

The session list (and the open session's header) shows that branch as
a chip. Clicking it goes to **wherever the branch currently lives**,
resolved live rather than from which field set it:

- the branch is checked out in a bound worktree → focus that worktree
  folder (the file tree / SCM follow; the per-project session list
  stays put, so nothing about the conversation is lost);
- otherwise → `git switch` the active folder to it (git's own refusal
  on a dirty tree is surfaced as-is).

So after juggling several agents across several branches off `main`,
you return to a past session and land on its branch — or its
worktree — in one click instead of hunting for the name.

A future direction (not built): when you want to revisit a session's
branch but an agent is mid-run on the current one, switching the
shared tree would disturb it — so we may offer to _reopen the old
session in a worktree on its branch_ instead, and possibly lean on
worktrees automatically once multiple agents are running concurrently.

The git primitives (`git_worktree_add` / `_list` / `_remove`)
serialise behind the per-folder git mutex ([ADR 0015](decisions/0015-git-serialisation.md)).
In a containerised workspace, isolated sessions run their tooling
**in the container** so builds use the container toolchain. The
worktree lives **inside the parent repo** at `<parent>/.worktrees/<branch>`
with `git worktree add --relative-paths` ([ADR 0029](decisions/0029-worktrees-inside-parent.md)),
so it rides the parent's bind mount and its relative git links resolve
the same on the host and in the container — no separate mount, no
`git worktree repair`, and host git keeps working when the container is
down. `/.worktrees/` is added to the parent's `.git/info/exclude` so it
never dirties the parent's `git status`. Each worktree is
`git worktree lock`ed (unlocked before removal). This needs git >= 2.48
on the host (worktree creation errors with an "update git" message
otherwise) and in moon-base (built from source); see ADR 0029 for the
`extensions.relativeWorktrees` repo-config caveat.

## UI placement

A right-side panel docked to the editor area. Chat and coder are
mutually exclusive tenants of a single right-side slot — one width,
toggle between them; the mounted surface (or `null`) lives on
`AppState.right_panel` and restores across launches. Toggleable from
the status bar, the command palette (`Coder: Toggle Panel`), and the
`F6` / `Shift+F6` focus rotation.

Top-to-bottom: identity card (when signed in) → session list /
sticky session header → transcript → session controls → composer.
Empty states: "Sign in with Hugging Face" when signed out; identity
card + "+ New session" when signed in with no session.

## Frontend ↔ backend boundary

Tauri commands in `src-tauri/src/commands/coder.rs`:

| Command                                                 | Purpose                                                                                                       |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `coder_start_device_flow()`                             | Returns `{ user_code, verification_uri, expires_in, interval }`; background poll runs in `moon-coder`         |
| `coder_status()`                                        | `{ signed_in, identity?, has_session, sync_enabled }`                                                         |
| `coder_sign_out()`                                      | Drops keyring + identity                                                                                      |
| `coder_list_sessions()`                                 | Per-session summaries for the list view                                                                       |
| `coder_open_session(id?)`                               | Load `id`, or create a new session; returns the active id                                                     |
| `coder_delete_session(id)`                              | Removes JSONL (+ sub-agent subdir)                                                                            |
| `coder_session_jsonl_path(id)`                          | Resolves a session id (parent or sub-agent) to its on-disk path; powers "open trace"                          |
| `coder_send(text, mode)`                                | Routes to the loop (`send` / `steer` / `follow_up`)                                                           |
| `coder_abort()`                                         | Cancels the visible session's in-flight turn                                                                  |
| `coder_respond_to_prompt(call_id, response)`            | Resolves a parked `ask_user` prompt; returns `false` when nothing's parked                                    |
| `coder_revert_to_message(user_ordinal)`                 | Truncates the visible session; returns the dropped prompt for edit-and-resend. Refused mid-turn               |
| `coder_replay_from_message(user_ordinal)`               | Truncates to before the message, then re-sends the same prompt (re-run the turn). Refused mid-turn            |
| `coder_resume_from_assistant(assistant_ordinal)`        | Truncates to the assistant message (kept), re-dispatches its tool calls, continues the turn. Refused mid-turn |
| `coder_rerun_tool_call(tool_call_id)`                   | Reapplies a recorded `write_file` / `edit_file` to disk (recovery); transcript untouched                      |
| `coder_set_model(slug)` / `coder_set_model_settings(…)` | Model picks                                                                                                   |
| `coder_*_provider*` commands                            | Custom-provider CRUD, probe, per-provider catalog, keyring-only API-key set/clear                             |
| `coder_set_sync_enabled(enabled)`                       | Per-workspace bucket-sync toggle                                                                              |

Push events: `coder:event` (every loop event, envelope-wrapped),
`coder:signed_out`, `coder:sync_state`.

## Failure modes

| Scenario                                     | UI behaviour                                                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| User has no HF token                         | "Sign in with Hugging Face" empty state                                                          |
| Device-flow polling expires                  | Modal shows "code expired, retry"                                                                |
| Token rejected by `/whoami-v2` after refresh | Toast + return to empty state; keyring cleared                                                   |
| `router.huggingface.co` 5xx                  | Streaming surfaces an error; "Retry" button performs a `continue()` against the existing context |
| Tool throws                                  | LLM gets `isError: true` + the message; loop continues                                           |
| Bucket creation 4xx (e.g. quota)             | Sync marked delayed; sessions stay local                                                         |
| `hf-xet` upload partial-fail                 | Retry on next sync; per-row icon flips red with error tooltip                                    |
| Network down                                 | Streaming aborts; same retry surface as 5xx                                                      |

## Out of scope (explicitly)

- **Pluggable agent binaries** (ACP) — superseded by ADR 0010.
- **Plan mode** — the team can write plans into `AGENTS.md`.
- **Permission popups** — see "Permissions" above.
- **MCP** — same posture as pi.
- **Per-sub-agent abort UI** — parent abort cascades to all live
  sub-agents; individual cancel buttons wait for a real need.
- **Background detached sub-agents** — sub-agents are
  synchronous-blocking.
- **Depth ≥ 2 sub-sub-agents** — hardcoded depth=1 cap.
- **Skill packages / installable skills** — file conventions only.
- **Bucket browser** ("import session from bucket") — bucket is
  backup-only at first.
- **Multi-account** — one HF account per install.
- **Detached / cross-restart agent runs** — turns already run
  independently of panel visibility, but a process restart kills
  in-flight turns (the runtime map is in-memory). Surviving restarts
  needs an always-on `moon-core` clients attach to — the same shape
  the [companion](companion.md) and remote-host stories converge on.
  Until then, keep the loop owned by `moon-core`, observed by
  clients, never by a UI lifetime.

## Cross-spec touch-points

- [`architecture.md`](architecture.md) — the agent loop lives in
  moon-core; the UI never touches LLMs directly.
- [`protocol.md`](protocol.md) — `coder.*` methods replace the old
  `acp.*` sketch.
- [`slack-chat.md`](slack-chat.md) — the "real agent in the IDE"
  pointer re-points here.
- [`containers.md`](containers.md) — the agent is a moon-core
  component in the Tauri shell; its bash / fs tools cross into the
  container the way the terminal does.
