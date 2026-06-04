# ADR 0026 — Mount the focus socket's directory, not the socket file

Date: 2026-06-04
Status: accepted; supersedes the bind-mount mechanics in
[ADR 0021](0021-git-editor-forward.md) (decision point 3 and the
"Bind the whole `<workspaces_dir>/<slug>/` directory" rejection).
Amends [`specs/containers.md`](../containers.md) § "Editor
forwarding" and the socket layout in
[ADR 0014](0014-process-per-workspace.md).

## Context

ADR 0021 forwards `$GIT_EDITOR` from container terminals to the
host IDE over the per-workspace focus socket, and to do that it
**bind-mounts the socket file itself** into the dev container:

```yaml
- <workspaces_dir>/<slug>/instance.sock:/run/moon/instance.sock
```

ADR 0021 justified this with a "free guarantee": the socket is
bound pre-Tauri, so "the file exists at `docker compose up` time,
every time." That guarantee is false in two real situations, and
both bite hard:

1. **Container outlives the IDE.** The focus socket's lifetime is
   tied to the IDE process — it's bound on startup and unlinked on
   clean exit (`focus_socket::cleanup`). The dev container (and
   especially the long-lived `ports` sidecar) routinely outlives
   the IDE. If anything runs `docker compose up` for the workspace
   while the IDE is **not** running — a restart policy, a manual
   `up`, a sidecar recreate — Docker finds the bind-mount source
   missing and **auto-creates it as a root-owned directory**.

2. **Once that root-owned directory exists, the workspace is
   bricked.** On the next launch `UnixListener::bind` fails with
   `AddrInUse` (the path exists), the stale-socket probe can't
   connect (it's a directory), the old recovery did
   `remove_file` (fails — it's a directory, not a file), the
   rebind fails again, and the launcher misreads the bind failure
   as "another instance owns this workspace." It sends a focus
   message that also fails, logs "failed to focus existing
   window," and exits without ever showing a window. The user
   cannot open the workspace again without `sudo rm` on a path
   they've never heard of.

This actually happened (the `hugging-face` workspace), which is
what prompted this ADR.

Even setting aside the root-owned-directory failure, bind-mounting
a **socket file** is the wrong shape for a socket whose inode is
recreated. Docker resolves the source to an inode at container
start. When the IDE restarts (unlink + rebind → a new inode), the
container's mount still points at the old, now-unlinked inode, so
editor forwarding silently breaks until the container is recreated
— a latent bug independent of the root-dir crash.

## Decision

**Mount the socket's parent directory, not the socket file.**

1. The focus socket moves from `<workspaces_dir>/<slug>/instance.sock`
   to `<workspaces_dir>/<slug>/run/instance.sock` — a dedicated
   `run/` subdirectory that holds the socket and nothing else.

2. The dev container bind-mounts that **directory**:

   ```yaml
   - <workspaces_dir>/<slug>/run:/run/moon
   ```

   read-write (a read-only directory mount makes the socket inside
   it unreachable, since `connect()` needs write on the inode).
   The in-container socket path is unchanged —
   `/run/moon/instance.sock` — so `moon-edit`, `$MOON_EDIT_SOCK`,
   and the per-`docker exec` env injection from ADR 0021 are
   untouched.

3. moon-ide guarantees the `run/` directory exists, user-owned,
   before any compose call: `focus_socket::try_bind` creates it
   pre-Tauri (via `create_dir_all` on the socket's parent), and
   the lifecycle layer's `write_state` re-creates it right before
   `docker compose up`. The directory persists across IDE restarts
   (only the socket file inside it is unlinked on exit), so Docker
   never needs to auto-create the mount source.

4. `try_bind` is hardened to recover from **any** non-socket
   debris at the lock path, not just a stale socket file: it now
   stats the entry and `remove_dir_all`s a directory (or
   `remove_file`s a file) before rebinding. Removal is governed by
   the _parent_ directory's permissions, which moon-ide owns, so
   even a root-owned empty directory unlinks cleanly. This is
   belt-and-suspenders: with the directory mount in place the
   root-owned-directory case should no longer occur, but a single
   bad state should self-heal on the next launch rather than
   require manual `sudo`.

### Why this reverses ADR 0021's "bind just the file" call

ADR 0021 deliberately mounted only the socket file and explicitly
rejected mounting the directory, on two grounds. Both are
addressed:

- _"Wider surface than the socket needs."_ The rejected option was
  mounting the **whole** `<slug>/` state dir, which carries
  `compose.yaml` (containing the host `gh` token!), `session.json`,
  and `bound-folders.json`. The `run/` directory we mount here
  holds **only** the socket — the container sees nothing it didn't
  already get with the file mount, plus the ability to create or
  delete entries in a directory that contains a single throwaway
  socket.

- _"`:ro` would break socket connect."_ True, and irrelevant: this
  mount is read-write by design, same as the file mount it
  replaces.

### Residual surface, accepted

A read-write directory mount lets the container create, delete, or
replace files inside `run/` — including unlinking the host's live
socket. The blast radius is a denial-of-service on that one
workspace's editor forwarding / single-instance lock (the IDE
re-creates the socket on next launch; `try_bind` clears debris).
The container cannot escalate to the host, cannot reach any other
workspace, and cannot read host state — `run/` contains only the
socket. For a container running the team's own dev tooling (ADR
0008's threat model treats it as a boundary, not as hostile), this
is an acceptable trade for a mount that can't silently rot. It is
the standard pattern for mounting a Unix socket whose lifetime is
shorter than the container's.

## Consequences

- **Socket path changes** from `<slug>/instance.sock` to
  `<slug>/run/instance.sock`. Per the repo's "no premature
  migrations" rule (AGENTS.md), no compat shim: the two
  `socket_path` helpers (`src-tauri/src/focus_socket.rs` and the
  bridge's `crates/moon-bridge/src/discovery.rs` mirror) are
  updated in lockstep, and any workspace still holding a socket at
  the old path simply re-binds at the new one on next launch.
- **Bridge discovery** (ADR 0023 / phase 13) enumerates
  `<slug>/run/instance.sock` instead of `<slug>/instance.sock`.
  Enumeration still walks the `<slug>/` directories; only the
  socket subpath moved.
- **compose.yaml** gains a directory mount
  (`…/run:/run/moon`) in place of the file mount. `MoonEditSocketMount`
  now carries the host `run/` directory rather than the socket
  file path.
- **No change** to `moon-edit`, the editor-forward env vars, or
  the in-container socket path — all still `/run/moon/instance.sock`.
- **Self-healing.** A pre-existing root-owned directory left by an
  older build is cleared automatically on the next launch.
