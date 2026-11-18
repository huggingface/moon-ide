# ADR 0125: Any session runs workers; the coordinator mode is gone

## Context

[ADR 0030](0030-orchestrator-sessions.md) made orchestration a _kind_
of session: a coordinator, picked at creation, that had the worker
tools and nothing else. It could not edit, could not ask the user
(`ask_user`), and had no sub-agents (`task`). So the kind had to be
chosen before the work was understood: an ordinary session that found
its task split in three could not fan out, and a coordinator could not
make a one-line fix.

## Decision

There is one kind of top-level session. Every one has the worker tools
(`spawn_worker` and the rest of ADR 0030's set) next to `task`,
`ask_user` and the write tools. "Coordinator" stays as the name of a
role: the session a worker is attached to.

- **One level.** A session that is someone's attached worker gets no
  worker tools, and a call to one is refused. `/attach` refuses a link
  that would put a worker under a worker. This is what the mode gave
  already (workers were plain agents); it is now a rule about links,
  so a worker the user disconnects can lead a fleet of its own.
- **No cap** on the number of workers, as before.
- **Prompt.** The coordinator prompt is replaced by a short _Workers_
  section after the base prompt, for sessions that have the tools:
  when to use a worker, a sub-agent, or neither, and the rules no
  single tool description carries (stale bases, landing, the user
  messaging or disconnecting a worker).
- **Nothing to pick.** The "New coordinator" buttons (desktop and
  phone), the `coord` badges and the coordinator empty state are
  removed, with `coder_new_coordinator_session` and the `mode` field
  of the session header, summary and `session_loaded` event. To
  coordinate, ask a session to.
- **`/attach`** always attaches the picked session to the one it is
  typed in. It used to depend on which of the two was a coordinator.

Sessions created as coordinators open as ordinary sessions and keep
their workers: the fleet is rebuilt from their records, not from the
mode.

## Rejected

- **Keep the button as a preset** ("delegate, don't edit"). A second
  way to start a session for what one sentence in the first message
  does.
- **Workers that spawn workers.** Chains of ownership for the wake-ups
  and for the user to follow, with no case asking for it.
- **Advertise the worker tools only once asked for.** A tool list that
  changes mid-session breaks the provider's prompt cache, and the model
  cannot offer what it cannot see.

## Cost

Every session's request carries fourteen more tool definitions and the
section. And nothing forces delegation any more: "cannot edit" kept a
coordinator's context small, where a session that can do the work
itself often will.
