# ADR 0062 — Coordinator notices park in the steer queue instead of waking an idle coordinator

Date: 2026-08-04
Status: accepted; implemented. Supersedes the delivery timing of
[ADR 0043](0043-user-message-notifies-coordinator.md); its content
decisions (quote the message, truncate to 200 characters, fire on
every message, both user paths notify) stand.

## Context

ADR 0043 delivers the "the user messaged your worker" notice via
`send_to`, which starts a whole coordinator turn when the coordinator
is idle. That wake is almost always redundant: a user nudge into a
worker starts (or steers) a worker turn, and that turn's
`TurnComplete` wakes the coordinator through the dispatch feeder
anyway — so one nudge costs two coordinator turns, and the first one
has nothing actionable in it. Worse, waking an LLM with
information-only input invites it to _act_ — observe the worker
mid-turn, steer it redundantly, contradict the user — against ADR
0043's own framing that "a nudge is information, not a directive to
re-plan around". Each spurious wake also grows the coordinator's
context for nothing.

## Decision

The notice **parks in the coordinator's steer queue**
(`pending_steers`) and never starts a turn:

- Coordinator mid-turn → drained at the next iteration boundary,
  exactly as before (that path already went through the steer queue).
- Coordinator idle → held until whatever starts its next turn: a
  dispatch-packet wake, a direct user message, or "go now" on the
  queued row. It renders as a queued transcript row with the usual
  go-now / unqueue affordances, so the user can see it waiting and
  force the wake if they want one.
- "Go now" on a steer parked on an **idle** session (previously
  impossible — steers only queued mid-turn) now spawns a fresh turn
  whose first iteration drains the queue.
- Parking does **not** skip a parked `ask_user` prompt. The old
  `send_to` path did, which could blow away a question the
  coordinator was waiting on; a notice is information, not an answer.

Known trade-offs, accepted:

- If the nudged worker's turn ends in a parked `ask_user` (no
  `TurnComplete`), the notice waits for some other coordinator turn.
  The worker already has the user's instruction, and `ask_user`
  discovery was already observe-on-wake before this change.
- Parked steers are in-memory; a process restart drops an undelivered
  notice. The coordinator ↔ worker registry is in-memory too, so a
  restart loses the fleet regardless.

## Rejected alternatives

- **Keep the eager wake.** Rejected above: pure cost, and the
  information arrives at a worse moment than the next dispatch packet.
- **Attach pending notices to the next dispatch packet's text.** New
  per-coordinator state and a second delivery mechanism for something
  the steer queue already does — queue, drain-at-turn-start, UI
  visibility, unqueue — for free.
- **Drop idle-time notices entirely.** The quote is the point (ADR
  0043): the coordinator's next decision about that worker needs the
  instruction, whenever that decision happens.

## Related

- [ADR 0043](0043-user-message-notifies-coordinator.md) — notice
  content and triggers; delivery timing superseded here.
- [ADR 0030](0030-orchestrator-sessions.md) — the dispatch feeder
  whose `TurnComplete` wake makes the eager notice redundant.
