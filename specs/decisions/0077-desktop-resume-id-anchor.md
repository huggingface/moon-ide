# ADR 0077 — The desktop's resume-from-assistant anchors on a tool-call id

Date: 2026-08-31
Status: accepted; implemented.

## Context

A "Replay from here" on a mid-turn assistant row of a long
coordinator session — a few messages above the end — instead cut
the transcript ~40 messages too high, swallowing the thread the
user was answering. The model's first post-cut thinking literally
read "This message seems out of context — but to what?".

The desktop sent an `assistant_ordinal` counted over **visible
assistant rows**; the backend's `truncate_before_assistant_record`
counts **assistant records with tool calls**. The sets diverge by
every tool-only assistant record — a bare call (a `bash`, a
`spawn_worker`, an `observe_worker`) with no prose renders tool rows
but no assistant row. On this session the drift passed 70 records
(120 visible mid-turn rows vs 193 backend records); every ordinal
landed that far off.

The companion hit the same class of bug first (its windowed
transcript undercounted) and was fixed in commit 8b6e1e7 by
anchoring on the persisted tool-call id; the backend method
(`resume_from_tool_call_in`) shipped then, but the desktop kept its
ordinal path.

## Decision

The desktop resume now anchors on the **first tool-call id** the
assistant row issued (mirroring the companion's `resumeAnchorFor`):
`coder_resume_from_tool_call(tool_call_id)` →
`Coder::resume_from_tool_call` → the existing id-anchored backend.
Tool-call ids are persisted and globally unique, so neither side
counts anything and nothing can drift. The bridge's ordinal variant
stays only as its fallback parameter.

A related observation, not changed here: the resume path
(`truncate_before_assistant_record`) writes no pre-truncate `.bak`,
unlike the user-message truncation. The one from this incident's
session was from an earlier, different revert. Worth a follow-up if
resumes ever misbehave — the backup already saved one 3000-message
session.

## Rejected alternatives

- **Fix the ordinal's counting to match the backend's.** Still two
  counts that must agree forever; any new row shape that doesn't
  map 1:1 to a record re-introduces the drift silently.
- **From-end counting** (the companion's revert fix). Fixes
  windowing, not set-mismatch — the invisible tool-only records
  still aren't in the tail count.

## Related

- Commit 8b6e1e7 — the companion-side fix whose backend method this
  now uses.
- `specs/coder.md` § Resume from a mid-turn agent response.
