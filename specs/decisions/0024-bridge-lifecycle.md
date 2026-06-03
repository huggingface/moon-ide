# ADR 0024 — Bridge lifecycle: IDE auto-starts it, it self-exits when idle

Date: 2026-06-21
Status: accepted; refines [ADR 0023](0023-mobile-companion-bridge.md)
(mobile companion via `moon-bridge`) and builds on
[ADR 0014](0014-process-per-workspace.md) (process per workspace,
per-workspace `instance.sock`).

## Context

[ADR 0023](0023-mobile-companion-bridge.md) made `moon-bridge` a
single host-resident daemon (one LAN port / cert / paired-device
list for the whole machine) that discovers workspaces by enumerating
their `instance.sock` files. It left _who runs it_ unspecified, so in
practice it was a binary the developer started by hand
(`moon-bridge serve …`).

That's the wrong user model. "I want my phone to reach my IDE" should
be satisfied by **running the IDE** — not by also remembering to
launch and babysit a second process in a terminal. But the bridge
can't simply _be_ the workspace process either: there are N IDE
processes (one per workspace, ADR 0014) and there must be exactly
**one** bridge per machine, or a phone would have to chase N ports
that appear and vanish as windows open and close.

So the question is: how does exactly one bridge come to exist when
any IDE is running, and stop existing when none is?

## Decision

**The IDE ensures the bridge; the bridge ensures it's a singleton and
ensures it exits when idle.** Two cooperating mechanisms, mirroring
the single-instance pattern ADR 0014 already uses for the
per-workspace socket:

### 1. Owner election via a machine-wide lock

The bridge binds its LAN listener (`0.0.0.0:53180`) as the election:
binding succeeds for the first starter and fails with `AddrInUse` for
any later one. A would-be second bridge that hits `AddrInUse` simply
exits 0 — a live owner already serves the whole machine. (This is the
same "bind = win, fail = someone's already there" shape as the
per-workspace `instance.sock`, just on a TCP port instead of a Unix
socket, because the port is the shared resource the phone connects
to.)

### 2. The IDE spawns the bridge on startup (best-effort, detached)

When a workspace process finishes setup, it spawns a **detached**
`moon-bridge serve --web-root <bundled-dist>` child and does not wait
on it. If a bridge is already running, that child loses the election
and exits immediately — harmless. If not, it becomes the owner. Every
IDE launch does this; the election guarantees at most one survives.

The child is detached (not parented to the spawning IDE) precisely so
the bridge outlives the particular window that happened to start it:
closing the IDE that spawned the bridge must not kill a bridge other
open IDEs still rely on.

### 3. The bridge self-exits when no workspace is live

The bridge already enumerates live `instance.sock`s for discovery.
It reuses that as an idle check: on a periodic tick (every 30 s, after
a 30 s startup grace period), if discovery finds **zero** live
workspaces, the bridge exits 0. No IDE has to tell it to stop;
"the last workspace closed" is observable from the same signal
discovery already reads. This closes the loop cleanly:

