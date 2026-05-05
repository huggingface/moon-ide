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
  `Qwen/Qwen3.6-35B-A3B:deepinfra`).
- Single in-memory session. No persistence.

Test plan: written before the commit.

### 6.1 — Streaming

**Acceptance**: assistant messages stream into the panel as SSE
chunks land, with `Esc` aborting the in-flight call cleanly
(partial assistant message preserved). Thinking blocks (when the
provider returns them) render collapsed under the assistant
message.

What ships:

- SSE client in `inference/`. Push-based parsing into the loop's
  event vocabulary (`message_update` for each delta).
- Tauri event channel `coder:event` with the full event set.
- Cancel propagation: `coder_abort` cancels the SSE read + any
  in-flight tool, the loop emits `agent_end { aborted: true }`
  and shuts down.
- Frontend `coderStream.ts` translates events into reactive
  state updates on `CoderPanel`'s message list.

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
container** when the active folder is a devcontainer, on the
host otherwise. The panel header surfaces the active target as a
small `host` / `container` pip next to the username.

What ships:

- `tools::bash` checks the active folder's
  `WorkspaceFolder.host`. `Local` → `tokio::process::Command::new("sh") -lc <cmd>`
  rooted at the folder, exactly as before. `Devcontainer` →
  `docker exec -w <container_cwd> <name> sh -lc <cmd>` against
  the workspace shell container compose already brought up.
- Reuses `moon_terminal::container_name_for_workspace` and
  `TerminalTarget::container_cwd_for_folder` so the framing
  matches terminals + LSP. No new trait method on
  `WorkspaceHost`; one is justified once a second host
  implementor (`RemoteHost`/`ContainerHost`) lands.
- `CoderStatus.bash_target: "host" | "container" | null` mirrors
  the bash tool result's `target` field. Status re-probes when
  the active folder switches so the pip stays fresh.
- Panel-header indicator pip in `CoderPanel.svelte`: subdued
  "host" border-only, accent-tinted "container" pip so the
  boundary is impossible to miss.

### 6.3 — Sessions on disk + todo list tool

**Acceptance**: every prompt/turn is persisted to JSONL under
`<workspace>/.moon/agent-sessions/`. The panel sidebar shows a
session list, "+ New session" creates one, clicking a session
opens it, deleting confirms-then-removes. `last_coder_session`
round-trips through `AppState`. The agent has a `todo_write`
tool whose state is part of the session and renders as a sticky
checklist in the panel.

What ships:

- JSONL writer in `crates/moon-coder/src/sessions/` with the
  header line + append-only events from
  [`coder.md`](../coder.md#sessions).
- `coder_list_sessions`, `coder_open_session`,
  `coder_delete_session` Tauri commands.
- `CoderSessionList.svelte` with the same sticky / scroll
  treatment as the Slack panel's session list.
- `AppState.coder = { last_session_id, sync_disabled_workspaces:
[] }` slice. The `sync_disabled_workspaces` list is wired in
  6.7 but the field exists from 6.3 to keep schema additions
  monotonic.
- `todo_write` tool per
  [`coder.md`](../coder.md#todo-list-tool). State lives in the
  session struct; replayed from JSONL when an old session is
  reopened. Sticky `CoderTodoList.svelte` widget at the top of
  the transcript.

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
