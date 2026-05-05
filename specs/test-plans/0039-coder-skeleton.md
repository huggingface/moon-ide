# Test plan 0039: Coder skeleton (Phase 6.0)

- **Date**: 2026-05-05
- **Phase**: Phase 6.0 — Coder

## What shipped

- Renamed `crates/moon-agent/` → `crates/moon-remote/` (workspace
  member, binary name, source comments). Frees the `moon-agent`
  name; the new AI coding agent crate is `moon-coder`. Per
  [ADR 0011](../decisions/0011-rename-moon-agent-to-moon-remote.md).
- New crate `crates/moon-coder/` owns the agent loop, the HF OAuth
  device flow + token refresh, the HF Inference Providers HTTP
  client (non-streaming), the tool registry, and the in-memory
  session. Per [ADR 0010](../decisions/0010-coder-rewrite-not-acp.md)
  and [`coder.md`](../coder.md).
- Right-side `CoderPanel.svelte` with three states: not signed in
  ("Sign in with Hugging Face"), device-code modal showing the
  user code + verification URL, and signed-in chat. Composer is
  Enter-to-send, Esc aborts.
- Tauri command surface: `coder_start_device_flow`,
  `coder_status`, `coder_sign_out`, `coder_send`, `coder_abort`.
  Loop events stream on the `coder:event` Tauri channel.
- Read-only tool subset wired to the active `WorkspaceHost`:
  `read_file`, `list_dir`, `grep`, `bash`.
- Single in-memory session, "large" model hardcoded as
  `Qwen/Qwen3.5-397B-A17B:scaleway`. Sessions, model picker,
  streaming, mutating tools, and bucket sync land in 6.1+.

## How to test

Prerequisites:

- `bun install` and `cargo` toolchain per `README.md`.
- A working OS keyring (libsecret on Linux — `gnome-keyring` /
  KWallet / KeePassXC daemon running). Verify with
  `secret-tool lookup service moon-ide account hf-oauth` after
  signing in (will return the JSON blob if storage works).
- A Hugging Face account.
- Network access to `https://huggingface.co` and
  `https://router.huggingface.co`.

### 1. Build clean

```bash
bun run check          # tsgo + svelte-check pass
cargo check --all      # workspace builds
cargo clippy --all-targets -- -D warnings
```

Expected: no warnings, no errors. The renamed `moon-remote` crate
builds; `moon-coder` builds and is reachable from `moon-coder` →
`moon-protocol` → `moon-core`.

### 2. Sign in via HF device flow

1. `bun run dev`. The IDE opens; the editor is empty (or whatever
   was last open).
2. Click the **robot icon** in the status bar (right of the chat
   icon). The right-side coder panel opens. It should show
   "Sign in with Hugging Face" with a primary button.
3. Click "Sign in with Hugging Face". A modal appears with:
   - A short user code (8 chars, mono font, big).
   - "Open in browser" button.
   - "Cancel" link.
   - A subdued "waiting..." line at the bottom.
4. Click "Open in browser". The system browser opens at
   `https://huggingface.co/login/device` (or whatever HF returns
   as `verification_uri`).
5. On the HF page, paste / type the code, log in (or already be
   logged in), accept the scope grant for `inference-api` and
   `contribute-repos`.
6. Within ~5 seconds the IDE modal closes itself. The panel
   reads "Signed in as `<your-username>`" with the avatar pulled
   from `whoami-v2`. Below: a "+ New session" button or directly
   the active session view (this skeleton creates a session
   eagerly).

Expected: under the hood, the access + refresh tokens land in
the OS keyring under `service=moon-ide`, `account=hf-oauth`. If
the keyring is broken, you'll see the modal close and immediately
re-prompt — that's the "tokens didn't persist" surface.

### 3. Send a turn against the large default

1. With the panel open and a folder bound, type a prompt that
   forces a tool call:

   > Read the file `package.json` and tell me the project name.

2. Press Enter. The composer disables and a `[user]` bubble
   appears immediately. Within a few seconds:
   - An `[assistant]` bubble streams in (non-streaming this
     phase, so it lands all at once).
   - A `[tool] read_file` block appears with `path: package.json`
     in its expandable input section, and the file contents in
     the output section.
   - A second `[assistant]` bubble appears with the agent's
     answer.
