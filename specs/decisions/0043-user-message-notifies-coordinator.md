# ADR 0043 — A user message to a worker notifies the coordinator, it doesn't unhook it

Date: 2026-07-28
Status: accepted; implemented. Supersedes
[ADR 0036](0036-worker-takeover.md) in full. Delivery timing is
superseded by [ADR 0062](0062-parked-coordinator-notices.md) — the
notice parks in the coordinator's steer queue instead of waking an
idle coordinator; the content decisions below stand.

## Context

[ADR 0036](0036-worker-takeover.md) made a direct user message into a
coordinator-spawned worker a permanent **takeover**: the dispatch
feeder stopped forwarding that worker's events, the control tools
(`steer_worker`, `abort_worker`, `respond_to_worker_prompt`,
`commit_worker_changes`) refused it, and the coordinator got one final
notice telling it to re-plan around a worker it no longer owned.

In use that trade is wrong. Typing into a worker is usually a _nudge_
("skip the e2e tests", "the config lives in `config/`, not `src/`"),
not a claim of ownership. Losing the worker for the rest of the run
costs more than the double-driver problem ADR 0036 was avoiding: the
coordinator can no longer commit the worker's branch, answer its
`ask_user`, or wind it down, so the user inherits chores they didn't
ask for — and the fleet-level plan silently loses a member. There is
also no hand-back, so a one-line correction is unrecoverable.

## Decision

A user message into a worker **notifies the coordinator and changes
nothing else.**

- The coordinator's session receives a dispatch notice (same
  events-as-messages channel as a `TurnComplete` wake) naming the
  worker and **quoting the user's message, truncated to 200
  characters** with a `… (N more characters)` tail. The quote is the
  point: the coordinator needs the instruction, not just the fact that
  one happened.
- The worker stays hooked up: the feeder keeps forwarding its events,
  and every control tool keeps working on it.
- Fires on **every** user message, not once. Each one is a fresh
  instruction the coordinator may need.
- Both user paths notify: the desktop composer (`CoderHandle::send`)
  and the phone (`send_to_as_user`, which the bridge's `coder_send`
  uses when it targets a session by id). Coordinator-originated traffic
  goes through plain `send_to` and stays silent — a coordinator doesn't
  need to be told what it just said.
- The prompt says to factor the message in and not to repeat it back
  to the worker. It deliberately does **not** rank the user's message
  against the coordinator's plan — a nudge is information, not a
  directive to re-plan around.
- No new state. `CoordinatorRegistry` is back to orchestrator → worker
  ids, and the `taken_over` flag plus the control-tool refusals are
  deleted.

Non-triggers are unchanged from ADR 0036 (viewing a worker, aborting
it from the panel, answering its `ask_user` card) — those never
injected a competing instruction, and now nothing does.

## Rejected alternatives

- **Keep ADR 0036's takeover.** Rejected above: the common case is a
  nudge, and the penalty is losing the worker with no hand-back.
- **Notify without the message text** ("the user intervened"). The
  coordinator's most likely next move is to steer the worker itself,
  and without the text it can only contradict the user. The quote is
  the useful part.
- **Forward the whole message.** A pasted stack trace or spec dump
  would eat coordinator context for an instruction the coordinator
  doesn't own. 200 characters carries the intent of a typical nudge;
  the coordinator can `observe_worker` if it needs more.
- **Suppress the coordinator's next steer of that worker** (a soft
  lock instead of a hard one). More state and more surprise for a
  problem the prompt handles: the model is told the user's instruction
  wins.
- **Emit a UI event instead of a session message.** The coordinator is
  an LLM loop, not a view — awareness has to arrive in its context to
  affect behaviour.

## Related

- [ADR 0036 — worker takeover](0036-worker-takeover.md) — superseded
  by this ADR.
- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the dispatch-packet channel this notice rides, and the "workers are
  ordinary sessions the user can open" premise that makes user
  messages possible at all.
