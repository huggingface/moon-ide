# ADR 0076 — Repair unparseable tool-call arguments on the request wire

Date: 2026-08-26
Status: accepted; implemented.

## Context

A session hit a wall it couldn't recover from:

1. The model emitted a big `write_file` whose `function.arguments`
   were cut off at the output-token ceiling — not valid JSON.
2. Our refusal logic (ADR 0045) correctly refused to dispatch it and
   answered with a tool result naming the cause and the recovery.
3. But the assistant turn itself (broken blob verbatim) is part of
   history, and the next round-trip replays it byte-for-byte.
4. The HF router validates `tool_calls[].function.arguments` as JSON
   in the **request** body and 400s the entire request:
   `{"message":"Invalid JSON in tool call arguments"}`. Every
   subsequent round-trip of that session failed the same way — the
   turn died, and would keep dying.

The dispatch-side refusal solved the "don't execute garbage" half;
the request side was never guarded.

## Decision

`build_wire_messages` (the single choke point both OpenAI-compat
request sites go through) repairs an assistant turn's tool calls when
any `function.arguments` doesn't parse as JSON: broken blobs degrade
to `{}`, valid siblings and call ids replay untouched. The field
became a `Cow<'a, [ToolCall]>` so the healthy path borrows verbatim —
no allocation, and the wire body stays **byte-for-byte identical**
(routers prefix-cache on the literal request bytes; a spurious
re-encode would miss every cache).

The in-memory history and the JSONL record keep the raw broken blob
— it's the honest record of what the model did, the refusal tool
result already explains it, and repairing on the wire heals
already-persisted sessions too (no migration, per the
no-premature-migrations rule). The Anthropic native path already
parsed args into `input` with a `{}` fallback and was never affected.

Repairing to `{}` rather than dropping the call: the paired tool
result must exist either way (every chat-completions API 400s an
unpaired `tool_calls`), and `{}` is exactly what the refusal already
told the model it got.

## Rejected alternatives

- **Validate-and-fail the turn loudly.** The 400 already did that; the
  problem was the session was _permanently_ bricked. We want the
  opposite: the turn continues.
- **Sanitize at push-into-`messages` time.** Only heals new turns;
  existing persisted sessions (and the JSONL replay path) would still
  ship the broken blob. The wire is the one point everything
  crosses.
- **Drop the broken call from the wire.** Unpaired `tool_calls`
  400 the request on every strict provider.

## Related

- ADR 0045 — the dispatch-side refusal this completes.
- `specs/coder.md` § Truncated answers — tool-call half.