3. Send a follow-up: `Now run "ls -la" in the workspace and
summarize what's there.`. The agent should call the `bash`
   tool — confirm:
   - The tool block shows `cmd: "ls -la"`.
   - The output is the actual `ls -la` output of the active
     folder.

Expected: every tool call is rendered as its own collapsible
block with `args` and `result` panes. Errors thrown by tools
(try `Read /etc/passwd` — outside the workspace) come back as
red `isError: true` blocks; the agent is allowed to retry or
explain.

### 4. Abort mid-turn

1. Send a long-running prompt: `Run "sleep 30 && echo done" via
bash and wait for its output.`.
2. While the bash tool is running, press **Esc** with the
   composer focused. The pip should flip to idle within a
   second.
3. Confirm:
   - The bash subprocess actually died (`pgrep -f "sleep 30"`
     returns nothing).
   - The panel shows the partial assistant message + an
     `aborted` notice.
   - You can immediately send another prompt; the new turn
     starts cleanly.

### 5. Sign out and re-sign-in

1. Click the disconnect icon in the panel header. Confirm in
   the dialog. The panel returns to the "Sign in" empty state.
2. Verify the keyring entry is gone:
   `secret-tool lookup service moon-ide account hf-oauth`
   returns nothing.
3. Re-sign in (steps 2 above). Confirm a new code is issued and
   the flow completes.

### 6. Container-aware bash (skip if Phase 2 is off)

If you have a workspace with a running `moon-base` container:

1. Open that workspace and the coder panel.
2. Send `Run "uname -a" via bash.`. The output should match
   what `docker exec` would return inside the container, **not**
   the host's uname.
3. The panel header shows "running in container" pip.

If Phase 2 isn't running, the same test on a host-only workspace
must show "running on host" pip and the host's uname.

## What must keep working

- Slack panel: `chat:` icon still toggles `ChatPanel.svelte`;
  signing into HF doesn't disturb Slack auth.
- File tree, editor, git overlays: unaffected — the coder is
  additive, no changes to existing flows.
- `cargo check --all` succeeds with no `moon-agent` references
  (every spec/source mention is now `moon-remote` or
  `moon-coder`).
- The keyring continues to honour the existing `slack-user-token`
  entry (the new `hf-oauth` entry sits next to it under the same
  `moon-ide` service).
- F6 / Shift+F6 focus cycle still cycles through file tree →
  editor → bottom panel → chat panel (the coder panel slots in
  alongside the chat panel; both occupy the same focus role for
  now).
- `bun run check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --workspace` all green.

## Known limitations

- **Non-streaming.** SSE arrives in 6.1; right now the assistant
  bubble appears all at once after the LLM call returns.
- **Single in-memory session.** No JSONL persistence, no session
  list, no resume-on-launch. Lands in 6.3.
- **No model picker.** The "large" default is hardcoded; the
  "fast" default exists in the constants file but isn't wired
  to anything yet (sub-agents are deferred). Picker lands in
  6.4.
- **No mutating tools.** `write_file` / `edit_file` are
  deliberately out of 6.0 — the agent can read, list, grep, and
  bash, nothing else.
- **No steering / follow-up.** Pressing Enter mid-turn does
  nothing; queueing lands in 6.5.
- **No skill discovery / `AGENTS.md` injection.** System prompt
  is a fixed string in `defaults.rs` for this slice. Lands in
  6.6.
- **No bucket sync.** Sessions live only in process memory.
  Lands in 6.7.
- **Refresh-token rotation is best-effort.** A 401 retry covers
  most cases; the proactive 60s-before-expiry refresh is
  implemented but only stress-tested against synthetic
  expirations.
- **No "running in container" pip yet.** The container vs host
  routing already works (bash routes through the active
  `WorkspaceHost`), but the visual indicator lands with the
  panel polish in 6.2.

## Related

- Specs: [`coder.md`](../coder.md), [`coder.md` § Loop shape](../coder.md#loop-shape),
  [`coder.md` § Authentication](../coder.md#authentication),
  [`coder.md` § Tool surface](../coder.md#tool-surface),
  [`roadmaps/phase-06-coder.md`](../roadmaps/phase-06-coder.md).
- ADRs: [0010](../decisions/0010-coder-rewrite-not-acp.md),
  [0011](../decisions/0011-rename-moon-agent-to-moon-remote.md).
- Prior test plans: [`0008-slack-foundation.md`](0008-slack-foundation.md)
  is the closest analog (HTTP client + keyring + Tauri commands).
