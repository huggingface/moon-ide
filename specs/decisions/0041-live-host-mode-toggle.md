# ADR 0041 — Host-mode override applies per tool dispatch, not per turn

Date: 2026-07-22
Status: accepted; amends the snapshot posture in
[ADR 0022](0022-coder-host-mode-override.md) for `resolve_bash_target`.

## Context

ADR 0022's per-session force-host toggle was read once at turn start,
so a mid-turn flip applied to the _next_ turn. That matched how model
picks are snapshotted, but the analogy is wrong in practice: a user
reaching for the toggle usually needs the _running_ agent's next
command on the other target (e.g. "check the host side of this" typed
as a steer into a long turn). Steers extend the same `run_turn` loop,
so the toggle appeared to do nothing until the whole task finished.

## Decision

The session's override lives in a shared live flag
(`SessionRuntime.force_host_bash`, an `Arc<AtomicBool>` mirroring the
persisted `header.bash_target_override`). Every turn's `ToolContext`
clones the flag in, and `bash` / the shell-target probe read it at
dispatch time — so a toggle re-routes the **next tool call** of an
in-flight turn. An already-running command keeps the target it
started with; nothing is relocated retroactively.

The system prompt's host-vs-container path advertising still composes
once per turn (it's part of `messages[0]`); a mid-turn flip corrects
it on the next turn. `bash` results echo their `target`, so the model
sees where each command actually ran regardless.

Sub-agents keep their historical behaviour (always auto) — the
override remains a top-level-session escape hatch, per ADR 0022.

## Rejected alternative

Re-snapshotting the header at each loop iteration — cheaper to write
but still misses toggles between a dispatch batch's calls, and locks
the session mutex more; the shared atomic is simpler and exact.
