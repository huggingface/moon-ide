# ADR 0106: Background work is never waited on

Amends [ADR 0102](0102-background-work.md) (the `wait` half of
`wait` / `stop`).

## Context

ADR 0102 gave `wait(id, wait_ms)` a blocking mode (up to 10 minutes)
next to the completion message. Models used it as the default: they
spawned a sub-agent and then sat in `wait` for it, holding the turn
open (and the user out) for a result that would have woken them
anyway. Lowering the cap and adding nudges only shortened the
polling loop.

## Decision

- `wait` is replaced by **`check(id, tail_bytes?)`**, which never
  blocks: a process's status and output tail, a sub-agent's status or
  report. A still-running sub-agent comes back with a note saying the
  report will arrive by itself.
- Waiting means ending the turn. Sub-agent reports and `notify`
  process results arrive as `<background_result>` messages, queued into
  a running turn or waking an idle session. Something seconds away (a
  dev server coming up) is waited for with a foreground `bash` that
  exits when the condition holds.
- With no parked waiters, every settled sub-agent and exited `notify`
  process reports. A `check` after the fact returns the cached result
  again; that duplicate is cheaper than the bookkeeping that hid it.

## Rejected alternatives

- **Keep `wait` with a short cap** — still a polling loop, just a
  tighter one.
- **Drop the tool entirely** — plain detached processes (dev servers,
  watchers) still need a way to show their output on demand.
