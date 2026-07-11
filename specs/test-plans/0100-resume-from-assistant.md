# Test plan 0100: resume from a mid-turn agent response

- **Date**: 2026-07-08

## What shipped

- A mid-turn assistant row (one whose `tool_calls` are non-empty and are followed by `Tool` records before the next user message) reveals a hover affordance: **Replay from here** (circular-arrow glyph, same icon as the user-message replay). This is a different operation from the user-message replay — it resumes the tool-loop from that checkpoint rather than re-sending a prompt.
- Backend: `sessions::truncate_before_assistant_record` rewrites the JSONL to keep everything up to and including the target `Assistant` record, dropping its `Tool` records and everything after. `Coder::resume_from_assistant` auth-gates before the truncation, refuses mid-turn, truncates, re-opens the session (so the trimmed transcript repaints), strips the orphan-recovery synthetic `Tool` messages from `messages`, then spawns the turn loop with the kept `Assistant`'s `tool_calls` as a `resume_tool_calls` parameter. The first loop iteration re-dispatches those tool calls via `dispatch_tool_calls`; subsequent iterations make normal LLM calls with the fresh tool results in context.
- Anchor is the 0-based ordinal among assistant records with non-empty `tool_calls` (matching the backend's `truncate_before_assistant_record` count), not a row id.
- New Tauri command `coder_resume_from_assistant(assistant_ordinal)`.
- `run_turn` gained a `resume_tool_calls: Option<Vec<ToolCall>>` parameter. When `Some`, the first iteration dispatches those calls instead of calling the model; `take()` ensures only the first iteration sees them.
- No confirm (tool calls re-execute fresh against current workspace state, nothing is lost — same posture as replay-from-message).

## How to test

1. Open the coder panel, sign in, and send a prompt that triggers multiple tool calls across several round-trips — e.g. "read the files in src/ and tell me what each one does". Let the turn finish. You should see: user bubble, assistant text + tool-call rows (read_file), assistant text + more tool-call rows, final assistant answer.

2. **Affordance visibility.** Hover each assistant row. Expected: only the mid-turn assistant rows (ones with tool rows after them) show the replay glyph on hover. The final answer row (no tool rows after it) does **not** show the glyph. The glyph is hidden while a turn is in flight.

3. **Resume from checkpoint.** Hover the **first** mid-turn assistant row (the one whose tool calls already ran). Click the replay glyph. Expected: the transcript snaps back to end at that assistant row — its tool results and everything after are gone. Then the tool calls immediately re-execute (you see fresh `tool_call` / `tool_result` rows stream in), and the turn loop continues — the model gets the fresh tool results and produces a new response. No confirm dialog appeared.

4. **Fresh results.** If you changed a file between the original turn and the resume, the re-dispatched `read_file` calls should return the **current** file contents, not the stale ones from the original turn. This is the key difference from replay-from-message (which re-sends the prompt and gets a completely fresh turn) — resume reuses the model's existing tool-call decisions but runs them against current state.

5. **Persistence.** After the resumed turn finishes, click the `</>` "open trace" button. Expected: the JSONL shows the kept prefix (up to and including the target Assistant), then fresh Tool records from the re-dispatch, then the new assistant responses. No stale Tool records from the original turn.

6. **Reload survives.** Switch to another folder and reopen the session. Expected: the trimmed + resumed transcript reloads correctly — the kept prefix, the re-dispatched tool results, and the new responses. No resurrected dropped rows.

7. **Auth gate.** Sign out, then try resuming from a mid-turn assistant row. Expected: a clean error (same as replay-from-message when signed out) — the JSONL is **not** truncated.

8. **Mid-turn refusal.** Start a turn, and while it's running try to resume from a mid-turn assistant row. Expected: the hover affordance is hidden (same as the user-message affordances). If you somehow trigger it (e.g. via IPC), the backend returns an error.

9. **Final answer row.** Hover the last assistant row (the final answer, no tool calls after it). Expected: no replay glyph. Resuming from a tool-call-less assistant is meaningless — the turn already ended there.

## Edge cases

- A turn with only one assistant round-trip (user → assistant with tools → tool results → final answer): the first assistant row is mid-turn (it has tool rows after it) and is eligible for resume. Resuming re-dispatches its tool calls and continues.
- A tool-only assistant turn (no text, just tool calls): the row is skipped in rendering (`hasThinking || hasText` is false), so no affordance shows. This is correct — the user can resume from the next visible assistant row that had tool calls.
- Sub-agent (`task`) tool calls: resuming re-dispatches the `task` call, which spawns a fresh sub-agent. The old sub-agent's JSONL on disk is not pruned (forensic-only, same as revert's behaviour for sub-agents).

## Related

- Spec: [`specs/coder.md` § Revert, replay, and edit & resend](../coder.md#revert-replay-and-edit--resend), [§ Resume from a mid-turn agent response](../coder.md#resume-from-a-mid-turn-agent-response).
- Code: [`crates/moon-coder/src/sessions.rs`](../../crates/moon-coder/src/sessions.rs) (`truncate_before_assistant_record`), [`crates/moon-coder/src/runner.rs`](../../crates/moon-coder/src/runner.rs) (`resume_from_assistant`, `run_turn` `resume_tool_calls` parameter).
- Prior test plans: [0092-coder-session-revert.md](0092-coder-session-revert.md) (the original revert/replay mechanism this extends).
