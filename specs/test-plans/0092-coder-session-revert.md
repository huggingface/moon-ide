# Test plan 0092: coder session revert + edit & resend

- **Date**: 2026-06-04

## What shipped

- Each user message in a coder transcript reveals two hover actions: **Revert to here** (undo-arrow) and **Edit & resend** (pencil). Both rewind the session to just before that message, dropping it and everything after from disk and memory.
- "Revert to here" confirms via a modal (it permanently rewrites the on-disk JSONL); "Edit & resend" skips the modal and drops the prompt's text back into the composer for the user to tweak and re-send.
- Backend: `sessions::truncate_before_user_record` rewrites the JSONL from the header plus the surviving records; `Coder::revert_to_message` refuses mid-turn, truncates, then reuses the `open_session` reload to replay the trimmed transcript to the panel.
- Anchor is the 0-based ordinal of the user message among the transcript's user records (reload-stable), not a row id (which is minted fresh on every replay).
- New Tauri command `coder_revert_to_message(user_ordinal) -> RevertedMessage`.

## How to test

1. Open the coder panel, sign in, and start a fresh session. Send three short prompts in sequence, letting each turn finish: e.g. `say one`, `say two`, `say three`. You now have three user bubbles with assistant replies between them.
2. **Revert.** Hover the **second** user bubble (`say two`). Two small icon buttons appear on its `you` label. Click the undo-arrow ("Revert to here"). Confirm the modal. Expected: the transcript snaps back to ending at the first turn's assistant reply — the `say two` and `say three` bubbles and their replies are gone.
3. **Persistence.** Click the `</>` "open trace" button (active-session header) to open the raw JSONL. Expected: only the header line plus the first user/assistant pair remain. Close the trace.
4. **Reload survives.** Switch to another folder (or restart the IDE) and reopen the session. Expected: the trimmed transcript reloads — no resurrected `say two` / `say three`.
5. **Edit & resend.** Send two more prompts so you have a couple of turns again. Hover the most recent user bubble, click the pencil ("Edit & resend"). Expected: that bubble (and anything after) disappears, and its text lands in the composer with focus there. Edit the text and press Enter. Expected: a fresh turn runs against the edited prompt; the transcript now ends with the new exchange.
6. **Empty the session.** Hover the very first user bubble, "Revert to here", confirm. Expected: the transcript is empty (back to the blank-session state). Send a new prompt. Expected: it persists normally (the header line was preserved, so the append path works) and a new turn runs.
7. **Mid-turn guard.** Send a long-running prompt (e.g. "run `sleep 20` via bash"). While it's streaming/running, confirm the revert/edit icons are **not** shown on any user bubble. Stop the turn (Esc). Expected: the icons reappear once the turn settles.

## What must keep working

- Reverting the 0th user message leaves the JSONL header intact, so the next `send` appends rather than re-writing a header (no duplicate-header corruption).
- An out-of-range ordinal is a clean error and leaves the transcript untouched (covered by `truncate_before_user_record_rejects_out_of_range`).
- Sessions written before this feature reload unchanged — the revert path is additive and never runs unless the user clicks the affordance.
- Queued steers (sent mid-turn, not yet drained) never expose the revert icons, and the ordinal counter skips them so it stays aligned with the backend's `User`-record count.
- Auto-rename, sub-agent cards, todos, and the context ring all rebuild correctly after a revert (they ride the same `open_session` replay).

## Known limitations

- Revert is only available on the **visible** session at rest; you can't revert a background turn's session without making it visible and stopping its turn first (matches `abort`'s scoping).
- No undo of a revert — the truncation is a permanent JSONL rewrite. The user keeps the dropped text only when they chose "Edit & resend" (it's in the composer).
- Sub-agent transcripts can't be reverted independently; reverting the parent past a `task` call drops the sub-agent's card along with the rest of the trimmed tail, but the sub-agent's own JSONL on disk is not pruned (forensic-only, same as today's delete-vs-subdir behaviour for finished sub-agents within a kept turn).

## Related

- Spec: [`specs/coder.md` § Revert and edit & resend](../coder.md#revert-and-edit--resend), [§ Frontend ↔ backend boundary](../coder.md#frontend--backend-boundary).
- Code: [`crates/moon-coder/src/sessions.rs`](../../crates/moon-coder/src/sessions.rs) (`truncate_before_user_record`), [`crates/moon-coder/src/runner.rs`](../../crates/moon-coder/src/runner.rs) (`revert_to_message`).
- Prior coder session plans: [0043-coder-sessions.md](0043-coder-sessions.md), [0085-coder-concurrent-sessions.md](0085-coder-concurrent-sessions.md).
