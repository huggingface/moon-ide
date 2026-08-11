# 0066 — Session-targeted desktop sends

## Context

The desktop composer's `coder_send` carried no session id: the
backend routed it to the active folder's **visible session**
(`FolderSession::visible`), trusting that pointer to match what the
panel showed. It doesn't always:

- `openSession` flips the frontend's `visibleSessionId` immediately,
  but the backend only moves its pointer at the _end_ of
  `open_session_impl` — after reading and replaying the whole JSONL.
  A send typed during that window landed in the **previously**
  visible session of the same project, silently.
- A _failed_ open left the two pointers desynced indefinitely; every
  subsequent send kept landing in the old session.

The companion already closed this hole for the phone: its
`coder_send` passes the open session's id and routes via
`send_to_as_user` "so the message can't land in whatever session the
desktop happens to have visible" (ADR 0023 / 0043). The desktop
never got the same treatment.

## Decision

`coder_send(text, images, session_id?)`. The panel passes the id of
the session its composer is bound to; the backend routes it through
`send_to_as_user` (same path as the phone — steer queueing,
coordinator notice, images all work by id). `session_id: null` (no
visible session yet — fresh folder) keeps the old visible-session
resolution, which mints a blank session on first use.

`send_to_inner` now returns the resolved `SendTarget` so the command
persists the last-opened-session pointer exactly as the untargeted
path does. The backend's visible pointer is **not** touched by a
send — it remains owned by open/new/delete, so a targeted send
racing an in-flight open can't flip it backwards.

A send into a session whose runtime isn't mounted (e.g. the open
that flipped the panel's pointer failed) now errors visibly in the
composer's own transcript instead of silently posting elsewhere.

## Rejected alternatives

- **Have the send also `set_visible` (self-heal the desync).** A
  send IPC racing an `open_session` IPC could land after it and
  point the backend at the wrong session again — recreating the
  class of bug this removes. Message routing is the part that must
  be correct; the pointer stays single-owner.
- **Block the composer until `openSession` resolves.** Punishes the
  common case (long transcripts take a while to replay) to fix the
  rare one, and does nothing for the failed-open desync.
