# ADR 0090: Agent-opened terminals

## Context

An agent that needs a dev server running (to check its work with
`open_browser`, ADR 0088) has two bad options: detached `bash`
(ADR 0034 / 0085) or asking the user to start it. A detached process
is the agent's private business — the user can only see it through a
tool row's live tail, can't type into it, and it dies with the session.
A dev server is the opposite: something the user wants to see, poke,
and keep after the agent is done. ADR 0048 already said that if a
coder-driven terminal were wanted, it should be the agent's own
terminal, not the user's.

## Decision

- **`open_terminal(command, title?, wait_for?, timeout_ms?)`** opens a
  normal terminal tab running `command`: an interactive shell on the
  session's `bash` side (host-mode override honoured) in the session's
  folder, with the command typed and executed (bracketed paste +
  Enter), so it lands in the shell's history and job control works.
  An agent terminal in the project already running the same command
  is returned instead (`reused`), mirroring `open_browser`.
- **One terminal set, mixed with the user's.** Same registry, same
  bottom-panel strip, same `read_terminal`, same persistence recipe.
  Agent terminals carry an `AgentTerminal { title, command }` marker:
  the tab shows the title and an `agent` badge, the tooltip shows the
  command, and the marker is persisted so a restored tab keeps it. On
  relaunch the command is prefilled, not re-run (ADR 0050 holds for
  agent terminals too).
- **Focus.** The panel is revealed; the new tab comes to the front
  unless focus is inside the bottom panel (the user is typing into
  another terminal).
- **Lifetime is the tab's**, not the turn's or the session's.
- **`terminal_tab(id, restart|close)`** controls terminals with the
  agent marker only, from any session in the project (the session
  that opened one may be long gone). `restart` replaces the shell
  under the same id and re-runs the command; the retained output is
  reset so a `wait_for` only sees the new run, while the xterm keeps
  the old scrollback behind a separator line. The user's terminals
  stay read-only.
- **`wait_for`** (plain substring over the rendered tail, default
  30 s, max 120 s) on `open_terminal`, `terminal_tab restart` and
  `read_terminal`, so "start the server, wait for `Local:`, open the
  browser" is two calls without `sleep`.
- **Write modes only.** In those modes the read pair is always
  advertised alongside `open_terminal`, because the tool list is fixed
  for a turn and a terminal opened mid-turn must be readable in it.
- **Plumbing.** The Tauri layer owns PTYs and the event bus, so it
  installs a `TerminalSpawner` on the `TerminalRegistry` the coder
  already shares. Agent opens emit `terminal:agent_opened` (the
  frontend adopts the running stream as a tab); restart and close emit
  `terminal:respawned` / `terminal:removed`.

## Rejected alternatives

- **A `terminal: true` flag on `bash`.** The two have different
  contracts: `bash` returns an exit code and output and its processes
  are session-owned; a terminal has no result, belongs to the user,
  and outlives the session. Folding them together blurs which
  lifetime and visibility a call gets, and builds/tests would spam
  the strip.
- **Promote every detached `bash` process to a visible terminal.**
  Same spam; and a detached process isn't interactive.
- **A separate "agent processes" panel.** A second place to look for
  the same kind of thing; the user should be able to Ctrl+C or retype
  the dev server command exactly as in their own terminal.
- **Letting agents type into any terminal** (including the user's).
  Races the user's keystrokes; restart/close of agent terminals covers
  the observed need.
- **Session attribution on the tab** (which session opened it).
  Agent terminals outlive sessions, and the transcript row links back
  to the tab; revisit if several concurrent agents make it confusing.
