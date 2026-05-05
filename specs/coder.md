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
explains why the new crate is `moon-coder` and the existing
remote-host stub becomes `moon-remote`. Sub-phase work breakdown:
[`roadmaps/phase-06-coder.md`](roadmaps/phase-06-coder.md).

## Why we own the loop

The Phase 6 reasons in ADR 0010, in one paragraph: ACP is a
protocol for driving somebody else's agent binary. The agents the
team would actually pick (Claude Code, Cursor, opencode) aren't ACP
native, and the ones that are (pi-coding-agent) are TS-first.
Either we adopt a Node sidecar and inherit pi's release cadence, or
we put fs/bash tools in JS land and break the architecture
invariant. Owning the loop in Rust is a few hundred lines around
`reqwest` + an SSE parser + a tool dispatcher, and it lets every
tool be a moon-core method that already respects `WorkspaceHost`.

## Loop shape

The vocabulary is borrowed from
[pi-agent-core](https://github.com/badlogic/pi-mono/tree/main/packages/agent)
because the event names are good and we have nothing to gain by
inventing our own:

```
prompt(msg)
├─ agent_start
├─ turn_start
├─ message_start    { role: "user", … }
├─ message_end      { role: "user", … }
├─ message_start    { role: "assistant" }
├─ message_update   { delta: "…" }*           // SSE chunks
├─ message_end      { role: "assistant", tool_calls?: […] }
├─ tool_execution_start  { id, name, args }*  // per tool call
├─ tool_execution_update { id, partial }*     // optional
├─ tool_execution_end    { id, result | error }*
├─ message_start    { role: "tool", … }       // tool result back to LLM
├─ message_end
├─ turn_end                                   // loop continues if tools were called
│
├─ turn_start                                 // next turn
├─ …
└─ agent_end        { messages: […] }
```

Tool calls in a single assistant turn run in **parallel** by default
(the LLM gets the results back together on the next turn). Sequential
mode is configurable per call but isn't surfaced in the UI initially.

States the panel needs to render:

| State       | Where it shows                                                                |
| ----------- | ----------------------------------------------------------------------------- |
| `idle`      | composer is the active focus, send button enabled                             |
| `streaming` | message_update events landing on the current assistant message; abort enabled |
| `tools`     | one or more `tool_execution_*` blocks expanding inline; abort enabled         |
| `error`     | error banner above composer, "retry" / "continue" affordances                 |

Three control surfaces:

- **Abort** (`Esc`): cancels the in-flight HTTP / SSE / tool-call.
  The session keeps the partial assistant message and any completed
  tool calls; the loop stops cleanly.
- **Steer** (Enter while `streaming` / `tools`): queue a user message
  to be delivered after the current turn settles (i.e. all tool
  calls from this assistant message complete). One queued message
  at a time by default; the queue is visible above the composer.
- **Follow-up** (Alt+Enter while idle-but-just-finished): queue a
  user message to be delivered after the agent finishes _all_ work —
  i.e. when there are no more tool calls and no steering messages
  to inject.

These match pi's defaults. They also match the Slack panel's
keymap (Enter sends, Shift+Enter newline) so a user moving between
the two doesn't relearn anything.

## Authentication

### HF OAuth Device Authorization Grant (RFC 8628)

The IDE registers a public OAuth app on Hugging Face Hub with
client ID **`7977dff4-917a-4cf9-a726-dd45e25faa5f`**. No client
secret. Scopes asked upfront, just like
[Phase 11's Slack flow](slack-chat.md#required-scopes-user-token):

| Scope              | Used by     | What it gets us                                                                                                                                                                                                                                                |
| ------------------ | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `inference-api`    | LLM calls   | Call HF Inference Providers' router endpoint                                                                                                                                                                                                                   |
| `contribute-repos` | Bucket sync | Create + write to the `<user>/moon-ide-sessions` private bucket. Strictly weaker than `manage-repos` (which would also grant repo deletion and settings edits); `contribute-repos` is the minimum that lets us `create-repo` + `push`, which is all sync needs |

The `read-billing` and `email` scopes are deliberately **not**
asked for: we don't need to read the user's email and we don't
surface billing in the IDE. Likewise we explicitly avoid
`manage-repos` — granting delete-repo on the user's whole
namespace would be way more power than session sync warrants.

### Flow

1. User clicks "Sign in with Hugging Face" in the coder panel
   header (or in the connect modal on first open). `moon-coder`
   `POST`s `https://huggingface.co/oauth/device` with
   `client_id=7977dff4-…`, `scope=inference-api contribute-repos`.
2. Response carries `device_code`, `user_code`, `verification_uri`,
   `expires_in`, `interval`. The IDE renders a modal showing the
   user code in a big monospace box and a "Open in browser" button
   that opens `verification_uri` (typically
   `https://huggingface.co/login/device`) via
   `tauri-plugin-opener`.
3. Background poll on `interval` seconds (typically 5 s) against
   `https://huggingface.co/oauth/token` with
   `grant_type=urn:ietf:params:oauth:grant-type:device_code`,
   `device_code`, `client_id`. Returns `access_token`,
   `refresh_token`, `expires_in`, `token_type=bearer`.
4. Persist `(access_token, refresh_token, expires_at)` to the OS
   keyring as a single JSON blob under
   `service=moon-ide`, `account=hf-oauth`. Same keyring backend the
   Slack panel already uses (`apple-native + windows-native +
sync-secret-service + crypto-rust` features in `keyring` 3.x).
5. The IDE pulls the user's profile (`GET https://huggingface.co/api/whoami-v2`)
   for the panel header (avatar, username, namespace). Cached in
   `AppState.coder.identity`.

### Refresh

The HTTP middleware on the inference + bucket clients refreshes the
access token when **`expires_at - now < 60 s`** (covers clock
skew) and on a 401 response (one retry). Refresh is `POST` to
`https://huggingface.co/oauth/token` with
`grant_type=refresh_token`, `refresh_token`, `client_id`. A
successful refresh writes the new triple back to the keyring.

A failed refresh (`invalid_grant`, network outage, …) drops the
keyring entry, fires `coder:disconnected` to the UI, and the panel
returns to the "Sign in with Hugging Face" empty state. The user
re-authorizes; everything else (sessions, bucket pointer) stays.

### Disconnect

Explicit "Disconnect" in the panel header confirms (modal), then:

- Drops the keyring entry.
- Clears `AppState.coder.identity` and any in-memory token.
- Does **not** revoke server-side. HF's revoke endpoint isn't
  reliably documented for device-flow tokens; revocation is the
  user's job from their HF account settings (we link it from the
  confirm dialog).

Token rejection mid-session (HF returns `invalid_token` /
`expired_token` and refresh fails) follows the same path as an
explicit disconnect, plus a toast.

## Providers

### Day-1: HF Inference Providers (only)

One HTTP client, OpenAI-compatible schema, against
`https://router.huggingface.co/v1`:

- `POST /chat/completions` (streaming via SSE when `stream: true`)
- `Authorization: Bearer <access_token>`

The model field carries HF's provider-routing slug verbatim:
`Qwen/Qwen3.5-397B-A17B:scaleway`,
`Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway`. The IDE doesn't parse it; the
router does.

#### Default models

Hardcoded in `crates/moon-coder/src/defaults.rs`:

- **`large`** → `Qwen/Qwen3.5-397B-A17B:scaleway` — the day-to-day
  default for chat / refactor / multi-step tasks.
- **`fast`** → `Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway` — the cheap default,
  used as the sub-agent default once that lands.

The "large" / "fast" abstraction lives at the moon-coder level so
sub-agents and future "rerun this turn on a cheaper model"
affordances aren't tied to a specific HF slug. Swapping a default
is a one-line constant change.

Per-session model override is stored in the session JSONL header.
Per-workspace defaults arrive only when somebody asks; until then,
new sessions start at the global defaults.

#### Why HF as primary

- The team's home turf (every model the team cares about is
  already on HF, including provider-routed access to Scaleway,
  Together, Fireworks, …).
