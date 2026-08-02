# ADR 0052 — Disconnect a worker from its coordinator

Date: 2026-07-30
Status: accepted; implemented.

## Context

[ADR 0043](0043-user-message-notifies-coordinator.md) settled that a
user _message_ into a coordinator-spawned worker is a nudge, not a
takeover: the coordinator is notified and the worker stays hooked up.
That covers "skip the e2e tests" — but there is no gesture for "this
one is mine now, stop driving it." Watching a coordinator steer a
worker you're actively working in, queue commits onto its branch, or
wind down work you're mid-edit on is the double-driver problem
[ADR 0036](0036-worker-takeover.md) was trying to solve — ADR 0043
removed its only exit (message = takeover) without replacing it with
an explicit one.

## Decision

A coordinator-spawned worker's session bar shows a **disconnect**
button (the chat panel's disconnect glyph). Clicking it unhooks the
worker from its orchestrator:

- **The link is cut.** The dispatch feeder stops forwarding the
  worker's events, and user messages into it no longer notify the
  coordinator (the ADR 0043 notice looks up the same link).
- **The control tools refuse it.** `steer_worker`, `abort_worker`,
  `respond_to_worker_prompt`, `commit_worker_changes`,
  `merge_worker_changes`, and `discard_worker_worktree` error with
  "disconnected by the user — no longer attached to you". The
  read-only tools (`observe_worker`, `review_worker_changes`,
  `workspace_scm_status`) still work — inspecting a session you no
  longer drive is harmless, and the coordinator may need one last
  look to re-plan.
- **The worker is never touched.** Its session, transcript, branch,
  and worktree stay exactly as they are. A turn in flight runs to
  completion — disconnect is a handover, not a kill switch.
- **The coordinator hears it exactly once.** If the worker was
  running, the final notice rides the in-flight turn's `TurnComplete`
  ("was disconnected by the user and its in-flight turn has now
  finished…"); if idle, the notice goes out immediately. Either way
  the notice names the worker, says the control tools now refuse it,
  and tells the coordinator to re-plan without it. After the notice
  lands the link is dropped entirely.
- **Second click = stop it now.** On an already-disconnected worker
  the same button cancels the in-flight turn (an abort), which also
  triggers the final notice via the abort's `TurnComplete`. The
  affordance is probed per visible session
  (`coder_is_coordinator_worker`) because the registry is in-memory:
  nothing on the session header marks a worker, and a restart makes
  the button disappear on its own.

Sessions no coordinator spawned are unaffected: the command is a
no-op for them and the control-tool gate only refuses ids that were
registered **and** disconnected — a coordinator can still steer a
session that was never its worker (ADR 0030 permits this).

## Rejected alternatives

- **Disconnect aborts the turn immediately.** Conflates "stop
  driving it" with "stop it". The user's own Esc already aborts; the
  second-click path covers the "and halt" intent without making the
  default gesture destructive.
- **Disconnect removes the session / worktree.** The worker's branch
  is the deliverable; the user taking over wants it intact.
- **Gate the read-only tools too.** A coordinator re-planning around
  a lost worker legitimately needs a final `observe_worker`; refusal
  would force it to guess. Reads can't steer.
- **Message-the-worker = disconnect (ADR 0036 redux).** Already
  litigated: ADR 0043 killed implicit takeover because the common
  case is a nudge. This ADR adds the _explicit_ gesture ADR 0043's
  design assumed would exist.
- **A coordinator-side `release_worker` tool.** Symmetric and cheap,
  but no driving use case yet — the coordinator can already abandon
  a worker by ignoring it. Added later if dogfooding asks for it.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md)
  — the registry, feeder, and control-tool surface this gates.
- [ADR 0043 — a user message notifies, doesn't unhook](0043-user-message-notifies-coordinator.md)
  — the implicit-intervention rule; disconnect is its explicit
  counterpart, and reuses the same link lookup so the two never
  disagree.
- [ADR 0036 — worker takeover](0036-worker-takeover.md) — the
  superseded implicit design; the control-tool refusal list mirrors
  it, now keyed to an explicit user gesture.
