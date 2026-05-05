# Phase 6 — Coder

The right-side coder panel is moon-ide's own coding agent. It owns
its loop end-to-end: streams from HF Inference Providers, dispatches
its own tool calls, routes every tool through the active
[`WorkspaceHost`](../architecture.md#workspacehost-phase-2) so a
containerised workspace gets a containerised agent for free.

This file owns the work breakdown. The architectural spec is
[`coder.md`](../coder.md). The pivot from "ACP host" to "in-process
loop" is [ADR 0010](../decisions/0010-coder-rewrite-not-acp.md).
The crate rename is
[ADR 0011](../decisions/0011-rename-moon-agent-to-moon-remote.md).

## Sub-phases

### 6.0 — Skeleton

**Acceptance**: signed in with HF (device flow), the right-side
coder panel can prompt the "large" default model and render a
non-streaming reply. Tools are read-only (`read_file`, `list_dir`,
`grep`) plus `bash` routed through `WorkspaceHost::spawn`. No
streaming, no mutating tools, no session UI, no bucket sync.

What ships:

- Rename `crates/moon-agent/` → `crates/moon-remote/`. Workspace
  members updated; `[[bin]] name = "moon-remote"`. Per
  [ADR 0011](../decisions/0011-rename-moon-agent-to-moon-remote.md).
- New crate `crates/moon-coder/` with the loop scaffold:
  `auth/` (HF OAuth device flow + keyring storage + refresh
  middleware), `inference/` (HF router HTTP client,
  OpenAI-compatible, non-streaming), `tools/` (`read_file`,
  `list_dir`, `grep`, `bash`), `loop/` (stateful `Coder` with
  `prompt() / abort()`, fully synchronous turn for now).
- Tauri commands `coder_start_device_flow`, `coder_status`,
  `coder_sign_out`, `coder_send`, `coder_abort` in
  `src-tauri/src/commands/coder.rs`.
- Right-side `CoderPanel.svelte` with the empty / signed-in /
  active states from [`coder.md`](../coder.md#ui-placement). Reuses
  the Slack panel's sticky-bottom scroll + auto-grow textarea.
- Default models hardcoded in
  `crates/moon-coder/src/defaults.rs` (`Qwen/Qwen3.5-397B-A17B:scaleway`,
  `Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway`).
- Single in-memory session. No persistence.

Test plan: written before the commit.

### 6.1 — Streaming — **done**

**Acceptance**: assistant messages stream into the panel as SSE
chunks land, with `Esc` aborting the in-flight call cleanly
(partial assistant message preserved).

What shipped:

- SSE consumer in `inference.rs`. Inline `\n\n` / `\r\n\r\n` event
  parsing + accumulator that re-assembles content + tool calls
  from chunked OpenAI-shape deltas. Five unit tests cover the
  parser (comment/keepalive skip, multi-`data:` events, tool-call
  argument concatenation, LF + CRLF boundaries, empty-buffer
  filtering).
- New event vocabulary in `event.rs`: `AssistantMessageStart {
id }` → N × `AssistantMessageDelta { id, delta }` →
  `AssistantMessageEnd { id, text }` (`AssistantMessage` deleted
  per "no premature migrations"). `End.text` carries the
  canonical full content so any drift between concatenated
  deltas and the final assembly heals on close. Tool-call
  fragments are buffered server-side and only surfaced as
  `ToolCall` once fully assembled — partial JSON arguments
  aren't useful to render.
- Runner uses `chat_completion_stream`. The same
  `CancellationToken` that already cancels tool dispatch now
  also drops the SSE byte stream; Esc-abort is one `select!`
  arm in `consume_sse_stream`.
- Frontend `coder.svelte.ts` reconciles deltas by id; new
  `appendDelta` helper keeps the row mutation pure.
- `CoderMarkdown.svelte` coalesces re-renders to one per
  `requestAnimationFrame` tick. With ~30 deltas/sec, that caps
  the markdown-it + DOMPurify + grammar-lookup work at one
  render per paint frame; `End.text` triggers a final canonical
  render.

Reasoning / thinking deltas (added in the same sub-phase):

- Backend accepts both `reasoning_content` (DeepSeek, Qwen) and
  `reasoning` (other providers) under the same `thinking`
  buffer, with serde aliases so the non-streaming path picks
  them up too.
- New `AssistantThinkingDelta { id, delta }` event, plus a
  canonical `thinking: Option<String>` field on
  `AssistantMessageEnd`. The runner fires
  `AssistantMessageStart` on the first thinking _or_ content
  delta, so the panel inserts the row before reasoning lands.
- UI: collapsible `<details class="thinking">` block above the
  answer, auto-collapsed on `assistant_message_end`. Empty
  thinking is dropped server-side so non-reasoning models
  don't get a useless empty disclosure.

Deferred from the original 6.1 plan:

- `coderStream.ts` extraction: the streaming logic ended up
  small enough to live inline in `coder.svelte.ts`. Split out
  if `applyEvent` grows past one screen.
- Anthropic-style structured `thinking` blocks with
  cryptographic signing: out of scope, the HF router doesn't
  pass them through anyway.
- Thinking-duration display ("Thought for 12 s"): cheap to add
  if anyone asks, but the streaming animation already conveys
  "the model is thinking" clearly enough.

### 6.2 — Mutating tools

**Acceptance**: the agent can `write_file` and `edit_file`, with
edits showing up live in any open editor tab (CodeMirror picks
them up via the existing fs-watch path).

What ships:

- `write_file` and `edit_file` tools. `edit_file` uses exact
  string match; failure throws so the LLM can retry with bigger
  context. Multi-match disambiguation via a 1-based `occurrence`
  arg — passing it without a prior failure is fine, but the
  prompt steers the model toward "add more context" first.
  Open-buffer collision: if the target is a dirty open tab, the
  agent overwrites and the editor reloads — the workspace-state
  save path already handles "external mtime changed".
- Tool-call render: each call shows up as a collapsible block
  with `args` (input) and `result` (output) tabs (already in
  6.0; no new UI surface needed).
- System prompt updated to advertise edits and the
  exact-string-match retry pattern.

### 6.2.x — Container-aware bash

**Acceptance**: `bash` runs **inside the workspace shell
container** when that container is `Running`, on the host
otherwise. The panel header surfaces the active target as a
monitor / container glyph (same `TerminalTargetIcon` the bottom-
panel terminal tabs use) next to the username.

What ships:

- `tools::resolve_bash_target` queries
  `moon_container::Workspace::status()` — the same lifecycle
  call `lsp.rs::resolve_target` makes. Routes to container only
  when the project state is `Running`; any failure (no compose
  project, daemon unreachable, parse error) falls back to host
  so the agent is never made unusable by a flaky docker daemon.
- `WorkspaceFolder.host` (`Local` / `Devcontainer`) is **not**
  the routing signal. That field is for the orthogonal "the
  folder's filesystem lives in a remote container" case (Phase
  2-ish via `RemoteHost`); it doesn't track the workspace shell
  container's runtime state and is always `Local` today. The
  signal we want is "did the user click Set up / Resume?", which
  is `ContainerStatus.state == Running`.
- Container path: `docker exec -w <container_cwd> <name> sh -lc <cmd>`,
  using `moon_terminal::container_name_for_workspace` and
  `TerminalTarget::container_cwd_for_folder` so terminals, LSP,
  and the coder all agree on the framing. No `-it` (we want
  captured I/O, not a TTY).
- `CoderStatus.bash_target: "host" | "container" | null` mirrors
  the bash tool result's `target` field. The frontend listens to
  the `container:state` Tauri event and re-probes status on
  every state change, so the pip flips immediately when the user
  uses the container popover or runs `docker compose down` from
  a terminal.
- No new `WorkspaceHost::spawn` trait method. The "container is
  up" check belongs to the lifecycle layer, not to a host
  abstraction; adding `spawn` to a single-implementor trait
  would just add bookkeeping. The trait method earns its keep
  when `RemoteHost` lands and there's a second implementor.

### 6.3 — Sessions on disk + auto-rename — **done** (todo_write deferred)

**Acceptance**: every prompt/turn is persisted to JSONL under
`<workspace>/.moon/agent-sessions/`. The panel surfaces a
sessions list (with a sticky `+` button) and an in-session
header (with `← Sessions | title | +`). Clicking a session opens
it; hover-revealed trash icon + confirm dialog deletes.
`AppState.coder.last_session_id` round-trips and is restored on
relaunch. After the _first_ turn of a fresh session, the fast
model is asked for a 4-6 word title that replaces the
truncated-prompt fallback.

What shipped:

- `crates/moon-coder/src/sessions.rs` — JSONL writer/reader, lazy
  persistence (header written on first record append), `load_summary`
  fast path for the list view, and `validate_session_id` to keep
  user-supplied ids inside the sessions directory.
- Runner refactored to track session metadata (`SessionHeader`,
  `folder_root`, `auto_rename_pending`). On send the session
  binds to the active workspace folder + title-from-prompt; on
  every chat-history append the corresponding `SessionRecord`
  flushes to disk. Open-session replays records as the same
  events a live turn would emit so the panel's existing
  handlers populate the transcript without a special "loaded"
  code path.
- New event variants: `session_loaded`,
  `session_title_updated`, `session_list_changed`.
- New Tauri commands: `coder_list_sessions`,
  `coder_active_session`, `coder_new_session`,
  `coder_open_session`, `coder_delete_session`. The open command
  also writes `AppState.coder.last_session_id`.
- `AppState.coder = { last_session_id }` slice + matching merge
  rule in `app_state_save` (preserved from disk like `slack`
  and `right_panel`).
- `CoderPanel.svelte` gains the two-view layout (`coder.view`
  enum: `'list' | 'session'`), sticky session-bar with
  `← Sessions | title | +`, hover-revealed delete on session
  rows, and an `Intl.RelativeTimeFormat` time format on the
  list rows.
- Auto-rename pass: spawned after the first turn completes,
  uses `DEFAULT_FAST_MODEL` with a tight system prompt asking
  for a 4-6 word title. Result is sanitised (trim quotes,
  ellipsis-truncate, collapse whitespace) before it lands in
  the header. Failures keep the truncated-prompt fallback.

Deferred to a follow-up commit (still in the 6.3 spirit, not
rolled into 6.4):

- `todo_write` tool. The schema and UX are already in
  [`coder.md`](../coder.md#todo-list-tool); the implementation
  is independent of the persistence work and ships next.
- Per-folder `last_session_id`. Today the slot is flat — if
  the user switches workspace folder, the relaunch points at
  a session id that probably doesn't exist in the new folder
  and the panel falls back to the list view. Switch to a
  per-folder map when somebody actually feels this.
- Session search / inline rename / `last_n_turns` truncation.
  Add when asked.

### 6.4 — Model picker

**Acceptance**: the panel header has a model dropdown with the
hardcoded "large" / "fast" defaults plus any free-form HF slug
typed by the user. Picks persist per session in the JSONL header.

What ships:

- Model picker in `CoderPanel.svelte` header, sourced from
  `defaults.rs` plus the active session's override.
- `coder_set_model` Tauri command.
- The session JSONL header gets a `model` field (and stays
  schema-compatible with the 6.3 header — additive).

### 6.5 — Steering, follow-up, ask_user

**Acceptance**: pressing Enter while the agent is streaming or
running tools queues a steering message (delivered after the
current turn settles). Pressing Alt+Enter while the agent is
between turns queues a follow-up (delivered after the agent stops
calling tools). The queued message is visible above the composer
with a tiny "queued" pip; Esc clears the queue. The agent can
also pause its turn with `ask_user` and the panel renders an
inline multiple-choice prompt that resolves on click.

What ships:

- Loop-level queue: `Coder::steer(msg)` and `Coder::follow_up(msg)`,
  both `one-at-a-time` mode (one queued message at a time, second
  one replaces the first with a small toast).
- Composer keymap: Enter sends if idle, queues steer if
  streaming/tools; Alt+Enter queues follow-up; Shift+Enter
  newline; Esc aborts (or clears the queue if there's nothing
  to abort).
- Queue indicator UI above the composer.
- `ask_user` tool per
  [`coder.md`](../coder.md#ask-user-tool). Backend wires a
  `oneshot::Receiver<UserChoice>` keyed by `tool_call_id`; new
  Tauri command `coder_respond_to_prompt(call_id, response)`.
  Cancellation tokens drop pending oneshots so abort is clean.
- `CoderAskUser.svelte` block rendered inline in the transcript:
  buttons for `options[]`, optional "Other…" textarea, optional
  multi-select with a confirm button. Auto-scrolls into view when
  it appears.

### 6.6 — System prompt + skills

**Acceptance**: the agent's system prompt includes (a) the
hardcoded base, (b) `AGENTS.md` walked from the active workspace
folder, (c) `<workspace>/.moon/SYSTEM.md` if present, (d) a list
of discovered `SKILL.md` files with their frontmatter
descriptions. The agent can `read_file` a skill body when it
decides to use one.

What ships:

- Skill discovery walks `skills/`, `.moon/skills/`,
  `.claude/skills/`, `.cursor/skills/`, `.cursor/skills-*/`,
  `.agents/skills/` (recursively, one level deep) for
  `SKILL.md` files under the active workspace folder and its
  parents. Reads YAML frontmatter (`name`, `description`,
  `fullPath`).
- Single-pass compaction: when the next turn would exceed the
  model's context limit, the older prefix is summarised by a
  `fast`-model call and replaced in-context.
- The full JSONL on disk is untouched (compaction only affects
  what the LLM sees, not what we persist).

### 6.7 — Bucket sync

**Acceptance**: on first sign-in moon-coder ensures the
`<user>/moon-ide-sessions` private bucket exists; every workspace's
sessions sync to `<workspace-slug>/<session-id>.jsonl` automatically
via `hf-xet`. The first time a fresh workspace starts uploading,
the panel shows a one-time banner with an inline opt-out toggle.
Sync failures pip the status bar; local JSONL stays the source of
truth.

What ships:

- `crates/moon-coder/src/sync/`:
  - REST client for the bucket lifecycle endpoints (`create`,
    `list`, `delete`, batch metadata) — wraps the Hub APIs the
    Python `huggingface_hub` library exposes for `repo_type="bucket"`
    (see [Buckets API PR #3673](https://github.com/huggingface/huggingface_hub/pull/3673)).
  - `hf-xet` integration for byte transfer (`XetSession` +
    `new_upload_commit`).
  - Per-session debounced upload task (5 s quiescence, force-flush
    on session close / app exit).
  - Tombstone file (`.tombstones`) for deleted sessions; sync
    deletes the remote key and removes the line.
- `coder_set_sync_enabled(enabled)` Tauri command and panel-header
  toggle.
- One-time per-workspace banner in `CoderPanel.svelte`.
- `coder:sync_state` Tauri events drive a status-bar pip.

End of Phase 6. Hand back for human review per
[`AGENTS.md` § Phased delivery](../../AGENTS.md#phased-delivery).

## What this phase deliberately doesn't do

The full list lives in [`coder.md`'s "Out of scope"](../coder.md#out-of-scope-explicitly).
The headline items:

- **Sub-agents** — schema and design in
  [`coder.md`](../coder.md#sub-agents-planned-not-in-initial-sub-phases),
  no implementation. Lands when somebody surfaces the first
  concrete use case (cheap-model research / mechanical refactor).
- **Custom providers / OpenRouter / local OpenAI-compat** —
  `AppState.coder.providers` slot exists, no UI.
- **Anthropic OAuth (Claude Pro/Max)** — separate flow, lands
  when somebody asks for subscription billing.
- **Bucket browser** — "import a session from another machine"
  UI. Bucket is backup-only.
- **Plan mode / permission popups / MCP / skill packages** —
  per ADR 0010, posture matches pi.

## Open questions

- **Compaction quality.** Single-pass summarisation may drop
  context the user actually wanted. Revisit if "the agent forgot
  the earlier discussion" reports come in. Branching / tree
  history (pi-style `/tree`) is a possible answer; not committing
  yet.
- **Skill discovery walks one level deep.** `cursor`'s
  `.cursor/skills-*/` convention has nested dirs; we'll go deeper
  only if a real skill set we want to load needs it.
- **`hf-xet` Tokio runtime.** The crate has its own runtime; we
  may need to feature-flag or share Tauri's. Surface during 6.7.