- One auth flow, one HTTP client, no per-provider integration.
- Bills via the user's HF account — we don't ship API keys.
- Adding more providers later is additive: a per-provider config
  array on `AppState.coder.providers[]`. The HF flow stays
  canonical.

### Later: custom OpenAI-compatible endpoints (OpenRouter, local)

`AppState.coder.providers` becomes a `Vec<ProviderConfig>` with:

```rust
struct ProviderConfig {
    id: String,                  // "openrouter", "ollama-local", …
    label: String,               // shown in the model picker
    base_url: String,            // OpenAI-compat /v1 root
    auth: ProviderAuth,          // ApiKey { keyring_account } | None
    model_prefix: Option<String>,// optional UI hint
}
```

API keys live in the keyring under
`service=moon-ide`, `account=coder-provider:<id>`. Adding a
provider is a small modal that takes `(label, base_url, api_key?)`,
verifies via a `GET /v1/models` (or a 1-token completion if `models`
isn't supported) before saving.

This is **not** in the initial sub-phases. It lands when someone on
the team wants OpenRouter, a local vLLM, or Anthropic-direct.

### Later: Anthropic OAuth (Claude Pro / Max)

A second OAuth flow, parallel to HF's, persisted in its own keyring
slot. Lands when somebody wants subscription billing instead of
API-key / HF-routed billing. Not in the initial sub-phases.

## Tool surface

Every tool is a moon-core method, dispatched through the active
`WorkspaceHost`. A workspace running in a Phase 2 container gets
container-bound tools without the agent loop knowing.

The schema is JSON-Schema in the request to the LLM; the
implementations are typed Rust:

| Tool         | Signature                                                                                               | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `read_file`  | `(path, start_line?, end_line?) -> { content, start_line, end_line, total_lines, truncated, mtime_ms }` | `content` is line-numbered: every line is prefixed with `<line_no>\|<line>` (right-aligned, width sized to the largest visible number). The prefix is metadata, not part of the file. `start_line` / `end_line` are 1-based and inclusive; `end_line` is clamped to EOF and the response echoes the _effective_ range so the model can detect short reads. Refuses paths outside the active workspace folder.                                                                                    |
| `write_file` | `(path, content) -> { path, bytes_written, mtime_ms }`                                                  | Creates parents only if they exist; agent does `bash mkdir -p` first when it needs to                                                                                                                                                                                                                                                                                                                                                                                                            |
| `edit_file`  | `(path, find, replace, occurrence?) -> { path, bytes_written, mtime_ms, occurrence, total_matches }`    | `find` is an exact substring (whitespace significant); empty `find` rejected; non-unique match without `occurrence` throws so the LLM retries with more context                                                                                                                                                                                                                                                                                                                                  |
| `list_dir`   | `(path) -> DirEntry[]`                                                                                  | Honours the same gitignore-aware walk the file tree uses                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `grep`       | `(pattern, case_sensitive?, max_matches?) -> { pattern, matches, count, truncated }`                    | `matches` is one hit per line in `path:line: text` form (line is 1-based). The exact line numbers feed back into `read_file`'s `start_line` / `end_line` so the typical loop is `grep` → narrow `read_file` → `edit_file`. Backed by the existing `ignore`/ripgrep dep.                                                                                                                                                                                                                          |
| `bash`       | `(cmd, timeout_ms?) -> { cmd, target, stdout, stderr, exit_code }`                                      | Routes to `docker exec -w <container_cwd> <name> sh -lc <cmd>` when the workspace shell container's lifecycle status is `Running`, else `sh -lc <cmd>` rooted at the active folder. The decision is made by `tools::resolve_bash_target`, which calls the same `moon_container::Workspace::status()` query `lsp.rs` already uses — so terminals, LSP, and the coder agree on the routing target. `target` field echoes `"host"` / `"container"` so the panel pip and the tool result can't drift |

Tools that arrive **as separate commits when proven needed**, not
in the initial slice:

- `goto_definition`, `find_references`, `hover` — wrappers around
  the existing LSP broker. Cheap and high-value once the LLM
  starts navigating code structurally.
- `git_status`, `git_diff`, `git_blame`, `git_log` — read-only
  wrappers around the existing git layer.
- `apply_diff` — unified-diff applier so the model can emit a
  single patch instead of N `edit_file` calls. Token-efficient on
  multi-file edits.
- `editor_open` — focus a file at line/col in the running IDE.
  Useful so the agent can drop the user at the right place after
  a refactor.
- `todo_write` — see [§ Todo list](#todo-list-tool).
- `ask_user` — see [§ Asking the user](#ask-user-tool).

### Todo list tool

`todo_write` lets the agent keep a small in-context to-do list as
it works through multi-step tasks. Same shape as the Cursor /
pi-mono convention so prompts can be carried over verbatim:

| Field    | Type                                                       | Notes                                                                                                                                   |
| -------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `todos`  | `[{ id, content, status }]`                                | Full list (with `merge=false`) or partial updates (with `merge=true`).                                                                  |
| `merge`  | `bool`                                                     | When `true`, items are matched by `id` and merged; new ids are appended. When `false`, the list is replaced wholesale. Default `false`. |
| `status` | `"pending" \| "in_progress" \| "completed" \| "cancelled"` | Same vocabulary as Cursor's `TodoWrite`.                                                                                                |

Backend semantics:

- The list is **session-scoped**. The loop keeps it in the
  session's state object (alongside `messages`) and snapshots it
  as part of the session JSONL header (Phase 6.3). Reset when
  the user starts a new session; never persisted across users
  (it's per-session, not per-workspace).
- The tool result is the _current full list_ after the update so
  the model always sees its own bookkeeping.
- Only one item should be `in_progress` at a time. The tool does
  **not** enforce this — the prompt does, the same way Cursor's
  prompt does. Enforcing it would make benign races (the model
  flips two items in one call) into errors for no benefit.

Frontend rendering:

- A sticky list near the top of the panel transcript shows the
  current todos with their status glyphs. Re-renders on every
  `tool_result` for `todo_write`.
- The agent owns the list. The user can't tick items by hand —
  this is the agent's scratchpad, not a shared task tracker.
  Editing-by-user is a separate question that we'd answer by
  having the user _ask_ the agent rather than by mutating the
  list directly.

### Ask user tool

`ask_user` lets the agent pause its turn and ask the user a
multiple-choice question. The natural use case is the moment
where the agent legitimately doesn't know which of two valid
paths to take and asking the human is cheaper than guessing
wrong twice. Inspired by Cursor's `AskQuestion` shape.

| Field            | Type              | Notes                                                                                 |
| ---------------- | ----------------- | ------------------------------------------------------------------------------------- |
| `question`       | `string`          | What the agent wants to know. Rendered as the prompt text.                            |
| `options`        | `[{ id, label }]` | Buttons the user can click. At least 2.                                               |
| `allow_other`    | `bool`            | When `true`, an "Other…" textarea is shown alongside the buttons. Default `true`.     |
| `allow_multiple` | `bool`            | Multi-select with a confirm button. Default `false` (single-select submits on click). |

Wire shape: this is the first tool that needs **bi-directional**
flow between the loop and the panel. The runner blocks the tool
call on a `oneshot::Receiver<UserChoice>`:

1. Loop emits a normal `tool_call` event with `name: "ask_user"`.
2. Frontend renders the prompt block with buttons (and the
   "Other" textarea when allowed).
3. User clicks an option → the panel calls a new Tauri command
   `coder_respond_to_prompt(call_id, response)`.
4. The backend resolves the oneshot; the tool returns
   `{ choice: { id, label, free_text? } }`; the loop emits the
   matching `tool_result` event and continues.
5. If the user aborts the turn (or closes the panel) before
   responding, the cancellation token short-circuits the
   oneshot and the tool returns `Aborted` — no half-state for
   the model to puzzle over.

Concurrency: only one `ask_user` call is in flight at a time
(the loop is single-turn-at-a-time and `ask_user` blocks the
turn). A second call before the first resolves is a loop bug;
the runner asserts on it rather than queueing.

System-prompt guidance: the base prompt tells the agent **not**
to use `ask_user` for clarification it could resolve by reading
files. The tool is for genuine forks where the user's intent is
the missing input — "rename it across the codebase or just
locally?", "delete the old file or keep it for compat?". The
prompt explicitly discourages "should I proceed?"-style polling
because that turns the agent into a confirmation maze.

### Error model

Tools **throw** on failure (`Result::Err` becomes
`isError: true` in the tool result message back to the LLM).
Returning a string like "ERROR: file not found" as content is
banned — it confuses the model and is the
[explicit pi convention](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md#error-handling).

### Permissions

No popups. The user's safety boundary is the workspace container
(Phase 2), not a per-call confirm dialog. If they ran `bun run dev`
moon-ide outside a container, their bash tool runs on the host —
that's their choice. The panel header surfaces a small "running on
host" / "running in container" indicator so the boundary is
visible, not hidden.

The one exception is **explicitly destructive built-ins** (none in
the initial set, listed here so we don't drift): `git push --force`,
`rm -rf` outside the workspace folder, etc. If we add such a tool,
it gets a one-time confirm. We do not gate `bash` itself; bash
inside a container is the safety story.

### What the LLM sees as system prompt

Concatenated, in this order, with `\n\n` between sections:

1. Hardcoded base prompt (workspace-aware): "You are moon-coder,
   the coding agent built into moon-ide. The user is working on
   …; here are the tools you have …".
2. **`AGENTS.md`** from the active workspace root, walked up
   parent dirs (a la `.editorconfig` / `git`). Same convention
   the rest of the agent ecosystem uses.
3. `<workspace>/.moon/SYSTEM.md` if present (project-specific
   override — used to extend, not replace, the base prompt).
4. **Skills** discovered from these directories under the active
   workspace folder (or any parent — same walk-up convention as
   `AGENTS.md`):
   - `skills/<name>/SKILL.md` — the project-local convention used
     by `agentskills.io` and the `pi`/`claude` agent ecosystem.
   - `.moon/skills/<name>/SKILL.md`
   - `.claude/skills/<name>/SKILL.md`
   - `.cursor/skills/<name>/SKILL.md` and `.cursor/skills-*/<name>/SKILL.md`
   - `.agents/skills/<name>/SKILL.md`

   Each skill is a `SKILL.md` whose frontmatter declares `name` +
   `description` (per the [agent-skills standard](https://agentskills.io)).
   All discovered skills' descriptions are listed in the system
   prompt as a "you may invoke skill X by reading file Y" index;
   the body of a skill is loaded via `read_file` only when the
   agent decides to use it. This keeps the system prompt small.

We do **not** ship a skill installer / skill package manager.
Skills are file conventions; the user drops `SKILL.md` into one of
the supported directories.

## Sessions

### On disk

Append-only JSONL at
`<workspace>/.moon/agent-sessions/<session-id>.jsonl`. One line per
event. The first line is a header:

```json
{
	"type": "header",
	"schema": 1,
	"id": "01HXY…",
	"created_at": 1714896000,
	"workspace": "<absolute path>",
	"model": "Qwen/…:scaleway",
	"fast_model": "google/…:scaleway"
}
```

After that, every event from the loop's vocabulary is appended as
it fires. A crash loses at most the in-flight event.

### Sidebar UI

A session list in the panel sidebar (collapsible). One row per
session, newest first, showing:

- The first user message truncated to ~80 chars (matches the
  Slack-panel preview).
- Timestamp of the latest event (relative — "2m", "yesterday").

Clicking a row opens that session into the main view. The panel's
sticky-bottom scroll behaviour mirrors the Slack panel's — opening
a session lands at the latest reply; the agent's own streamed
messages keep the scroll pinned iff the user was already at the
bottom.

`+ New session` creates a fresh `<session-id>.jsonl` and seeds the
header. Deleting a session removes the file from disk and from the
bucket on next sync (a one-line `tombstone:<id>` is appended to a
`<workspace>/.moon/agent-sessions/.tombstones` file so the sync
loop knows to delete on the remote side).

### Compaction

Long sessions exceed the model's context window. moon-coder ships
a **single compaction strategy** — "summarize older turns" — and
calls it automatically when the next turn would exceed the model's
context limit (provider-reported), or manually via a "Compact"
command. The summary turn is a fast-model call against the older
prefix; the result becomes a synthetic `system` message that
replaces the prefix in the LLM context. The full JSONL on disk is
untouched (so a future "view raw history" affordance can still
show the original).

## Bucket sync (HF buckets)

Buckets are HF Hub's S3-like object storage backed by Xet — see
[the official guide](https://huggingface.co/docs/huggingface_hub/guides/buckets)
and [the Buckets API PR](https://github.com/huggingface/huggingface_hub/pull/3673).
They're a different `repo_type="bucket"` from models / datasets /
spaces. moon-coder uses one bucket per user to keep the team's
session history in one place.

### Bucket layout

- **One per-user private bucket**, name `moon-ide-sessions`. Created
  on first sign-in with the API equivalent of
  `create_bucket("moon-ide-sessions", private=true, exist_ok=true)`.
- Key prefix per workspace: `<workspace-slug>/<session-id>.jsonl`
  where `<workspace-slug>` is a stable short hash of the
  workspace's canonical absolute path (e.g.
  `<sha256(path)[:16]>`). Renaming a folder doesn't fork its
  history; moving a checkout to another machine still lines up.
- A small `<workspace-slug>/manifest.json` lists the workspace's
  human-readable name, last-seen path, and session-id → first-line
  preview. Useful for a future "browse remote sessions" UI.

### Sync cadence

A debounced background task per session:

- Local writes are append-only, so the sync task knows the new
  byte offset. Every **N seconds of quiescence** (default 5 s)
  after the last local write, the task uploads the **whole file**
  again — Xet dedup makes this nearly free, since the unchanged
  prefix re-uploads as a hash-only reference.
- Force-flushes on session close (panel hidden / app exit).
- The task uses `hf-xet`'s `XetSession` for the byte transfer and
  the Hub REST API (`/api/buckets/...` endpoints) for create /
  list / delete operations. We don't shell out to the Python
  `huggingface_hub` CLI.

### Privacy / opt-out

Default: **on for every workspace once signed in**. The first time
a fresh workspace's session uploads, the panel shows a one-time
banner:

> Syncing this workspace's coder sessions to your private HF bucket
> `moon-ide-sessions/<workspace-name>`.

The banner has an inline toggle next to that text — flipping it off
sets `AppState.coder.sync_disabled_workspaces[]` for the workspace
and dismisses the banner. The same toggle is reachable from the
panel header after the banner is gone, so the off-switch is always
one click away.

A failed upload is a `tracing::warn!` plus a small status-bar pip
("coder sync delayed"). Local JSONL stays the source of truth.

### What never leaves the host

- HF access / refresh tokens (keyring only).
- Provider API keys for any custom provider (keyring only).
- File contents that the agent _reads_ during a session **do**
  end up in the JSONL (they're part of the tool result message),
  and therefore in the bucket. NDA workspaces should toggle sync
  off; the banner exists exactly for this case.

## Sub-agents (planned, not in initial sub-phases)

A `spawn_subagent` tool exposed to the parent loop:

```jsonschema
spawn_subagent(
  task: string,                    // human-readable description
  system_prompt?: string,          // overrides the workspace default
  model?: "fast" | "large" | string, // defaults to "fast"
  allowed_tools?: string[]         // defaults to read-only
) -> { result: string, tokens_used: number }
```

Implementation: the parent's tool dispatcher constructs a fresh
`Loop` instance with its own `messages: []`, the requested model
slug, and a tool subset (default: `read_file`, `list_dir`, `grep`).
The sub-agent runs the full loop and returns a single text result;
the parent only ever sees that string in its context. Token cost,
turn count, and the sub-session JSONL are persisted alongside the
parent (under `.moon/agent-sessions/<parent-id>.subs/<sub-id>.jsonl`)
and shown collapsed in the parent's tool-call render.

Use cases the team has surfaced:

- "Research where in the codebase X is used and summarize" — a
  `grep` + `read_file` loop on the cheap model that returns one
  paragraph instead of polluting the parent's context with 20
  file reads.
- "Apply this list of mechanical refactors" — a write-capable
  sub-agent that edits files and reports back with a diff
  summary. (Tools: `read_file`, `edit_file`, `apply_diff`, `bash`.)

Open questions for when this lands:

- Per-sub-agent budget (max turns / max tokens).
- Whether the parent can `abort` an in-flight sub-agent
  separately from itself.
- UI: collapsed-by-default render inside the parent's tool-call
  block, expand to see the sub-session. Probably shares
  rendering machinery with the regular tool-call view.

## UI placement

A right-side panel docked to the editor area. Chat and coder are
**mutually exclusive tenants** of a single right-side slot — the
two panels share one width and the user toggles between them; we
don't stack two narrow columns on top of each other. Which surface
is mounted (or `null` for closed) lives on `AppState.right_panel`
and is restored across launches via the dedicated
`ui_set_right_panel` Tauri command. Resizable horizontal splitter
(uses the same hand-rolled splitter the Slack panel currently
uses; will switch to `paneforge` when that lands repo-wide).

Toggleable from:

- Status-bar button (robot/wand icon, active when signed in and a
  session is open).
- Command palette: `Coder: Toggle Panel`.
- Keyboard: `F6` / `Shift+F6` cycle (the chat panel is in this
  rotation today; whichever surface is mounted in the right slot
  takes that focus stop).

Top-to-bottom layout:

```
┌──────────────────────────────────────┐
│ HF identity card | sign out          │  ← only when signed in
├──────────────────────────────────────┤
│ Sessions ▾  | + New session          │  ← session list (collapsible)
│ "implement bucket sync" · 2m         │
│ "rename moon-agent → moon-remote" · 1h│
├──────────────────────────────────────┤
│                                      │
│  [user] you said …                   │
│  [assistant] thinking… (collapsed)   │
│  [assistant] streamed reply …        │
│  [tool] read_file src/lib/state…     │
│         (expandable: input | output) │
│  [assistant] continues …             │
│                                      │
├──────────────────────────────────────┤
│ Model: large ▾  | sync: ●            │  ← session controls
├──────────────────────────────────────┤
│ ┌──────────────────────────────────┐ │
│ │ Type a message — Enter to send   │ │
│ └──────────────────────────────────┘ │
└──────────────────────────────────────┘
```

States the panel renders at the top:

- **Not signed in**: empty state with a "Sign in with Hugging
  Face" primary button.
- **Signed in, no session**: identity card + session list + a
  big "+ New session" button if the list is empty.
- **Active session**: full layout above.

## Frontend ↔ backend boundary

Tauri commands in `src-tauri/src/commands/coder.rs`:

| Command                                                    | Purpose                                                                                               |
| ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `coder_start_device_flow()`                                | Returns `{ user_code, verification_uri, expires_in, interval }`. Background poll runs in `moon-coder` |
| `coder_status()`                                           | `{ signed_in, identity?, has_session, sync_enabled }`                                                 |
| `coder_sign_out()`                                         | Drops keyring + identity                                                                              |
| `coder_list_sessions()`                                    | List of `{ id, first_line, latest_event_ts }`                                                         |
| `coder_open_session(id?)`                                  | If `id` given, load it; else create a new one. Returns the new active id                              |
| `coder_delete_session(id)`                                 | Removes JSONL + tombstones for sync                                                                   |
| `coder_send(text, mode: "send" \| "steer" \| "follow_up")` | Routes to the loop                                                                                    |
| `coder_abort()`                                            | Cancels the in-flight loop                                                                            |
| `coder_set_model(slug)`                                    | Override on the active session                                                                        |
| `coder_set_sync_enabled(enabled)`                          | Per-workspace bucket-sync toggle                                                                      |

Push events from backend → frontend (Tauri event channel):

- `coder:event` — every loop event (`agent_start`, `turn_start`,
  `message_start/update/end`, `tool_execution_*`, `turn_end`,
  `agent_end`, `error`). Single channel; the UI dispatches by
  `event.type`.
- `coder:signed_out` — token went bad / explicit sign-out.
- `coder:sync_state` — `{ session_id, status: "uploading" | "ok" |
"delayed", error? }` for the status-bar pip.

## Failure modes

| Scenario                                     | UI behaviour                                                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| User has no HF token                         | "Sign in with Hugging Face" empty state                                                          |
| Device-flow polling expires                  | Modal shows "code expired, retry"                                                                |
| Token rejected by `/whoami-v2` after refresh | Toast + return to empty state; keyring cleared                                                   |
| `router.huggingface.co` 5xx                  | Streaming surfaces an error; "Retry" button performs a `continue()` against the existing context |
| Tool throws                                  | LLM gets `isError: true` + the message; loop continues                                           |
| Bucket creation 4xx (e.g. quota)             | `coder:sync_state delayed` pip; sessions stay local; banner offers "show details"                |
| `hf-xet` upload partial-fail                 | Retry on next quiescence; if still failing after 3 attempts, mark `delayed`                      |
| Network down                                 | Streaming aborts; same retry surface as 5xx; sync stays in `delayed` until network returns       |

## Frontend module layout

New files (all per [`frontend.md`](frontend.md) conventions):

```
src/lib/
├── components/
│   ├── CoderPanel.svelte
│   ├── CoderConnectModal.svelte    // device-flow user code display
│   ├── CoderSessionList.svelte
│   ├── CoderMessage.svelte
│   ├── CoderToolCall.svelte
│   └── CoderComposer.svelte
├── coder.svelte.ts                 // panel state ($state runes)
└── util/coderStream.ts             // event-channel parser
```

The panel reuses the Slack panel's sticky-bottom scroll and
auto-grow textarea verbatim — that's a five-line lift each time;
no shared component. ADR 0003 ("no adapter layer") still applies.

## Out of scope (explicitly)

- **Pluggable agent binaries** (ACP) — superseded by ADR 0010.
- **Plan mode** — the team can write plans into `AGENTS.md` /
  `SYSTEM.md`. Reconsider when somebody asks.
- **Permission popups** — see "Permissions" above.
- **MCP** — same posture as pi.
- **Sub-agent UI / scheduling** — the schema and plan are above;
  no implementation in the initial sub-phases.
- **Skill packages / installable skills** — file conventions only.
- **Custom providers** — schema lives in `AppState.coder.providers`
  but no UI / wiring in the initial sub-phases.
- **Bucket browser** — "import session from bucket" UI for picking
  a chat up on a different machine. Bucket is backup-only at first.
- **Multi-account** — one HF account per moon-ide install.
- **Background agent runs** — the loop only runs while the panel
  is active. A "run this overnight" mode is a Phase 12 problem.

## Cross-spec touch-points

- [`architecture.md`](architecture.md) — the agent loop lives in
  moon-core; the UI never touches LLMs directly. Cross-cutting
  invariant updated.
- [`protocol.md`](protocol.md) — `coder.*` methods replace the
  sketch of `acp.*`.
- [`AGENTS.md`](../AGENTS.md) — invariant 1 updated to mention
  coder instead of ACP.
- [`slack-chat.md`](slack-chat.md) — the "real agent in the IDE
  with context" pointer in the lead paragraph re-points at this
  spec.
- [`containers.md`](containers.md) — the "what runs on the host
  vs the container" table flips the ACP row to coder. The agent
  itself is a moon-core component, so it lives in the Tauri shell;
  its bash / fs tools cross into the container exactly the way
  the terminal does.
