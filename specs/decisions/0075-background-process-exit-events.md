# ADR 0075 — Live settlement events for detached background processes

## Context

ADR 0034 gave `bash` a detach mode, but the UI row settled the moment
the spawn returned: the `tool_result` flips the row to `ok` with a
near-zero elapsed, and nothing tells the panel when the process
actually finishes. Worse, when the model answers without polling to
completion, the turn-end cleanup kills the still-running child — and
the row still reads `ok`, which is a lie about what happened.

Detached `task` sub-agents already solved the same shape: the spawn
returns a handle immediately, and a live `subagent_finished` event
flips the card to done/error when the run settles.

## Decision

Mirror the sub-agent pattern for background processes:

- New live-only `CoderEvent::BackgroundProcessExited { tool_call_id,
id, killed, exit_code }`. Live-only like `retry_backoff`: never
  persisted, never replayed. `tool_call_id` is the spawning `bash`
  call's id so the panel flips that row without pattern-matching the
  opaque process id; `killed` distinguishes `stop_process` /
  turn-end-cleanup kills from natural exits.
- The per-turn `BackgroundProcessRegistry` gains an event sink,
  installed by the runner at construction (sub-agent runs wrap theirs
  in `SubagentEvent` so the pop-out transcript behaves the same).
  Every settlement path emits exactly once: natural exit observed by
  `read_process`/`try_reap`, `stop_process`, and the turn-end
  `cleanup()` (which now `try_wait`s before killing so an
  already-dead-but-unpolled child reports as a natural exit, and
  records the exit code of kills it reaps).
- `ToolRegistry::dispatch_with_call_id` threads the provider's
  tool-call id into `bash` so the spawn can stamp its settlement
  event with the row that launched it. Plain `dispatch` passes `""`.
- The panel's `bash` tool row gains a tri-state: after the detached
  spawn lands it reads `detached · running`; the settlement event
  flips it to `detached · exit N` or `detached · killed`. On replay
  (a reopened session) the row shows the plain detached body with no
  live claim — the event is live-only, and per-turn processes never
  outlive their turn anyway (ADR 0034).

## Rejected alternatives

- **Session-scoped registry with cross-turn liveness.** ADR 0034
  already considered and deferred this; the per-turn kill-at-turn-end
  safety net stays. The event stream makes the existing semantics
  _visible_ rather than changing them.
- **Panel-side polling.** The frontend could poll `read_process` per
  open row, but that duplicates the registry's own reaping, races
  `stop_process`, and can't see the turn-end cleanup at all. The
  registry is the one place that already knows every settle.
- **Persist the settlement** so a reopened session shows final
  states. Rejected for now: per-turn processes are dead by the time a
  session reopens, so the persisted row's "detached, no live claim"
  is honest. If a cross-turn registry ever lands, persistence comes
  with it.
