# ADR 0110: Stale thinking blocks are dropped, live state leaves the prefix

## Context

Claude models with preserved thinking (Fable / Mythos 5.1+, Opus /
Sonnet / Haiku 5.5) sign each thinking block against its prefix:
`system`, `tools`, and every earlier message. A replayed block whose
prefix changed fails the check; on Fable 5.2 and Mythos 5.1+ that is
a 400 (`The block is bound to a different conversation`), and the
session is wedged because its stored history keeps the change.
([Anthropic docs](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking/prefix-checks).)

The coder is not append-only:

- the system prompt is recomposed every turn (project rules, bound
  folders and their summaries, container vs host paths, worktrees);
- the tool list follows mode and settings, and the MCP meta-tools
  name the enabled servers;
- compaction replaces older turns with our own summary and keeps the
  recent ones;
- the terminal tools' descriptions listed the open terminals, so
  `tools` changed every time one opened, closed or exited.

The first three change when the project or its configuration does.
The last changed during ordinary work, and `tools` leads the request:
each change also missed the entire prompt cache, on every provider.

## Decision

**Configuration-driven changes reset the reasoning.** Every
native-Anthropic request that carries a `thinking` object sends the
`thinking-binding-controls-2026-08-01` beta and
`thinking.block_binding.prefix_mismatch_behavior: "drop_block"`. The
API drops the blocks whose prefix changed (unbilled) and answers
without that reasoning; blocks produced afterwards are valid against
the new prefix, so the loss is once per change, not once per request.
Drops are counted from the response's `input_transformations` at
debug level. A rules edit, a folder bound or a compaction already
restarted the prompt cache; losing the earlier reasoning with it is
accepted.

**Live state is appended, never rewritten.** Tool descriptions and
the system prompt hold nothing that changes while the user works. The
list of open terminals moves into the conversation: a composer send
carries an `<open_terminals>` element in its `<context>` block when
the list differs from the last one the conversation holds
(`crates/moon-coder/src/terminal_context.rs`). The message is stored
as sent, so the prefix stays byte-stable. Anything live added later
follows the same route.

## Rejected alternatives

- **Go fully append-only** (freeze `system` and `tools` per session,
  persist them with it, deliver rules and folder changes as appended
  messages, hand compaction to the API). Keeps the reasoning across
  those changes too, at the price of reshaping the per-turn prompt
  refresh, compaction and the session file. Not worth it for changes
  this rare.
- **Drop the terminal list and let the model call `list_terminals`.**
  Costs a round-trip whenever terminals matter, and in write modes,
  where the tools are always advertised, the model no longer knows
  whether the user has a dev server running before it starts its own.
- **Announce terminal changes as a `[moon-ide]` notice message.** Same
  information, but a visible bubble at the top of nearly every
  session.
- **Retry on the 400** (send `"error"`, flip the session to
  `drop_block` when it fails). Same end state for any session whose
  prefix changed, plus a failed round-trip and a per-session flag to
  persist.
- **Strip thinking blocks client-side** when the prefix changes. Needs
  a prefix fingerprint stored per block to know which ones to remove;
  the API already has it.
