# ADR 0046 — Retry a failed turn from the error row

Date: 2026-07-29
Status: accepted; implemented.

## Context

A turn that dies on the model call — a provider 5xx, a rate limit, a
dropped SSE stream — leaves an error row and nothing to do about it.
Everything the turn completed is already persisted and still perfectly
valid; only the last round-trip is missing. The affordances that
existed all assume the transcript is _wrong_ and rewrite it: "replay
from here" and "edit & resend" on a user row truncate back to the
prompt and re-run the whole turn (throwing away every tool result it
had already earned), "resume from here" on an assistant row truncates
to that checkpoint and re-dispatches its tool batch. None of them say
"that one request failed, do it again".

In practice the user re-typed "continue" into the composer, which
works — `send` continues from the existing `messages` — but costs a
turn's worth of prompt tokens on a message that carries no
information.

## Decision

**A trailing error row gets a `retry` button.** It re-runs the
round-trip with the messages already in memory: no truncation, no
re-prompt, no re-dispatch. The backend runs orphan recovery first (the
error path, unlike abort, can leave a mid-dispatch tool call
unanswered, and every chat-completions API rejects an unpaired
`tool_use`) and then spawns the turn loop with no resume parameter.

**The error stays.** The `Error` record stays on disk, the row stays
in the transcript, and the retry's output appends below it. It
happened; a transcript that hides it is a transcript that lies about
why there are two assistant answers to one prompt. Keeping it also
makes live state and reloaded state identical without a JSONL rewrite.

**Only the tail row offers it.** An error row with rows after it has
already been retried, or superseded by a later turn — and "call the
model again with the messages we have" only means "redo the thing that
failed" when the failure is the last thing that happened. This is also
what retires the button: a successful retry pushes rows past it.

Anchoring on the tail rather than an ordinal is what makes this the
cheap affordance. Error rows carry no ordinal — they aren't part of
the `User` / `Assistant` record counts the other three commands index
into, and their ids are synthetic counters minted fresh per replay.

## Rejected alternatives

- **Retry on a tool row too.** The obvious sibling, and mechanically
  close to `coder_resume_from_assistant` — but a tool failure doesn't
  fail the turn: the dispatcher turns it into an `is_error` result and
  the model reacts to it, usually correctly. Re-running one tool
  behind the model's back is a different, murkier gesture than
  re-running a request that never happened. Deferred until somebody
  actually wants it.
- **Strip the trailing `Error` record and remove the row.** Cleaner
  transcript, but it needs a JSONL rewrite, and it hides the reason
  the session has a gap. Errors are data.
- **Retry from the nearest preceding checkpoint** (scan back to a
  mid-turn assistant row and resume from it). Re-executes tools that
  already succeeded, for no benefit — the checkpoint we want is the
  tail, and it's already in `messages`.
- **Auto-retry transient provider failures.** Silent retries hide a
  broken key or an exhausted quota behind latency. The loop already
  auto-retries the one failure it can classify (`EmptyResponse`);
  everything else is the user's call.

## Related

- `specs/coder.md` § Retry a failed turn — the shipped contract.
- `specs/coder.md` § Revert, replay, and edit & resend, and § Resume
  from a mid-turn agent response — the three truncating affordances
  this deliberately isn't.
