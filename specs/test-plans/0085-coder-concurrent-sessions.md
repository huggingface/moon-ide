# Test plan 0085: concurrent coder sessions per folder

- **Date**: 2026-05-25
- **Phase**: Phase 6.x (coder polish)

## What shipped

Each bound workspace folder can now run **multiple coder sessions concurrently**, not just one. Clicking `+` for a new session or picking another row from the sessions list no longer aborts the previously-visible session's in-flight turn — it keeps running in the background and the user can come back to it whenever (see [ADR 0016](../decisions/0016-coder-concurrent-sessions.md)).

- **Per-session runtime in the backend.** `FolderSession` went from "one `Mutex<Session>` + one `Mutex<TurnState>`" to "many `Arc<SessionRuntime>` keyed by session id + a `visible: Option<String>` pointer". Each runtime owns its own session, its own cancel token, and is independently spawnable. `new_session` / `open_session` no longer cancel any other runtime — they allocate / look up a runtime and set it visible.
- **Per-session UI bucket on the frontend.** `FolderViewState` split into `FolderState` (folder-scoped: sessions list, view selector, attention rollup, `visibleSessionId`, `sessionsById: SvelteMap<sessionId, SessionViewState>`) and `SessionViewState` (per-session: transcript, busy, todos, context ring, sub-agent cards, composer draft, attachments). Event dispatcher routes by `(folder, session_id)` from the envelope; folder-scoped events arrive with empty `session_id` and route through a folder-level handler.
- **Wire format.** `CoderEventEnvelope` grew a `session_id: String` field. `PROTOCOL_VERSION` bumped to 1. Per AGENTS.md no-premature-migrations, no compatibility shim — both sides update together.
- **Sessions list pip.** Every session row whose turn is currently running paints the pulsing accent dot + `running…` label, not just the visible one. Test plan 0079's "only one row at a time" observation is superseded.
- **`abort` semantics.** Still cancels the active folder's **visible** session only. Stopping a background turn requires switching to it (clicking its row in the sessions list) and then hitting Esc / stop. Sign-out is the global escape hatch and cancels every running session in every folder.

## How to test

Prerequisites: `cargo test -p moon-coder`, `cargo clippy --workspace --all-targets`, `bun run check`, `bun run lint` all clean.

### 1. The bug the user reported

1. Open a folder in moon-ide, sign in to the coder, click `+` for a fresh session.
2. Send a prompt that runs long enough to observe — e.g. `read AGENTS.md, then specs/coder.md, then summarise the differences in one paragraph`. The agent should call `read_file` at least twice and then start streaming an assistant message.
3. **While the turn is mid-stream**, click `+` in the panel header (top-right of the session view) to start a second session in the same folder.
   - Expected: the panel switches to a blank session. The previous session's turn is **not** aborted — no `Aborted` row is written into its transcript on disk (you can confirm by opening the trace via `</>` and looking at the JSONL tail later).
   - Expected: clicking "← Sessions" shows both rows in the list. The previous session's row paints the pulsing pip + `running…` label until the agent finishes; the new (blank) session's row is dormant.
4. Click the previous session's row to make it visible again. Expected: the transcript appears in its current state (whatever the still-running agent has produced by now), and any deltas that arrive after you switch back continue to land in the transcript live.
5. Wait for the turn to complete. Expected: `turn_complete` lands in the originally-running session's bucket. If you're looking at it, the stop button disappears and the composer is enabled. If you switched away to the new blank session, the folder-bar pip stays painted as attention (background turn finished while away).

### 2. Three concurrent turns in the same folder

1. From the sessions list, click `+`. Send a quick prompt (`list specs/test-plans`). Don't wait for it to finish.
2. Click `+` again. Send another quick prompt (`list specs/decisions`).
3. Click `+` a third time. Send a third prompt (`list specs/roadmaps`).
4. Click "← Sessions". Expected: three rows, all painting the pulsing pip + `running…` label simultaneously. Each row's title is the truncated prompt from its first message.
5. Click into each one. Expected: each row's transcript shows that session's own conversation — no cross-contamination, no shared tool calls, no shared assistant deltas.
6. Wait for all three to finish. Expected: every row's pip disappears as its `turn_complete` lands. The folder-bar pip stays painted as attention until you visit a session whose turn finished while you were elsewhere.

### 3. Switch projects mid-turn (cross-folder concurrency, still works)

This was already supported before this change. Confirming we didn't break it.

1. With folder X open, kick off a long-running prompt in some session.
2. Switch to folder Y. Expected: the panel renders folder Y's last-visible session (or its sessions list if there's nothing to restore). Folder X's turn keeps streaming events into X's bucket.
3. Switch back to folder X. Expected: the transcript is up to date with everything that streamed while you were away.

### 4. Stop semantics — visible only

1. In folder X, start two concurrent turns (sessions A and B). Make A visible (click it in the list).
2. Hit Esc (or click the stop button). Expected: A's turn cancels, A's transcript gets an `Aborted` row. B is **untouched** and still streaming.
3. Switch to B. Confirm B is still running (the stop button is visible, the transcript is still receiving deltas).
4. Hit Esc in B. Expected: B cancels too.

