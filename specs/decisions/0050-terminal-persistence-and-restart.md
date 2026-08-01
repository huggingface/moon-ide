# ADR 0050 — Terminal persistence, restart, and close reasons

Date: 2026-06-21
Status: accepted
Supersedes: the "No persistence in 3.0" section of [ADR 0009](0009-terminal-pty-and-targets.md)

## Context

[ADR 0009](0009-terminal-pty-and-targets.md) deliberately shipped
Phase 3 terminals with no persistence: PTYs don't survive an IDE
restart, and the cost of re-spawning shells with stale titles was
judged higher than the value of `Ctrl+T`. Two real workflows have
since shown up that change that calculus:

1. **Relaunch-and-resume.** The user runs three long-lived
   terminals (three dev servers with distinct `NODE_ENV=…
pnpm --filter …` commands). Relaunching the IDE — which it
   does constantly as we develop it — used to mean re-opening
   three tabs and re-typing three commands.
2. **Container churn.** A terminal opened against the workspace
   container dies when the container stops, gets recreated, or
   when the user opens it during the seconds the auto-resume is
   still booting (`docker exec` refuses with "container is not
   running"). The old UX left a dead tab showing `[exited 127]`
   that the user had to close by hand; there was no path back
   short of opening a new terminal and re-typing.

The user-facing asks that fall out of these: auto-close a tab
when its shell exits (Ctrl+D shouldn't leave a corpse), offer to
restart an exited terminal, and make relaunching the IDE bring
back the same terminals with the same commands.

## Decision

### Persist the recipe, not the PTY

`AppState.bottom_panel.terminals` is a list of
`PersistedTerminal { target, folder, command }` — one per open
terminal tab, in tab order, snapshotted on every persist tick.
The PTY itself (the process, the scrollback) is gone with the
IDE; what survives is the _recipe_ to make an equivalent
terminal: where it runs (host cwd / container cwd), which
project owns it, and the shell-history line it last ran.

On launch, `WorkspaceState.restoreAppState` hands the list to
the terminal store, which re-spawns each entry as a fresh shell
and types `command` into it. The replayed command is what seeds
the new shell's history, so an up-arrow afterwards keeps
walking the same session the command came from. Container
entries wait for the workspace shell to reach `running` (the
launch-time auto-resume can take minutes on an image pull)
rather than erroring out.

This is a fresh shell with its last command re-run, **not**
process persistence — the dev server actually died when the IDE
closed; we just restart it for you. The alternative (tmux
windows inside the container, reattach on launch) was rejected
in ADR 0009 and stays rejected: it's a multiplexer the user
didn't ask for, and "your process kept running while the IDE
was closed" is a surprise we don't want to explain.

### The recorded command comes from the shell's own history

We do **not** guess the command by scraping the screen or
remembering what was typed. The backend prepends a one-line
hook to `PROMPT_COMMAND` in every spawned shell (host and
container):

```
history 1 | sed "s/^ *[0-9]* *//" | base64 -w0 | sed "s/^/MOONCMD/"
```

bash ≥ 5.1 runs `PROMPT_COMMAND` just before displaying each
primary prompt, so after every command the shell itself echoes
its newest history entry, base64'd, with a marker the
frontend's output scanner recognises. The scanner lifts the
marker line out of the byte stream (the user never sees it) and
records the decoded command on the session. Because the source
is the shell's history, the capture is authoritative — it
covers `cd`, aliases, and commands the user recalled with
up-arrow and edited before running, none of which a
screen-scrape sees.

Degradation is explicit: shells without `PROMPT_COMMAND` (zsh
without a bash-compat shim, fish) never emit the marker, so the
recorded command stays whatever the spawn/replay seeded —
restart and persistence degrade to "replays the restore
command", which is still correct, just not live-updating.

### `command` is typed into the shell, never spawned as argv

Both the restore replay and the restart affordance deliver
`command` as **keystrokes** after the shell is up, not as
`bash -c <cmd>` on the spawn. Two reasons:

- **History.** A `bash -c` command never enters the shell's
  history, so up-arrow after a restore would walk nothing. A
  typed line lands in history like any other.
- **Interactivity.** The recorded command is often a dev server
  or a `cd` + something; running it as keystrokes in a real
  interactive shell preserves job control (Ctrl+C, fg/bg)
  exactly as if the user had typed it.

The write happens after a short delay (readline needs a moment
to initialise) and relies on readline's bracketed-paste mode to
keep any embedded newline from auto-executing mid-line — the
trailing `\n` we append is what runs it. For the _restart_
case, where a stale half-typed line might be sitting at the
prompt, the supervisor clears the line with Ctrl+C before
typing, and owns that clear-then-type sequencing so a racy
frontend can't interleave the two writes.

### Auto-close on shell exit; offer respawn on environment loss

A terminal tab's fate is decided by _why_ its child exited,
classified by the supervisor into a `TerminalCloseReason` on
the `terminal:closed` event:

| Reason                   | Meaning                                            | Tab behaviour         |
| ------------------------ | -------------------------------------------------- | --------------------- |
| `shell_exited`           | host shell exited (Ctrl+D, `exit`, command done)   | **closes itself**     |
| `container_shell_exited` | in-container shell exited, container still running | **closes itself**     |
| `container_stopped`      | container not running afterwards (Stop/Recreate)   | stays, respawn banner |
| `container_not_running`  | `docker exec` refused (container still booting)    | stays, respawn banner |
| `unknown`                | portable-pty couldn't translate the exit           | **closes itself**     |

The split is the honest one: a shell that ends is done, and a
dead tab strip was the old UX's main complaint — so Ctrl+D and
`exit` now close the tab outright. But an environment loss is
not the user's intent, so the tab stays and offers to respawn a
fresh shell (with its recorded command replayed) once the
container is back, or to wait for the container first.

Classification is two probes: a `docker exec` refusal is
recognised from the child's own output (`Error response from
daemon`, exit 125) and skips the daemon probe; any other
container exit is decided by one `docker inspect` liveness
check after the exit.

## Consequences

### What's nice

- Relaunching the IDE brings back the exact terminal setup —
  three tabs, three commands — with zero retyping. This is the
  workflow that motivated the whole change.
- Ctrl+D finally behaves the way a terminal user expects: the
  shell exits, the tab is gone.
- Container churn stops stranding dead tabs. The boot-race
  case (open a terminal while the container is still coming up)
  now recovers on its own instead of showing `[exited 127]`.
- The command replay doubles as the history seed, so the
  restored terminal is a _continuation_ of the old session, not
  just a lookalike.

### What's not nice

- The `PROMPT_COMMAND` hook runs `history 1 | base64` at every
  prompt render, adding a small constant amount of invisible
  PTY traffic. It's a few dozen bytes per prompt; xterm never
  paints it. A user with a very aggressive `PROMPT_COMMAND`
  chain could notice the extra fork.
- History capture only works on bash (and bash-compatible
  `PROMPT_COMMAND` shells). zsh/fish users get restore/restart
  that replays the seeded command but doesn't live-track what
  they ran afterwards. Closing this gap wants a per-shell hook
  (zsh `preexec`), deferred until a team member on zsh asks.
- A command typed but **not run** (sitting at the prompt when
  the IDE quits) is not captured — the history only has what
  executed. Acceptable: persisting a half-typed line would be
  weirder.

## Alternatives considered

### Scrape the screen for the last command

Read xterm's buffer backwards for the last prompt line and take
whatever follows the prompt glyph. Rejected: prompt shapes are
user-configurable (right prompts, multi-line, starship/powerline
glyphs), there's no reliable prompt boundary, and it can't see
commands recalled with up-arrow and edited. The shell's history
is the only source that's always right.

### Remember what the user typed, client-side

Intercept `onData` keystrokes and reconstruct the line.
Rejected for the same reason: it can't model up-arrow recall,
line editing, or paste, and it misses everything typed before
the IDE started watching. Keystroke delivery of the replay
_uses_ `onData`-level writes, but the source of truth for
"what ran" is always the shell.

### tmux / dtach for true process persistence

Revisit ADR 0009's rejected multiplexer: run each terminal as a
tmux window in the container, reattach on launch. This would
give real process persistence (the dev server keeps running
across an IDE relaunch). Still rejected: it's scope creep, a
mental model the team didn't ask for, and for the workspace
container it doesn't even survive the churn that motivated this
(a Recreate kills the tmux server too). The recipe-replay model
gets 90% of the value with none of the multiplexer.
