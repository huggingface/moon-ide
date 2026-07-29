# ADR 0045 — Output-token truncation is a bug, not a result

Date: 2026-07-29
Status: accepted; implemented.

## Context

A four-way parallel research delegation produced four reports that all
stopped mid-sentence — one ended inside a fenced code block, another
inside a markdown table row. The parent model synthesised from the
fragments without noticing. Every one of the four sub-agent
transcripts ends with `stopReason: "length"`.

Two independent defects lined up:

1. **`max_tokens` was an allowlist.** `max_tokens_for` gave 32 K only
   to slugs matching a hardcoded set of adaptive-thinking families and
   8 K to everything else. The model actually in use (`claude-opus-5`)
   post-dates that list, so every request — parent and sub-agent —
   went out with an 8 K output ceiling. Reasoning counts against the
   same budget, so a long write-up ran out of room mid-answer.
2. **Nothing reacted to `stop_reason: "length"`.** The field was
   parsed, normalised, and persisted, but both agent loops branch only
   on "are there tool calls?". No tool calls meant "final answer", so
   a fragment was returned as the turn's result, and a sub-agent's
   fragment became the `task` tool's result with no signal that it was
   incomplete.
3. **The ceiling can also land inside a tool call**, and that path was
   worse. A big `write_file` cut off mid-`content` yields a `tool_use`
   block whose arguments aren't valid JSON; `parse_tool_args` logged a
   warning and passed `{}`, so the tool answered "missing field
   `path`". That reads to the model as a schema mistake, not a size
   problem, so the natural recovery is to re-send the same oversized
   call — and hit the same ceiling.

Truncation is the worst failure mode we ship: it looks exactly like
success. Both defects had to be individually harmless-looking to
survive, and they were.

## Decision

**`max_tokens` fails open.** The predicate is inverted: 32 K unless
the model is a known small-output family (Haiku, Claude 3), which get
8 K. A model we've never heard of gets the generous ceiling. The
failure mode of being wrong flips from "silently truncated answers"
to "a 400 naming the real cap" — loud, immediate, and fixable in one
line.

**A `length` stop triggers continuation, in both loops.** When a
response carries no tool calls and stopped at the ceiling, the loop
appends a user sentinel telling the model to resume exactly where it
stopped and takes another round-trip, up to `OUTPUT_CAP_CONTINUATIONS`
(3). The sentinel is a real, persisted, rendered user turn — same
posture as the tool-budget wrap-up sentinel — so a transcript read
later explains why an answer arrived in two pieces. Sub-agents
concatenate their fragments so the parent receives one report, and
append an explicit `[Report truncated: …]` marker if the continuation
budget runs out with the answer still incomplete.

**A tool call with unparseable arguments is refused, not
dispatched.** The refusal names the cause (cut off at the
output-token limit, N bytes received), states plainly that nothing
was written or run, and gives the recovery: chunk the write, or make
a targeted `edit_file` instead of rewriting the file. Broken JSON —
not the stop reason — is what triggers the refusal, so a complete
call in a response the ceiling truncated _after_ it still runs; the
stop reason only picks the wording.

The fixes are deliberately all kept. The ceiling fix makes
truncation rare; the continuation fix means that when it happens
anyway — a genuinely enormous answer, a provider on the
OpenAI-compatible path with its own lower default, a future model —
the system degrades to "answer arrives in two bubbles" instead of
"answer silently ends mid-sentence".

## Rejected alternatives

- **Only raise the ceiling.** Doesn't cover the OpenAI-compatible
  path, which sends no `max_tokens` at all and inherits whatever the
  provider defaults to; two of the observed `length` stops were on a
  non-Anthropic route.
- **Only add continuation.** Leaves every answer on an unrecognised
  model chopped into 8 K pieces, burning a round-trip and a prompt
  re-send per piece. The ceiling is the cheap fix; continuation is
  the safety net.
- **Read the per-model output cap from `/v1/models`.** Anthropic
  advertises it, and this is the principled answer — but it needs the
  catalog resolved at request time on every route, and the catalog is
  only fetched when the picker is opened. A denylist of two families
  buys ~all of the benefit today. Worth revisiting if a model with a
  sub-32 K cap ever lands in the team's rotation.
- **Surface truncation to the user and stop.** An error banner on a
  90 %-complete audit is worse than the missing 10 %; the model can
  finish the sentence itself.
- **Continue a truncated _tool call_ the way we continue prose.**
  Asking the model to resume mid-JSON means trusting it to reproduce
  a byte-exact prefix boundary, and a wrong guess writes a corrupt
  file. Refusing and letting it retry smaller is the only safe move.
- **Keep passing `{}` and let the tool's schema error do the
  talking.** It's the status quo, and it's what turned a size problem
  into an unproductive retry loop.
- **Enable adaptive thinking for unrecognised models too** (the same
  allowlist governs `thinking_config_for`). Left alone on purpose:
  sending `thinking: adaptive` to a model that doesn't support it
  400s the request outright, so _that_ predicate is right to fail
  closed. `claude-opus-5` returns summarised reasoning without the
  parameter anyway.

## Related

- `specs/coder.md` § Extended / adaptive thinking — the shipped
  contract, including where `max_tokens` is chosen.
- `specs/coder.md` § Sub-agents / Budget — the iteration cap this
  sits next to.