### 5. Sign-out cancels everything

1. Start two concurrent turns in folder X. Optionally start one in folder Y too.
2. Click the sign-out button.
3. Expected: every running turn across every session in every folder receives `Aborted`. The panel falls back to the sign-in state. Coming back in (sign in again), the sessions list re-paints from disk; no `running…` pips remain since no runtime exists for any session yet.

### 6. Delete a running session

1. Start a long turn in some session.
2. Without making any other session visible, scroll the sessions list (you may need a second session to see this — `+` then back). Find the still-running session's row.
3. Hover the row, click the trash icon, confirm the dialog.
4. Expected: the session's JSONL is removed from disk, the row disappears from the list, the runtime is dropped (its turn is cancelled as a side effect). If the deleted session was the visible one, the panel falls back to the sessions list view (or the blank/empty state when the list is empty).

### 7. Re-open a background session via the list

1. Start a long turn in session A. Click `+` for a new session B. Click "← Sessions". A's row shows `running…`.
2. Click A's row.
3. Expected: the transcript view re-binds to A. Replay events fire (the panel sees `SessionLoaded` plus the historic event stream) so the transcript repopulates from disk. **And** live events from A's still-running turn keep landing on top — when the agent emits the next delta / tool result, you see it appear immediately.
4. Hit Esc. Expected: A's turn cancels (it was the visible session at the time of the abort gesture).

### 8. Cold start with a previously-running background session

1. Start a long turn. Don't wait for it.
2. Quit moon-ide hard (kill the process, or close the window — anything that ends the Tauri process).
3. Reopen the folder.
4. Expected: the panel restores the last-visible session per [`last_session_by_folder`](../coder.md#multi-session-per-project). Other sessions are listed but not pre-hydrated. The previously-running turn was killed when the process exited — no `running…` pip survives the restart. The JSONL on disk shows whatever made it through before the kill; reopening the session may show orphan tool calls (handled by the existing orphan-recovery path).

### 9. Steers still steer the visible session, not background sessions

1. Start a long turn in session A. Don't switch away.
2. Type a follow-up into the composer and hit Enter while A's turn is still running.
3. Expected: the message lands as a queued steer in A's transcript (muted style); A's running turn will drain it at the next iteration top. No effect on any other session.
4. From the sessions list, click `+` to make a fresh session B visible. Type a prompt and send.
5. Expected: B's transcript starts a new turn. A's pending steer still lives in A's runtime; switching back to A shows it queued; the agent in A picks it up on its next drain.

## What must keep working

- All Phase 6.x parent behaviours: streaming, auto-rename, attachments (selection / image / terminal), sub-agent spawning and the popped-out transcript, hub bucket sync, model picker, token-usage ring, auto-compaction.
- Test plan 0050 sub-agents flow — parent runs, dispatches `task`, sub-agent cards render inline, pop-out transcript works. Sub-agent events arrive tagged with the **parent's** session id, so a parent in session A's sub-agents still render under A even when session B is visible.
- Test plan 0079 sub-agent JSONL persistence + open-session replay.
- The folder-bar `attentionPending` sparkle for background completions in non-active folders. Still rolled up at the folder level, not per session — a user juggling agents in multiple folders sees one pip per folder, with the granularity inside the sessions list.
- `coder_sign_out` still cancels every running turn across the process.

## Known limitations

- **No per-row stop button.** Stopping a background turn requires opening it first. Two clicks in the worst case. A per-row stop affordance is deferred until the volume of concurrent agents justifies it.
- **No cold-start preservation of background turns.** A process restart kills all in-flight turns regardless of which sessions they belonged to. The runtime map is in-memory only; survival across restart would require a workspace-bus protocol we don't have. The visible session per folder still restores via the on-disk JSONL.
- **No `running` pip on the folder bar disambiguating "this folder has N running sessions" from "this folder has one".** The folder-bar pulsing pip means "anything is running here"; the sessions list inside the folder is where the per-session granularity surfaces.
- **Per-session composer draft.** Phase 6 made the draft per-folder; this ADR makes it per-session. There is no migration of a previously-saved per-folder draft into any specific session — drafts only live in memory anyway, so the first launch after the change starts with empty drafts everywhere.
- **No concurrency cap.** The user can in principle start as many turns as they want in the same folder. Memory cost is per-runtime cheap (a `Mutex<Session>` + a `Mutex<TurnState>` + the rebuilt `messages: Vec<ChatMessage>`); provider rate limits throttle the network side. Practical experience will tell us whether a soft cap is worth adding.

## Related

- [ADR 0016 — Concurrent coder sessions per folder](../decisions/0016-coder-concurrent-sessions.md) — the architectural decision this plan exercises.
- [specs/coder.md § Multi-session per project](../coder.md#multi-session-per-project) — rewritten in the same commit to describe the per-session shape.
- [test plan 0079](0079-coder-host-paths-and-task-rename.md) — its § D "Running pip" observation is superseded here.
- [test plan 0050](0050-sub-agents.md) — sub-agent invariants this plan inherits.
- [test plan 0079](0079-coder-host-paths-and-task-rename.md) — sub-agent JSONL persistence the open-session-while-running path piggybacks on.
