# Test plan 0016 — terminal basics (Phase 3.0)

Manual end-to-end checks for PTY-backed terminals in the
bottom panel.

Architecture context:

- [Phase 3 roadmap](../roadmaps/phase-03-terminal.md).
- [ADR 0009](../decisions/0009-terminal-pty-and-targets.md).
- [containers.md § Terminals](../containers.md#terminals).

## Setup

1. Daemon ready: `docker info` succeeds.
2. Built `moon-base:dev` locally and the workspace container
   is running (status pip green). At least one bound folder.
3. moon-ide running via `bun run dev` (or release build).

## A. Host terminal basics

A.1. Click the terminal icon in the status bar (right of
the container pip). Popover shows two options: **On host**
and **In container**, both enabled.

A.2. Click **On host**. Expectation:

- Bottom panel becomes visible if it wasn't.
- A new tab appears with the host (monitor) icon and a title
  equal to the active folder's basename (e.g. `moon-landing`).
  Hovering the tab shows `host: <full-cwd>`.
- The shell prints a prompt within ~1 s. `pwd` returns the
  active folder's path.

A.3. Type `ls`. Output renders correctly with colour (if
the user's `$LS_COLORS` is set), Unicode, and tabs. Long
lines wrap to the next line.

A.4. Resize: drag the bottom panel taller. The shell's
view of `$LINES` updates (`echo $LINES` reports the new
row count after a redraw — try `tput lines`).

A.5. Type `exit`. The shell terminates. The tab title gains
an `[exited 0]` suffix in the warning colour, and input is
no longer forwarded. Closing the tab via the × removes it
from the strip.

## B. Container terminal basics

B.1. With the workspace container `running`, click the
status-bar terminal icon → **In container**.

B.2. Expectation:

- New tab with the container icon (accent colour matches
  the workspace pip) and a title equal to the active folder's
  basename. Hovering the tab shows
  `container: /workspace/<basename>`.
- Prompt comes back from the in-container shell. `pwd`
  reports `/workspace/<basename>`. `whoami` reports the
  in-container user (`moon` from moon-base).
- `cat /etc/os-release` shows Debian, confirming we're
  inside the container.

B.3. From the container terminal, run a build/test that
exercises the moon-base toolchain (e.g. `bun --version`,
`cargo --version`).

## C. Multi-terminal

C.1. With one host and one container terminal open, click
**+ Terminal** in the panel strip (right side). Popover
appears with the same two options.

C.2. Open another host terminal. Three tabs in the strip;
the new one becomes active.

C.3. Click between tabs:

- Each tab's xterm scrollback is preserved across switches
  (no clear / reflow on activation — the panel keeps every
  body mounted, hidden ones use `display: none`).
- Resizing the panel resizes whichever tab is active; the
  inactive ones refit when they're next selected.

C.4. Type a long-running command in tab 1 (`yes | head -n
100000` or similar). Switch to tab 2. Switch back: the
output is there, and `Ctrl+C` interrupts cleanly.

## D. Container disabled when shell is down

D.1. Stop the workspace container (status-bar pip → Stop).

D.2. Open the terminal launcher (status bar or panel
strip). Expectation:

- **On host** stays enabled.
- **In container** is disabled with tooltip "Workspace
  container is not running. Start it from the status
  bar."
- Existing host terminals keep working.
- Existing container terminals show `[exited 137]` or
  `[exited 143]` in the tab title — the `docker exec` died
  with the container.

D.3. Restart the workspace container. Open a new container
terminal — it lands cleanly. Existing dead container tabs
stay dead until closed.

## E. Resize / cwd edge cases

E.1. Open a host terminal with **no active folder** (close
all bound folders first if needed). Expectation:

- Prompt lands in `$HOME` (typically `~`).
- Tab title is `~`. Hover shows `host: ~`.

E.2. Open a container terminal with no active folder.
Expectation:

- Prompt lands in `/workspace`.
- Tab title is `workspace`. Hover shows `container: /workspace`.

E.3. With a terminal open, switch the active folder via
the folder bar. Expectation:

- Existing terminals **don't migrate** — their cwd is
  fixed at open time. The tab title and hover both still
  show the original cwd basename / path.
- Opening a _new_ terminal uses the new active folder's
  cwd.

## F. Lifecycle: IDE quit

F.1. Open three terminals (mix of host and container).

F.2. Quit moon-ide.

F.3. From a separate shell, before the IDE finishes
exiting:

- `pgrep -af "docker exec"` shows the container terminal's
  exec children getting killed (briefly visible, then
  gone).
- After the IDE exits, `pgrep -af "docker exec"` returns
  nothing — no orphaned terminals.
- `pgrep -af bash | grep -v grep` doesn't show the host
  terminal's shells either (they were direct children of
  the IDE process; SIGKILL via `PtySession::drop`).

F.4. Re-launch the IDE. The bottom panel shows no terminal
tabs (no persistence, by design). The workspace container
auto-resumes per the existing shutdown story.

## G. Failure modes

G.1. **Bad container target**: with the workspace
container down, force a container open (e.g. via the
palette later, or by patching the disabled state).
Expectation: the open call fails; the tab still mounts
showing "Failed to open terminal: …" so the message is
visible. Closing the tab works.

G.2. **Daemon down**: stop the docker daemon (`sudo
systemctl stop docker`). Existing container terminals
freeze (no signal yet); closing the tab still works
(SIGKILL reaches the host-side `docker exec` regardless of
daemon state). New container terminals fail to open with
a docker-CLI error in the body. Restore the daemon and
host terminals are unaffected.

G.3. **Window torn down mid-session**: kill -9 the IDE
process (don't use the close button). Containers and
host shells are reaped via the kernel (parent dead → init
inherits → SIGCHLD handling), and `pgrep` shows them gone
within a few seconds. There may be a brief window where
they're orphaned to PID 1; this is the documented escape
hatch (see the `containers.md` shutdown section).

## What must keep working

- Compose log tabs (`LogTab.svelte`) still receive lines
  and follow tail correctly — they share the same panel
  but use a different event channel.
- `Ctrl+J` (or the panel toggle) hides/shows the panel
  without dropping terminal state.
- Workspace container Stop / Recreate / Down all behave
  the same as before — this PR only added the terminal
  abort step at the _front_ of `stop_all`, the rest of
  the shutdown sequence is unchanged.
- IDE auto-resume on next launch still works exactly as
  documented in test plan 0015 — auto-resume runs on the
  workspace container, not on terminals (which are gone
  by design).

## Known limitations (3.0)

- No persistence across IDE restart. Terminals close on
  quit; reopen is manual.
- No splits — multiple terminals share one panel via tabs
  only.
- No "Open terminal here" from the folder bar.
- No search-in-scrollback (xterm-addon-search).
- Hardcoded `bash` for container terminals; `$SHELL` for
  host terminals.
