# ADR 0095: Remote workspaces — the desktop as a front end to a server

Status: accepted. Supersedes the `RemoteHost` plan in
[ADR 0002](0002-workspace-host.md) / [ADR 0011](0011-rename-moon-agent-to-moon-remote.md);
extends [ADR 0059](0059-headless-enrolled-ide.md).

## Context

The user works on two desktop machines and wants to start agent work
on one and pick it up on the other. The work is mostly steering agents
and reviewing, but also uses terminals (long-running commands), dev
servers previewed in the in-IDE browser tab or a system browser, and
dev containers. A third machine can stay on as a server.

Syncing sessions between the desktops was rejected (see below): the
transcript is the easy part, but the code state, containers,
terminals, background processes and dev servers can't follow a
session from one machine to another.

## Decision

- **A workspace can be remote.** Its whole workspace process —
  registry, coder loop and sessions, containers, terminals, LSP, git,
  fs watcher, browser-tab preview proxy — runs on the server as
  `moon-remote serve`, which outlives any desktop connection. There is
  one copy of everything, so there is nothing to sync, and in-flight
  turns survive a laptop closing. Remote is a per-workspace choice;
  local workspaces are unchanged.
- **The desktop is a front end.** A remote workspace window runs the
  same Svelte UI; its core calls go over the network instead of to an
  in-process backend. The workspace process's command surface becomes
  **one name-dispatched JSON-RPC surface** with event push through an
  event sink, served in-process by Tauri for local workspaces and by
  `moon-remote` for remote ones — one implementation, so the two can't
  drift. Desktop-shell actions (windows, native dialogs, opener,
  clipboard, focus) stay local Tauri commands in both modes.
- **Transport is the system OpenSSH client.** The desktop holds one
  multiplexed connection to the server (`ControlMaster`):
  `moon-remote attach --workspace <slug>` over it bridges stdio to the
  long-lived workspace process (launching it if needed). SSH provides
  auth, encryption, `~/.ssh/config` / agent / host aliases, and the
  extra channels below for free. The relay (ADR 0031) stays the
  phone's path; desktops don't use it.
- **Browser tabs and port forwards tunnel explicitly.** The preview
  proxy runs on the server; the desktop opens a loopback listener per
  tab target and forwards it over the SSH connection. Each tunnel is
  requested by a tab or a declared forward, never by scanning ports
  (invariant 3).
- **File transfer is explicit.** Drag-and-drop onto the file tree and
  an "Upload file…" menu entry upload; "Download to this computer"
  downloads. Transfers stream over the SSH connection (never a single
  base64 RPC), write to a temp file renamed into place, ask before
  overwriting, and land on the server's folder path, which the
  container sees through its bind mount. The same actions work on
  local workspaces as plain copies.
- **One desktop attached at a time, with takeover.** Attaching from a
  second desktop detaches the first (`tmux attach -d`), whose window
  shows a "detached — attach again" state. Per-window UI state
  persists in the workspace process, so the second desktop opens on
  the same tabs. The phone keeps its observe-only access alongside.

## Rejected alternatives

- **Session sync** (bidirectional bucket sync, or explicit handoff
  with a pushed WIP ref). Moves the transcript, not the environment:
  containers, terminals, dev servers and background processes stay
  behind, and automated sync additionally needs conflict handling for
  transcript truncation and in-place header rewrites. Manual handoff
  is the right shape only for two machines with no server; we have
  one.
- **`RemoteHost`** (core on the desktop, `WorkspaceHost` over the
  network). Keeps the coder loop on the laptop, so turns die when the
  lid closes, and makes every fs / git call a round-trip from the
  core. Moving the whole process is simpler and strictly more useful.
- **Desktops through the relay.** Needs both ends enrolled, adds a
  public hop to terminal latency, and has no channel for bulk file
  transfer or port tunnels. SSH already reaches the server.
- **An SSH library in-process.** Re-implements config parsing, agent
  forwarding, `ProxyJump` and known-hosts handling the system client
  already does correctly.
- **Concurrent multi-desktop attach.** The frontend owns per-window
  state (active folder, open tabs, saved session); two writers would
  fight. Revisit if someone actually needs two live desktops.
