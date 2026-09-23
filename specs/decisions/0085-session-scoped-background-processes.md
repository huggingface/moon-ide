# ADR 0085 — Session-scoped background processes and interrupt-safe bash output

Date: 2026-09-19
Status: accepted; implemented. Revises the lifetime half of
[ADR 0034](0034-detached-background-processes.md); its tool surface
(`detach` / `read_process` / `stop_process`) and
[ADR 0075](0075-background-process-exit-events.md)'s settlement
events stand.

## Context

ADR 0034 scoped detached processes to the **turn**: killed and
reaped when the turn ends, on every termination path. Dogfooding
broke that model on the abort path: the user hits Esc to redirect
the agent mid-build, the turn ends, and the cleanup kills the
20-minute build the agent had sensibly detached — plus the next
turn's `read_process(id)` misses (fresh per-turn registry), so even
the log was unreachable. The same interrupt also destroyed
_foreground_ bash output: an aborted `bash` returned a bare
`Aborted`, discarding everything the command had printed. And there
was no way for the _user_ to watch what an agent's background
command was doing — the row said `detached · running` and nothing
else.

## Decision

Three coupled changes:

- **Session-scoped registry.** The `BackgroundProcessRegistry`
  moves from per-turn to the `SessionRuntime`. Processes survive
  turn boundaries — an interrupt ends the _turn_, not the
  _conversation_ — and `read_process` ids stay valid across turns.
  Settled entries and their log files are retained for the
  session's lifetime, so the model can fetch an exit code + tail
  turns later. Ids come from a process-global counter (the log path
  derives from the id; per-registry counters had two sessions
  clobbering the same `/tmp/moon-coder-bg/bg_0.log`).
  - **No-orphan guarantee, relocated:** children carry
    `kill_on_drop` (die with the runtime — session deletion,
    replacement), and the IDE's graceful-shutdown hook sweeps every
    mounted session's registry (`kill_all_background_processes`)
    because process exit doesn't run destructors.
  - **Sub-agents keep the per-run lifetime**: a sub-agent's report
    is its end; its runner still cleans up at settle.
- **Foreground bash keeps its partial output.** The `bash` tool
  drains stdout/stderr incrementally instead of
  `wait_with_output`. A command cut short returns a normal result
  carrying everything it printed, flagged `interrupted: true` (user
  abort — the result persists before the turn's next cancel check
  aborts the loop, so the _next_ turn's model sees it) or
  `timed_out: true` (previously an error that discarded all
  output).
- **Foreground bash streams live.** The same incremental drains
  forward output to the panel as a live-only `tool_output_delta {
tool_call_id, stream, chunk }` event, coalesced (~100 ms, flushed
  on silence) and split on UTF-8 boundaries. The running row shows
  the latest output line collapsed and the growing stream expanded
  (auto-scrolls unless the user scrolled up); the settled
  `tool_result` replaces it. Never persisted — the result already
  carries the (truncated) full output, so replay loses only the live
  view. Rides the turn's sink, so sub-agent pop-outs stream too.
- **Live tail in the panel.** New `coder_read_background_process` /
  `coder_stop_background_process` commands read the same registry
  the model polls; the expanded `bash` row of a detached spawn
  renders a polling live tail (1.2 s while running, once when
  settled) with a stop button. Parent transcripts only — a
  sub-agent's per-run registry isn't addressable by session id.

## Rejected alternatives

- **Keep per-turn kill, add a "survived" ledger.** Preserves ADR
  0034's shape but still kills the build on Esc — the ledger would
  only document the damage. The user's point was that an interrupt
  isn't the end of the session.
- **Survive only aborts, kill on clean turn end.** Splits the
  lifetime on a distinction the model can't see ("did my turn end
  cleanly?") and makes `read_process` ids valid or invalid by luck.
- **Wake the agent when a background process exits while idle**
  (ADR 0053-style feeder). Not asked for; the model polls when it
  cares, the panel shows the settlement live, and an unprompted
  wake per compile would be noisy. Revisit on concrete need.
- **Persisting streamed chunks.** Would make replay show the same
  stream, but duplicates the settled result's output record-by-record
  and bloats the JSONL for a view nobody rewatches.

## Related

- [ADR 0034 — detached background processes](0034-detached-background-processes.md)
  — tool surface unchanged; per-turn lifetime revised here.
- [ADR 0075 — background process exit events](0075-background-process-exit-events.md)
  — settlement events now also fire from idle-time observations
  (UI polls / next-turn reads), same wire shape.
