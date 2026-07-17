# ADR 0034 — Detached background processes for `bash`

Date: 2026-07-17
Status: accepted

## Context

The `bash` tool races `tokio::time::timeout` against `child.wait_with_output()`.
On timeout the child is killed (`kill_on_drop` + the handle is consumed) and
all collected output is discarded — the model gets back
`"timed out after N ms"` and nothing else. That is the worst outcome for a
long-running build or test suite: dead process, no partial output, no handle
to retry.

The 10-minute cap (`BASH_MAX_TIMEOUT`) is a budget guard, not a usability
feature. The model can chain bash calls to wait longer, but each chain step
re-runs the command from scratch.

## Decision

Add a **detach mode** to `bash` plus two companion tools, backed by a
per-turn process registry.

- **`bash` gains `detach: bool`** (default false). When true the command is
  spawned with stdout+stderr redirected to a log file, the child handle is
  stored in a per-turn `BackgroundProcessRegistry`, and the tool returns
  immediately: `{ detached, id, pid, log_path, target, cmd }`. The 10-minute
  timeout does not apply — the process runs until it exits, the model calls
  `stop_process`, or the turn ends.

- **`read_process(id, wait_ms?, tail_bytes?)`** — polls a detached process.
  Optionally blocks up to `wait_ms` (capped at 60 s, responds to cancel)
  for the process to exit, then returns
  `{ id, running, exit_code, tail, cmd, target }`. `tail` is the last
  `tail_bytes` (default 8 kB, capped at 64 kB) of the log file. The
  `wait_ms` parameter lets the model avoid busy-polling: one
  `read_process(id, wait_ms=60000)` call per minute instead of hundreds of
  instant polls.

- **`stop_process(id)`** — kills a detached process if still running and
  reaps it. Returns `{ id, killed, exit_code }`.

### Registry lifetime

The registry is **per-turn**, carried on `ToolContext` (same pattern as
`FormatQueue`). At turn end, `spawn_turn_loop` calls
`registry.cleanup()` — which kills + reaps every still-running process and
deletes log files — right after `flush_format_queue`, for every termination
(Ok / Aborted / Err). Sub-agents get their own registry (they build their
own `ToolContext`); it is cleaned up when the sub-agent task finishes.

Processes do **not** survive across turns. The model's expected workflow is:
launch detached → poll with `read_process` (using `wait_ms`) → report
result → finish turn. If the model gives a final answer while a process is
still running, the process is killed at turn end — a reasonable safety net.
The system prompt tells the model to poll until completion before answering.

### Log files

Host-side `/tmp/moon-coder-bg/<n>.log` (monotonic global counter for
uniqueness across concurrent turns). Both host and container commands
redirect through the same mechanism: `docker exec`'s stdout/stderr is
captured on the host side, so the log file is always host-local.

### Mode gating

Not gated — `Research` sub-agents can launch detached inspection commands
(`cargo check`, `pytest --collect-only`), same as the existing `bash` tool.
The "don't mutate" half stays prompt-enforced.

## Rejected alternatives

- **Per-session registry (processes survive across turns).** More flexible
  — the model could "check back later" in a follow-up — but requires a
  Tauri command for UI-level kill, threading the registry into
  `SessionRuntime`, and deciding whether sub-agents share the parent's
  registry. None of that is needed for the core workflow (launch → poll →
  report). Per-turn keeps the scope tight; a per-session upgrade is
  non-breaking if the need appears.

- **Return partial output on timeout instead of killing.** The current
  `wait_with_output()` buffers all output in memory before returning, so
  partial output is not available without switching to streaming reads.
  Detach mode sidesteps the problem entirely — the process keeps running
  and output is tailed from the log file on demand.

- **Model hand-rolls `nohup … &` + `tail`/`ps`.** Works but fragile: the
  IDE can't clean up, the model parses shell output to check status, and
  log files litter `/tmp`. First-class tools give structured results and
  deterministic cleanup.

- **Raise or remove the timeout cap.** A hung process would burn the LLM's
  context budget waiting. Detach mode keeps the cap for blocking calls
  (where it belongs) and removes the time pressure for detached calls
  (where the model is actively polling).
