# ADR 0009 — Terminal: portable-pty + host/container targets

Date: 2026-05-03
Status: accepted

## Context

[Phase 3](../roadmaps/phase-03-terminal.md) adds PTY-backed
terminals to the bottom panel. Three orthogonal questions:

1. **Where does the terminal process run?** The user has
   both a host machine and a workspace container
   (`moon-ws-<id>-dev-1`, the `dev` service from
   [Phase 2](../roadmaps/phase-02-containers.md)). A naive
   IDE picks one. Real workflows want both:
   - Run a `git push` against a host SSH agent,
   - then `cd /workspace/moon-landing && bun test`
     against the container's toolchain,
   - in two terminals, side by side.
2. **What library does the PTY?** Cross-platform PTY is
   surprisingly involved (Windows ConPTY vs Unix `posix_openpt`
   / `grantpt` / `unlockpt` / `ptsname`).
3. **How does the lifecycle interact with the workspace
   container, which can pause / stop / be recreated?**

## Decision

### Two terminal targets, fixed at open time

The terminal tab carries an immutable `target` that's one of:

```rust
pub enum TerminalTarget {
	Host { cwd: Option<Utf8PathBuf>, shell: String },
	Container { container_name: String, cwd: Utf8PathBuf, shell: String },
}
```

- `Host`: spawn `shell` directly on the user's machine.
  `shell` is `$SHELL` from the user's environment with
  `/bin/bash` fallback. `cwd` is the active folder's
  absolute path; `None` means start in the user's home
  directory.
- `Container`: spawn `docker exec -it <container_name>
<shell>` on the host. portable-pty allocates a host-side
  PTY; `docker exec -it` allocates a TTY for the
  in-container process and bridges through. SIGWINCH
  propagates correctly. `shell` is hardcoded `bash` (it's
  in `moon-base`); `cwd` is the in-container mount path
  (`/workspace/<basename>` for a bound folder).

A terminal opened against the host stays a host terminal
even if the workspace container later starts. A terminal
opened against the container dies when the container goes
away (Stop / Down / Recreate); the user opens a fresh tab
to talk to the new container. We deliberately don't try to
"reattach" — `docker exec`'s lifetime is the user's mental
model and matching it keeps the implementation honest.

### portable-pty for the PTY layer

[`portable-pty`](https://docs.rs/portable-pty) (from the
Wezterm project, BSD-licensed) handles the OS PTY
differences and exposes a `MasterPty` (read/write the
terminal "wire") + `Child` (the spawned process).

Why portable-pty over alternatives:

- **`pty_rs` / `pty-process`** are Linux/macOS only — no
  Windows ConPTY. moon-ide doesn't ship for Windows yet,
  but the workspace **already** crosses that boundary
  (Slack runs on Windows hosts), so closing it now costs
  nothing.
- **Direct syscalls (libc + nix)** is two screens of code
  per platform plus a maintenance burden; portable-pty
  is one screen of `CommandBuilder` use.
- **`tokio_pty` / bollard's exec** would couple us to
  Docker for the container case (no host case) and
  lose the unified PTY abstraction.

portable-pty is sync, but each PTY runs on its own
`spawn_blocking` task that pumps bytes into a `tokio::sync::mpsc`
channel; the supervisor task on tokio sends those bytes
out via `Tauri::emit`. Same pattern as the existing
`compose_logs` supervisor.

### Wire format mirrors compose_logs

Four Tauri commands + two events, all keyed on a UUID
`stream_id` the frontend mints on the open call:

| Command           | Payload                              | Returns     |
| ----------------- | ------------------------------------ | ----------- |
| `terminal_open`   | `{ target, cols, rows }`             | `stream_id` |
| `terminal_write`  | `{ stream_id, data }` (base64 bytes) | `()`        |
| `terminal_resize` | `{ stream_id, cols, rows }`          | `()`        |
| `terminal_close`  | `{ stream_id }`                      | `()`        |

| Event             | Payload                                    |
| ----------------- | ------------------------------------------ |
| `terminal:output` | `{ stream_id, data }` (base64 bytes)       |
| `terminal:closed` | `{ stream_id, code }` (`code` may be null) |

