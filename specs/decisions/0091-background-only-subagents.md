# ADR 0091: Sub-agents always run in the background

Revises [ADR 0053](0053-detached-task-subagents.md): `detach` is gone,
every parent `task` is detached, and the wake carries the report.

## Context

ADR 0053 added `task({ detach: true })` next to the synchronous
default. In practice the synchronous call parks the whole parent turn
behind its slowest sub-agent, and the model rarely picks the right
mode. With both shapes around, the wake was only a pointer
("finished, call `task_collect`"), which cost an extra round trip. And
every aborted run still woke its parent, so with all runs in the
background, Esc would stop them and immediately restart the session.

## Decision

- **No `detach` parameter.** Every parent `task` returns
  `{ detached, subagent_id, status: "running" }` immediately. The
  homogeneous-batch parallel path and its 4-permit cap are removed:
  sequential dispatch of instantly-returning calls is already
  concurrent, and nothing needs a cap yet.
- **The completion callback carries the report.** When a run settles
  the parent gets a user-role message
  `<subagent_report subagent_id="…" status="done|error">…</subagent_report>`
  — queued into a running turn or waking an idle parent. The tool
  description and system prompt tell the model this arrives on its
  own, so it doesn't poll. The panel renders it as a collapsed report
  card, not a "you" bubble.
- **`task_collect(wait_ms)` blocks** (cap raised 60 s → 10 min,
  cancel-aware) for a report the parent can't proceed without. A run
  that settles while a collect is parked on it gives the report to that
  call and skips the callback, so the model never sees it twice.
- **No callback for aborted runs** (`task_abort`, the pop-out's stop
  button, Esc, parent session deleted). Esc still cascades to the
  session's sub-agents as in ADR 0053; before this rule each aborted
  run's wake restarted the session the user had just stopped.
- **Background sub-agents count as agent activity.** Each holds a
  running-turn count for its lifetime (handed off to the turn its
  callback starts), so the OS indicator doesn't flip to "done"; and
  the turn-finished notification is skipped while the session still
  has a running sub-agent — the turn the last report starts notifies.
- **Nested research sub-agents stay blocking.** A sub-agent has no
  session to wake.

## Rejected alternatives

- **Keep the wake a pointer** (ADR 0053's choice, to spare context
  when the parent no longer wants the result). In practice the parent
  nearly always collects, so the pointer only added a round trip.
- **Esc stops only the parent turn**, letting sub-agents finish and
  report back (the ADR 0085 posture for background `bash`). Briefly
  shipped; rejected because stop should mean stop, and redirecting
  mid-research is rarer than wanting everything halted.
- **A concurrency cap / queue** on background runs. No observed
  rate-limit problem; add one when there is.
