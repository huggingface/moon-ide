# ADR 0078 — Cap fully-refused tool-call rounds; quote the bytes in the refusal

Date: 2026-08-31
Status: accepted; implemented.

## Context

Session `sess-…-8f998988` (GLM-5.3 via baseten, right after a
compaction): the model needed to call
`mcp_call(server: "playwright", tool: "browser_navigate", args: {url})`.
It emitted `{"args": , "server": "playwright", "tool":
"browser_navigate"}` — a key with no value, literal invalid JSON —
**eleven rounds in a row**, ~2 s apart, each round re-paying a
~170k-token prompt at 26 output tokens. Not truncation:
`stopReason: toolUse`, 26/26-token responses. The model cannot write
the `args` object it never learned (it had `mcp_list_tools`' schema
but fumbles the JSON), and the refusal it got back — "arguments were
not valid JSON (expected value at line 1 column 10)" — names a
position, not the mistake. Since ADR 0076 repairs the broken call to
`{}` on the wire, the refusal result was the **only** place the raw
bytes ever surfaced, and they weren't in it. The turn only stopped
because the user did.

Two gaps: the refusal wasn't actionable, and nothing capped the
loop. The empty-shell retry (`EMPTY_RESPONSE_RETRIES`) already
established the pattern for "provider keeps returning garbage →
fail the turn loudly".

## Decision

1. **The refusal quotes the model's own arguments** (clipped to ~512
   chars from the head, ellipsis-marked) plus a concrete recovery
   line — including the specific fumble seen live: to omit an
   optional argument, leave the key out entirely.
2. **A round in which every tool call was refused counts toward
   `BROKEN_TOOL_CALL_ROUNDS` (3)**, in both the parent turn loop and
   the sub-agent loop. Consecutive fully-refused rounds beyond the
   cap fail the turn with `BrokenToolCallLoop` — an error banner,
   not a silent stop, and no more prompt-token burn. Any round with
   at least one dispatchable call resets the counter: one malformed
   call beside healthy ones is a fumble, not a stuck model.

## Rejected alternatives

- **Raise the cap / let MAX_TURN_ITERATIONS handle it.** 200
  iterations at ~170k prompt tokens each is the burn we're
  preventing; the iteration cap is for productive work, not loops.
- **Repair-and-dispatch** (pass `{}` to the tool). The pre-ADR-0045
  behaviour; it turns a JSON mistake into a misleading schema error
  ("missing field `path`") and hides the real problem.
- **Disable the tool after repeated failures.** Too aggressive for a
  first cut; the quoted-bytes refusal may unstick most models, and
  the loop breaker bounds the damage when it doesn't.

## Related

- ADR 0045 — the refusal itself; this makes it self-descriptive and
  bounded.
- ADR 0076 — the wire repair that made the refusal the only place
  the raw bytes surface.
- `specs/coder.md` § Truncated answers — tool-call half.
