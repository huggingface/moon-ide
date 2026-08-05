# ADR 0057 — Tool-call ids are session-unique, enforced by moon

Date: 2026-08-05
Status: accepted; implemented.

## Context

The coder pipeline treats a tool call's id as **session-wide
identity**: the panel reducer upserts transcript rows by id
([`specs/coder.md` § Session persistence](../coder.md) — "a
transcript row's id is its identity"), orphan recovery keys off it,
`rerun_tool_call` / `ask_user` prompt routing look calls up by id.

That contract assumes the provider mints globally unique ids
(Anthropic's `toolu_…`, OpenAI's `call_…`). Some OpenAI-compat
providers don't: Kimi-K3 via Baseten emits per-message ids —
`bash:0`, `bash:1`, … — resetting the counter on every assistant
message. Under the upsert contract a recycled id is silently
catastrophic: the new call's `tool_call` event matches the old,
already-finished row and is dropped, so the tool **runs without ever
appearing in the transcript**, and its result overwrites the old
row's. Observed live on a 369-call session: 67 calls invisible from
the point the provider switched id schemes; reload "fixed" it only
because replay keeps the first occurrence per id — still losing 67
rows. Orphan recovery was equally confused: a shadowed call reads as
completed by the collision, so a crash there loses the call forever.

## Decision

Moon enforces session-unique tool-call ids itself rather than
trusting the provider:

- **Live path**: `dedupe_response_tool_call_ids` runs on every
  streamed response before anything observes it (events, `messages`
  push, persisted record, dispatch all agree). A collision is
  remapped to `{id}-dupN` (N ≥ 2) — alphanumeric-safe, so Anthropic's
  id charset stays satisfied. The pairing the provider sees on the
  next round-trip stays consistent because the assistant
  `tool_calls` entry and the tool result reference the same remapped
  id.
- **Load path**: `dedupe_record_tool_call_ids` applies the same
  remapping in `sessions::load`, the choke point every consumer
  flows through (replay, orphan recovery, resume/revert truncation
  rewrites). `Tool` and `SubagentSpawned` records re-pair with the
  _latest_ prior use of their original id, matching the live pairing
  order. Old transcripts repair on next open — no one-off migration,
  per "no premature migrations" the repair lives in the parser.

## Rejected alternatives

- **Keep provider ids, teach the frontend positional matching.**
  Every id-keyed surface (reducer, prompts registry, rerun,
  workers) would need its own disambiguation, and the on-disk
  transcript would stay corrupt for any consumer that reads it
  directly (the trace viewer, hub-synced sessions).
- **Refuse providers that recycle ids.** The router model list
  changes under us; hard-failing a model the user picked for cost /
  speed because of an id quirk is worse than a transparent remap.
- **Per-turn id prefixes minted by moon (`turn5-bash:0`).** Also
  works, but rewrites every id even for well-behaved providers and
  makes traces noisier; suffixing only collisions keeps the common
  case byte-identical.
