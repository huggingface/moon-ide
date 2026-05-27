# ADR 0021 — Forward `$GIT_EDITOR` / `$EDITOR` from container terminals to the IDE

Date: 2026-06-07
Status: accepted; amends
[`specs/containers.md`](../containers.md) (new "Editor forwarding"
section) and extends the focus-socket protocol documented in
[`src-tauri/src/focus_socket.rs`](../../src-tauri/src/focus_socket.rs).

## Context

Plenty of routine git operations spawn `$GIT_EDITOR` (falling back
to `$EDITOR`, then `$VISUAL`) and block until that editor exits:
`git commit` without `-m`, `git commit --amend`, `git rebase -i`,
`git rebase --continue` on a pending message, `git tag -a`. The
same env-var dance covers `gh pr create` / `gh pr edit`,
`crontab -e`, and anything else that respects POSIX-editor
convention.

Inside a workspace shell container today this just fails:

- `moon-base` doesn't ship `nano`, `vim`, or any other CLI editor.
- moon-ide itself runs on the host (specs/containers.md § "What
  runs where"), with no in-container companion process — that's
  load-bearing for the unprivileged-container threat model in
  ADR 0008.

So `git commit --amend` from a container terminal dies with
`error: cannot run editor: No such file or directory`. The team
hits this often enough that it's the single most disruptive paper-
cut of the container workflow.

We considered three shapes before picking one:

1. **Ship `vim` or `nano` in `moon-base`.** Works, but doubles down
   on "two editors are open during a commit" — the user is sitting
   at moon-ide and now has to context-switch to a TUI in a
   terminal pane. Misses the point of having a real editor right
   there.
2. **Mount the host's `git` config into the container and set
   `core.editor` to a host-side command.** Doesn't work — the
   container can't exec host binaries, and the `core.editor`
   command has to be runnable from the container's namespace.
3. **Forward via the IDE.** A small CLI shim inside the container
   speaks to the running IDE process over an IPC channel, the IDE
   opens the file as a normal buffer, the shim blocks until the
   user finishes editing.

(3) is the only shape that lets the user edit a commit message in
the same editor they already use for everything else, without
introducing a second long-lived process inside the container.

## Decision

Forward editor invocations from container terminals to the host
IDE by:

1. **Extending the per-workspace focus socket** with an `E\n`
   message kind that takes a host-absolute path and blocks until
   the IDE replies `OK\n` (user saved + finished) or `CANCEL\n`
   (user closed the tab without finishing). The existing `F\n`
   focus message stays untouched. Both the protocol byte tags and
   the framing helpers live in a new `moon_protocol::focus_socket`
   module — same shape as every other wire-format the IDE owns.

2. **Shipping a tiny `moon-edit` Rust binary in `moon-base`** at
   `/usr/local/bin/moon-edit`. It's the only piece in this design
   that runs inside the container, and it has no listener — `git`
   spawns it, it speaks the protocol, it exits.

3. **Bind-mounting the host's `instance.sock` into the dev
   service** at `/run/moon/instance.sock` (read-write — Unix
   sockets need write permission on the inode to `connect()`).
   The mount is just the socket file, not its parent directory:
   the only thing the container can do with that write capability
   is speak the protocol the host process implements, which is
   the surface we want to expose anyway.

4. **Injecting the editor env vars per-`docker exec`**, not at
   compose level. The terminal supervisor (`crates/moon-terminal`)
   already builds every container terminal as
   `docker exec -it -e TERM=… …`; we add
   `-e GIT_EDITOR=moon-edit -e EDITOR=moon-edit -e VISUAL=moon-edit
-e MOON_EDIT_SOCK=/run/moon/instance.sock
-e MOON_EDIT_PATH_MAP=<container>=<host>:…`. Anything launched
   from outside the IDE (e.g. a user manually `docker exec`-ing
   into the container from a host terminal) sees no `MOON_EDIT_*`
   vars and falls back to git's "no editor" error — which is the
   correct behaviour for an out-of-band session.

The shim resolves `argv[1]` to an absolute container path against
`$PWD`, walks `$MOON_EDIT_PATH_MAP` for the longest matching
container prefix, swaps it for the corresponding host prefix, and
sends the translated host path to the IDE. The IDE then opens the
file through the existing `Workspace.openHostFile` machinery
(`OpenFile.isExternal = true`, no LSP / editorconfig / git
indexing — a commit message buffer shouldn't show up in `git blame`
or get tracked by tsgo). One new field, `OpenFile.pendingEdit:
EditId | null`, drives a tab-strip "Finish editing" affordance.

### Why the IDE's existing per-workspace socket

`<workspaces_dir>/<slug>/instance.sock` already exists (single-
instance lock + focus IPC, see ADR 0014 — process per workspace).
It's keyed by workspace and bound pre-Tauri, before any compose
call runs. That gives us a free guarantee for the bind mount: the
file exists at `docker compose up` time, every time. No setup
ordering work, no "what if the socket isn't there yet" race.

The focus socket's docstring already calls out that the protocol
is extensible "(e.g. 'open file' from a CLI handoff in a future
phase)". This is that future phase.

### Why a bash script + `ncat`, not a Rust binary

