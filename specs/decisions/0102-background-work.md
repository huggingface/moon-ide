# ADR 0102 — One model for background work: notify-processes, `wait` / `stop`, staged Esc

Date: 2026-11-01
Status: accepted. Revises [ADR 0085](0085-session-scoped-background-processes.md)
(its rejected "wake on process exit" alternative) and
[ADR 0091](0091-background-only-subagents.md) (tool names, report
envelope, Esc cascade).

## Context

Long jobs (an e2e suite, a CI run, a deploy) outlast the 10-minute
`bash` cap. The model can detach them, but nothing tells it when they
finish, so it either polls in a loop of `read_process(wait_ms)` calls
or ends its turn and forgets. Sub-agents already solved this: they run
in the background and their report wakes the session (ADR 0091). The
two kinds of background work had grown separate tool sets
(`read_process` / `stop_process` vs `task_collect` / `task_abort`),
separate wording in the prompt, and different Esc behaviour. The user
also couldn't see at a glance what was still running in a session.

## Decision

- **`bash({ detach: true, notify: true })`.** When the process exits,
  the session gets a `<background_result kind="process" …>` message
  carrying the exit code and the output tail. Like a sub-agent report,
  it is queued into a running turn or wakes an idle session. A "probe"
  needs nothing extra: it is a command that exits when its condition
  holds (`gh run watch --exit-status`, an `until curl …` loop,
  `timeout 2h …`).
- **One envelope.** Sub-agent reports become
  `<background_result kind="subagent" …>`, replacing
  `<subagent_report>`.
- **One pair of tools.** `wait(id, wait_ms)` replaces `read_process`
  and `task_collect`. `stop(id)` replaces `stop_process` and
  `task_abort`. `task_steer` and the `bash` spawn stay as they are. A
  result taken by a parked `wait` is not delivered again, and a
  `stop`ped item never wakes the session.
- **Staged Esc.** The first Esc stops the running turn only.
  Background sub-agents and notify-processes keep running and still
  report. A second Esc, with no turn running, stops all of them.
- **Active strip.** Above the composer, the session lists its running
  sub-agents and notify-processes, each with a stop button. Plain
  detached processes stay off it: they are usually dev servers or
  watchers that never wake the session, and their `bash` row already
  shows a live tail with a stop button.

## Rejected alternatives

- **A dedicated probe / watch tool.** Every condition the model needs
  can already be expressed as a command, and exit codes cover success,
  failure and timeouts.
- **Notify on by default for detached processes.** A build detached
  only to keep the turn moving would wake the session on every
  compile.
- **Esc stops everything at once** (ADR 0091's cascade). With the
  strip showing what's running and a second Esc stopping it, the first
  Esc can mean "let me redirect the agent" without killing a long run.
- **Esc cancels pending wake-ups but leaves processes running.**
  Silently drops the result the user was waiting for.