The frontend stores the stream id alongside the
`BottomPanelTab` and can replay a write even after the
backend's `Child` has exited (which lets the body show
`[exited (N)]` even if the user reattaches focus to the
tab much later).

Bytes are base64-encoded for transport: PTY output is
arbitrary 8-bit (escape sequences, UTF-8 fragments mid-codepoint
on a `read(2)` boundary), and Tauri's IPC payload codec
is JSON. xterm.js `write()` accepts byte arrays so the
frontend `atob`s on the way back out.

### No persistence in 3.0

PTY state can't survive an IDE restart anyway — the
`Child` is gone, the in-memory scrollback is gone. We
deliberately don't persist tab metadata either: the cost
(re-spawning fresh shells on next launch with stale
titles) outweighs the value (typing `Ctrl+T` is fast).
Reconsider when someone reports a real workflow that
needs it.

## Consequences

### What's nice

- The host/container split is honest about which machine
  the user is talking to. The icon on each tab (monitor for
  host, container box for container) makes it unambiguous;
  mistakenly running `rm -rf` against the wrong target is
  harder.
- Container terminals reuse the existing workspace
  shell — there's no extra container, no extra image
  pull, no extra startup time. The first "In container"
  click after launching the IDE returns a prompt in
  hundreds of milliseconds.
- Multi-target generalises naturally to remote hosts
  (Phase 6+): a `TerminalTarget::Remote { workspace_id,
cwd }` variant that JSON-RPC-tunnels to a remote
  agent's PTY runner. Not in 3.0.

### What's not nice

- `docker exec` doesn't know about the workspace
  container's `restart` policy. If a user's process
  crashes the container and compose restarts it, every
  open container terminal dies and the user has to
  reopen them. Acceptable: container restarts during
  active dev are rare and the new shells take
  milliseconds to spawn.
- portable-pty's `Child` doesn't expose async exit;
  we poll `try_wait` from `spawn_blocking`. One thread
  per terminal at idle. For the expected ≤4 terminals
  this is fine; if it ever becomes a real cost we can
  switch to `tokio::process` for the host case (still
  through a PTY allocated by portable-pty) and accept
  the ConPTY edge case being slightly different.
- We don't surface the host shell's exit code in any
  durable way — the tab title's `[exited N]` suffix goes
  away when the tab is closed. That's fine for now;
  most "what was the exit code?" needs are scratched
  by the shell's own prompt.

## Alternatives considered

### One terminal type, picked dynamically

"Always run terminals in the container if it's up,
otherwise on host." Considered and rejected:

- Confusing when the container starts mid-session: do
  existing terminals migrate? (No good answer.)
- Loses the use case of "I want a host shell to run
  `git push` with my SSH agent even though the
  container is up." That's a real workflow today.

### Run a Wezterm-style multiplexer in moon-base

Spin up a `tmux` server in the workspace container and
have moon-ide attach windows. Gives free persistence
across IDE restarts.

Rejected for 3.0: scope creep, and the mental model
(every terminal is a tmux window in the same server) is
weird if you don't already use tmux. If we want
persistence later we can layer this in without changing
the wire format — the host side stays a `docker exec`,
just to `tmux attach -t <window>` instead of `bash`.

### Bollard's exec API for container terminals

Use `bollard::exec` directly instead of shelling to
`docker exec`. Pros: pure Rust, no docker CLI required.
Cons: pulls in hyper/h2 transitively (per
[ADR 0007](0007-compose-and-moon-base.md#why-shell-out-to-compose),
we already chose shell-out for compose), requires its
own SIGWINCH bridging, doesn't share the auth /
context handling that `docker` already does for free.
Stay shell-out for symmetry.

## Notes

- The `docker exec -it` invocation requires `-i` (stdin)
  and `-t` (TTY). portable-pty's `CommandBuilder.args`
  carries them through; the host PTY provides the
  in-container TTY.
- `TERM=xterm-256color` is set in the spawned env on
  both targets so prompts and TUIs render correctly.
- The container target's `container_name` is derived
  from the workspace id at open time, not stored — if
  the workspace id ever changes (it can't today) the
  command path would update on the next open.