The first draft of this ADR had `moon-edit` as a static Rust
binary built from a new workspace member crate that shared the
`moon_protocol::focus_socket` framing helpers with the host
listener. It worked, but the Dockerfile cost was outsized: a
multi-stage build with a separate `rust:bookworm` stage, a
synthesised standalone `Cargo.toml` so the build context didn't
need every workspace crate, and a build-stage cache that
invalidated on every workspace dependency change.

The protocol is two write lines and one read line. Bash plus
`ncat -U` (Nmap's Unix-socket-aware netcat, one apt package)
covers it in ~80 lines, with no build step. The shim ships as
`COPY images/moon-base/moon-edit /usr/local/bin/moon-edit` —
plain text, easy to read, easy to audit.

The cost is one duplication: the wire format constants
(`"E\n"`, `"OK\n"`, `"CANCEL\n"`) live both in
`moon_protocol::focus_socket` (the host's encoder/decoder) and
in the shim. We accept it because the protocol is so small —
two tags and a single line of body — that the duplication is
trivial to keep in sync, and the moon-base CI smoke test
catches any drift via the test plan's end-to-end pass.

### Why a per-`docker exec` env injection, not compose

Compose-level `environment:` would set `$GIT_EDITOR=moon-edit` for
every command anything ever runs in the dev container — including
the LSP broker's `docker exec rust-analyzer`, the linter / format-
on-save dispatch, and any future agent tool that shells in. Most
of those have no IDE to forward to (no tab, no user) and any of
them that happens to spawn `git commit` internally would silently
hang waiting for a tab that's never going to be opened.

Per-`docker exec` injection scopes the forward to the surface that
makes sense: an interactive terminal the user opened. Programmatic
shells stay clean.

### Why the bind mount is read-write

A Unix-socket `connect()` requires write permission on the socket
inode. Read-only bind mounts make the socket unreachable from
inside the container. The protocol surface — what the container
can do with that write — is bounded by the listener: the IDE
parses `E\n<path>\n`, validates the path is plausible, and emits a
Tauri event. The container can't write arbitrary data to disk
through the socket; the only thing it can "write" is a request
the IDE chooses how to handle.

### Why scope to IDE-launched terminals only

The earlier sketch put `GIT_EDITOR=moon-edit` in compose, but the
edge cases were ugly: what if the user manually `docker exec`-s
in from a host shell before the IDE is up? What if a coder
agent's bash tool invokes `git commit --amend`? Scoping to the
terminal supervisor means we only forward when there's an actual
user sitting at a tab to receive the request. Other call paths
(coder agents that need to commit) can opt in later by setting
the env vars themselves; the protocol and shim are ready.

## Consequences

- **Adds one wire-format module** (`moon_protocol::focus_socket`)
  shared between `src-tauri` and the new `moon-edit` binary.
  Replaces the bare `const FOCUS_MESSAGE: &[u8] = b"F\n";` in
  `src-tauri/src/focus_socket.rs` with an enum + framing helpers
  on the protocol crate.
- **Adds one bind mount** to the rendered `compose.yaml` — just
  the workspace's `instance.sock`, read-write.
- **Adds four env vars** to `docker exec` invocations for
  container terminals: `GIT_EDITOR`, `EDITOR`, `VISUAL`,
  `MOON_EDIT_SOCK`, `MOON_EDIT_PATH_MAP`.
- **Bumps `moon-base`'s `COPY` posture** from "every tool comes
  from a remote installer" to "one bash shim ships from this
  repo". The previous convention (no `COPY` in the Dockerfile,
  everything fetched from upstream) was a coincidence rather
  than a rule; ADR 0007 doesn't forbid `COPY` and the script is
  small enough to audit in one screen.
- **No new persistent state.** A blocking edit lives entirely in
  RAM on the IDE side as a `oneshot::Sender<EditResult>` keyed by
  `EditId`. If the IDE quits mid-edit, the socket connection
  drops, the shim sees `EOF`, exits non-zero, and `git` aborts
  the commit (which is git's standard "empty editor" behaviour).

## Alternatives considered and rejected

- **`vim` / `nano` in `moon-base`.** Already covered — misses
  the point.
- **A separate "moon-edit" UDS per workspace, distinct from
  `instance.sock`.** No reason to maintain two single-purpose
  sockets when the existing one's protocol is explicitly
  designed to grow.
- **A WebSocket / TCP channel.** Would need port-forwarding
  configuration (ADR 0008's threat model frowns on that), and
  UDS is strictly cheaper for in-host IPC.
- **Bind the whole `<workspaces_dir>/<slug>/` directory.** Wider
  surface than the socket needs, and the natural `:ro` posture
  for "host state" would break socket connect — which would
  push us into per-file bind mounts anyway. The narrow socket
  bind is the right shape.

## Follow-ups

- The same protocol generalises to `code .`-style "open this
  path in moon-ide" without blocking. Adding an `O\n<path>\n`
  variant is a one-evening addition the next time someone wants
  it.
- A future "agent runs `git commit`" path can opt into the
  forward by setting `GIT_EDITOR=moon-edit` itself before it
  shells out; the agent presumably has a coder UI to receive the
  blocking edit request. That's a separate UX design and out of
  scope here.