- First IDE launch → spawns bridge → bridge wins election, serves.
- More IDEs launch → each spawns a bridge → all lose the election, exit.
- IDEs close one by one → bridge keeps serving while ≥1 is live.
- Last IDE closes → its `instance.sock` is unlinked on exit
  (ADR 0014's `focus_socket::cleanup`) → bridge's next idle check
  sees zero live workspaces → bridge exits.

A new IDE launched later re-spawns the bridge, so the machine
self-heals back to "bridge running iff an IDE is running."

### Why spawn a child, not run the bridge in-process

We considered running the bridge as a task **inside** whichever IDE
process owns it. Rejected:

- **Lifetime coupling.** The bridge would die when _that_ IDE window
  closes, even if other windows are open. We'd then need a handoff
  protocol to migrate ownership to a surviving window mid-flight —
  exactly the kind of cross-process coordination ADR 0014 spent a
  whole decision avoiding. A detached child sidesteps it: the bridge's
  life is tied to "any workspace is live," not to one specific
  process.
- **Blast radius.** A bug in the LAN-facing listener (a panic on a
  malformed frame, a TLS edge case) would take down a real editor
  window with unsaved work. A separate process contains it.
- **The single binary already exists.** `moon-bridge` is built and
  shipped anyway; spawning it is a `Command::spawn`, the same
  primitive the launcher already uses to spawn workspace children
  (ADR 0014).

### Dev mode

Under `bun run dev` / `tauri dev` the debug build does **not**
auto-spawn the bridge (same reason the launcher runs inline in debug
— a forked child can't reach the vite dev server, and dev sessions
are single-workspace anyway). The developer runs `moon-bridge serve
--web-root companion/dist` by hand when testing the companion, which
stays the documented dev affordance. Auto-start is a release-build
behaviour.

### The manual `serve` command stays

`moon-bridge serve` remains the entry point — auto-start just means
the IDE runs it for you in release builds. It keeps its flags
(`--bind`, `--advertise-host`, `--no-pairing`, `--web-root`) for the
dev/debug path and for anyone who wants to run the bridge explicitly.

## Consequences

- Running any release IDE makes the companion reachable; closing the
  last one stops the bridge. No manual process management.
- One new owner-election path in `moon-bridge serve` (bind-or-exit)
  and one idle-watcher task. Both lean on machinery that already
  exists (the listener bind; the discovery enumeration).
- The IDE gains a best-effort `Command::spawn` at the end of setup
  (release only). Both build paths place the `moon-bridge` binary +
  the companion PWA where `ensure_bridge_running` looks:
  - **Bundled** (`bun run build`): tauri `bundle.resources` ships
    them under the app's resource dir as `bridge/{moon-bridge,
companion/}`; the IDE resolves it via `app.path().resolve("bridge",
BaseDirectory::Resource)`. We bundle the binary as a **resource**
    and spawn it ourselves (detached `std::process::Command`) rather
    than via tauri's sidecar API, because the sidecar API ties the
    child's lifetime to the app process — the exact coupling this ADR
    avoids. Resources can lose the exec bit on copy, so the IDE
    `chmod +x`es the binary before spawning.
  - **`--no-bundle`** (`bun run build:bin`, the team's path): the
    bridge + PWA are staged next to the exe in `target/<profile>/`.
  - `scripts/stage-bridge.mjs` (`prepare` before bundling /
    `exe-adjacent` after) builds `moon-bridge` and places both. The
    resource source dir keeps a tracked `.gitkeep` so tauri-build's
    resource-path validation passes on a fresh checkout before the
    script populates it.
- **The bridge port is now an implicit machine-wide singleton.** If
  another app squats `53180`, the bridge loses the election and the
  companion silently doesn't come up. Acceptable for an internal tool
  on the team's own machines; a port-in-use diagnostic in the IDE's
  logs covers the rare collision.
- A brief window exists where the last IDE has closed but the bridge's
  idle tick hasn't fired yet (≤30 s). The bridge serving a workspace
  list that's gone empty
  for a few seconds is harmless — `call`s just fail with "no live
  owner" and the phone shows the empty switcher.

## Alternatives considered

- **Bridge as a task inside the owner IDE process.** Rejected above:
  lifetime coupling + blast radius + a handoff protocol we don't want.
- **A systemd/launchd user service.** Real "always on," but it's
  platform-specific install machinery for a tool whose whole point is
  "reachable while I'm working," not "always up." Auto-start-on-IDE +
  self-exit-when-idle matches the actual need without an installer.
- **IDE tells the bridge to stop (explicit shutdown IPC).** Redundant:
  the bridge can observe "no live workspaces" from the discovery
  signal it already reads, so an extra teardown message would be a
  second source of truth to keep in sync. Self-exit on the idle check
  is simpler and also handles the crash case (an IDE that dies without
  cleanup still leaves a stale socket, which `probe_alive` rejects, so
  the bridge still sees it as not-live).
- **Reference-count workspaces explicitly (each IDE pings the bridge
  on start/stop).** More moving parts than enumerating sockets, and it
  breaks on an IDE crash that skips the stop-ping. Discovery is the
  robust signal.
