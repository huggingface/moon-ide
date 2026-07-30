# ADR 0048 — The coder can list and read the project's terminals

Date: 2026-08-05
Status: accepted; implemented.

## Context

The agent and the user work the same project through two disconnected
surfaces. The user has terminals open — a dev server, a test watcher,
a long build — and the agent has `bash`. It cannot see any of them, so
when the user says "the dev server is throwing an error", the agent's
only move is to run the dev server _again_ through `bash`, which
either fails on the port the user's process already holds or produces
a second, differently-configured run whose output disagrees with the
one on screen. Detached `bash` processes (ADR 0034) solved the "agent
needs a long-running process" half; they did nothing for "the _user_
already has one".

Reading a terminal is also the cheapest possible answer to "which
ports is this project using" — the dev server printed it.

## Decision

**Two read-only tools, `list_terminals` and `read_terminal`**, backed
by a `TerminalRegistry` in `moon-terminal` that the Tauri terminal
commands populate and `moon-coder`'s `ToolRegistry` reads. Nothing
writes: the agent cannot type into a terminal, resize it, or close it.

**Scoped to the session's own folder.** Each terminal is tagged at
open time with the bound folder it was opened for, and both tools only
ever see terminals whose tag matches the session's routing folder. A
worktree session therefore sees its worktree's terminals and not its
parent's — consistent with how every other tool in a worktree session
routes (ADR 0040). A terminal belonging to another project is refused
exactly like an unknown id, with this project's ids listed for
recovery.

The folder tag is **passed in explicitly** rather than derived from
`cwd`. A worktree rides its parent's bind mount, so its container cwd
is a path _under_ the parent's folder; and a shell's live cwd stops
describing the project the moment the user `cd`s.

**Advertised only while the project has a terminal open**, following
the MCP meta-tools' precedent (ADR 0033): with no terminals the pair
can only answer "there are none", and the tool list is rebuilt every
turn, so they appear the moment the user opens one.

**Output is emulated, not ANSI-stripped.** Each terminal keeps a
bounded ring of raw PTY bytes (256 kB) and a read replays it through
a throwaway `vt100` emulator sized to the terminal. That is what turns
`\r`-redrawn progress bars, colour codes and cursor addressing back
into the text the user is actually looking at — a stripped byte log
reads as every frame a progress bar ever painted. Emulating at read
time rather than keeping a live emulator per tab puts the cost on the
rare read instead of the hot write path, and keeps standing memory at
a fixed ring per terminal instead of megabytes of character grid.

**Registry lifetime tracks the tab, not the process.** A terminal
whose shell exited stays readable — its output is still on the user's
screen, so it should still answer — and is dropped when the tab
closes.

Reads are capped (200 lines by default, 2000 and 100 kB per call) and
the tool description tells the model that terminals may hold output
the user did not mean to share, and not to quote more of it back than
the task needs.

## Rejected alternatives

- **Ambient terminal context in the system prompt** (inventory, or
  worse, recent output on every turn). Terminals hold secrets; a
  dev-server log would also churn the prompt prefix every turn and
  cost tokens on turns that have nothing to do with terminals. An
  explicit tool call is auditable in the transcript and only pays when
  used.
- **Letting the agent write to a terminal.** Turns a user-owned
  surface into a shared one, races the user's keystrokes, and there is
  no need: `bash` already runs commands, with `detach` for long ones.
  If a coder-driven terminal is ever wanted, it should be its own
  terminal, not the user's.
- **Deriving the project from the shell's live cwd** (`OSC 7`, or
  probing the child's `/proc/<pid>/cwd`). Follows a `cd` — attractive
  until an agent asks for "this project's terminals" and gets one that
  wandered into a sibling, or misses one whose worktree cwd looks like
  the parent's.
- **A port-focused tool instead** (`list_ports` off `ss`/`lsof`).
  Answers less: ports don't say what crashed. Nothing stops us adding
  it later, and reading the terminal already covers the common case.
- **Frontend-sourced scrollback** (ask xterm.js for its buffer). Puts
  the UI in the data path for an agent capability, against the
  architecture invariant that the core owns everything the agent
  touches, and would return nothing for a background window.
- **A live `vt100::Parser` per terminal.** Simpler read path, but
  standing memory scales with grid × scrollback per tab to serve reads
  that essentially never happen.

## Related

- `specs/coder.md` § Tool surface, § Reading the user's terminals.
- [ADR 0009](0009-terminal-pty-and-targets.md) — the PTY / target
  split this registry sits alongside.
- [ADR 0034](0034-detached-background-processes.md) — the agent's own
  long-running processes, which this deliberately isn't.
- [ADR 0033](0033-coder-mcp.md) — the "advertise only when there's
  something to talk to" precedent.
