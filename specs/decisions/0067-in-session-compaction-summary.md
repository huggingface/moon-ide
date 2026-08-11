# 0067 — In-session compaction summaries

## Context

Auto-compaction summarised the folded prefix out-of-band: render the
older messages to role-labelled text, chunk it under the standard
model's window, summarise each chunk sequentially, merge with a
final pass. That request shape shares no prefix with the session's
own round-trips, so every byte was a prompt-cache miss — the
provider reprocessed up to ~80 % of the context window as fresh
input, per chunk, sequentially. Users saw compaction stall a session
for minutes; it also billed the whole prefix at full input price.

## Decision

Ask the driver model to summarise its own live conversation: append
one summarise instruction to the in-memory `messages` and send it as
a normal round-trip — same model, same composed system prompt, same
tool definitions, same image elisions — so the request is
byte-identical to the session's previous round-trip up to the
appended instruction and the provider's prompt cache absorbs the
entire prefix. Cost drops to one cache-hot call plus the summary
output; latency drops from minutes to roughly the summary's
generation time. The summarise interaction is never persisted or
echoed back into the history; only the existing `Compaction` record
lands in the JSONL, so replay is unchanged.

The call passes an explicit `max_tokens` sized to the remaining
window headroom (new optional override on
`chat_completion_stream`), because Anthropic rejects requests where
`input + max_tokens` exceeds the context window. When headroom is
too small for a useful summary, or the in-session call fails /
replies empty / calls a tool despite the instruction, compaction
falls back to the old chunked out-of-band pass — still the only
strategy that works on a prompt already past the window (e.g. a
giant session reopened after the trigger was disarmed).

Tools stay in the request and `tool_choice` stays untouched: both
are part of the provider's cache key (Anthropic documents
`tool_choice` changes as invalidating the messages cache), so the
"don't call tools" constraint lives in the instruction text instead.

## Rejected alternatives

- **Keep the chunked pass but parallelise the chunk calls.** Fixes
  some latency, none of the cost; still a full cache miss.
- **`tool_choice: none` to force a text reply.** Invalidates the
  cached messages portion — defeats the point of the in-session
  path.
- **Persist the summarise turn like a normal interaction.** Pollutes
  the transcript and the replayed history with an exchange the user
  never had; the `Compaction` record already carries everything
  replay needs.
