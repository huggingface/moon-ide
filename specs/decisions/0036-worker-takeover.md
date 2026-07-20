# ADR 0036 — Worker takeover: a direct user message unhooks a worker from its coordinator

Date: 2026-07-14
Status: accepted; implemented.

## Context

ADR 0030 workers are ordinary top-level sessions the user can open
mid-run — "those sessions look like ones I opened; I can take over /
follow up from the agent." But the first cut gave the coordinator and
the user a _shared_ control surface with no arbitration: after the
user started driving a worker from the composer, the dispatch feeder
kept waking the coordinator on every `TurnComplete`, and the
coordinator kept steering / aborting / answering the same session.
Two drivers, one steering wheel — the coordinator would burn turns
reacting to work it no longer owned, and could actively fight the
user (abort their turn, answer their worker's `ask_user`, commit
their half-done changes).

## Decision

A direct user composer message into a coordinator-spawned worker
**takes the worker over**, permanently:

- **Trigger** is a user send (fresh prompt or mid-turn steer) landing
  in a worker session. User sends arrive via `CoderHandle::send`
  (desktop panel and companion bridge both route through
  `coder_send`); coordinator traffic uses `send_to` — the two paths
  are already distinct, so no caller flag is needed.
- **Feeder**: the worker stops feeding dispatch packets to its
  orchestrator.
- **Control tools** (`steer_worker`, `abort_worker`,
  `respond_to_worker_prompt`, `commit_worker_changes`) refuse the
  worker with a recoverable tool error naming the reason. **Read-only
  tools** (`observe_worker`, `review_worker_changes`,
  `workspace_scm_status`) keep working — the coordinator may still
  report on a user-owned worker's state.
- **One final notice** is fed into the coordinator's session (same
  events-as-messages channel as a `TurnComplete` wake) so it re-plans
  instead of waiting forever on a worker that now answers to the user.
  The coordinator system prompt documents the semantics.
- State lives in the in-memory `CoordinatorRegistry` (per-worker
  `taken_over` flag). Not persisted: neither the feeder task nor
  background turns survive a restart, so there is nothing to take
  over after one.

Non-triggers, deliberately: _viewing_ a worker, aborting it from the
panel, and answering its `ask_user` card are assistance, not
takeover — none of them injects a competing instruction stream. The
first typed message is the unambiguous "this is mine now" gesture.

## Rejected alternatives

- **Keep feeding, annotate packets with "user intervened"** — keeps
  two drivers on one session; the coordinator model reliably treats
  awareness as license to keep steering.
- **Pause the whole coordinator on any intervention** — punishes the
  other workers for the user grabbing one.
- **Explicit hand-back (user returns the worker)** — no concrete need
  yet; the user can always tell the coordinator to spawn a fresh
  worker for the remainder. Revisit if it comes up in practice.
- **Takeover on panel abort** — a user stopping a runaway worker may
  still want the coordinator to handle the fallout; only typing
  claims ownership.
