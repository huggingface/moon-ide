# 0011 — Slack polling + read receipts (Phase 11.2)

Background poll loop that keeps the active thread fresh without
manual refresh, plus `conversations.mark` so the user's other
Slack clients drop the unread badge as soon as they've seen the
message in moon-ide. Detailed design lives in
[`slack-chat.md`](../slack-chat.md#cadence-ladder).

## What shipped

- Active thread stays fresh without manual refresh: a new
  `slack_poller` Tokio task runs a cadence ladder (3 s hot → 5 s
  warm → 15 s → 60 s → paused cold) and pushes
  `slack:thread-update` events when the `(ts, edited_ts)`
  fingerprint moves.
- Read receipts: `conversations.mark` fires on view and on the
  next poll tick of a focused, panel-visible thread, so unread
  badges on the user's other Slack clients clear in real time.
  Deduped per-`(channel, thread_ts)` so session switches don't
  spam.
- Window focus is forwarded from the frontend (`tauri://focus` /
  `blur`) and gates the read-receipt write — polling still
  runs in the background, but marks only fire when the user is
  actually looking.
- Auth failures discovered by the poll loop take the same
  clear-and-reset path as `slack_status` (drop cache, keyring,
  bot pick, emit `slack:disconnected`), so a revoked token
  lands the user back on the empty state without a manual
  probe.
- Startup hook replays `panel_visible`, `active_bot.dm_channel_id`,
  and `active_thread_ts` into the poller before the frontend
  mounts, so a relaunch polls within 3 s of first paint.

## Setup

- Connected Slack with a bot picked + a session open
  (per the previous test plans).
- Network is up; no rate-limit warnings in the tracing logs.

## Scenarios

### 11.2a — Fresh-thread polling

1. From regular Slack, send a new message to the bot in an
   already-open thread.
2. **Expected**: within ~3 s, the new message appears in the
   moon-ide thread without clicking Refresh. The "Refresh thread"
   affordance is still there (and still works) but is now a
   power-user fallback.

### 11.2b — Edit detection

1. From regular Slack, edit a recent bot reply (or a message
   you typed earlier).
2. **Expected**: within ~3 s the edited content replaces the
   original. The relative time in the message header doesn't
   change (we only track `edited.ts`, not `ts`).

### 11.2c — Cadence ladder downshift

1. Open a thread that hasn't seen activity in > 1 hour.
2. Watch the network panel (or `tracing` logs at `debug`) — the
   "cold" state shows no auto-polls.
3. Send a new message from regular Slack.
4. **Expected**: nothing happens automatically (the panel is
   paused). Click the thread (or any other interaction) to
   un-pause; the next 3 s tick brings the message in.

### 11.2d — Cadence ladder upshift

1. Sit on a thread cold (> 1 h).
2. Click into it. Watch the logs.
3. **Expected**: the loop wakes up, polls within 3 s, and goes
   into the "hot" 3 s cadence on the next activity. Subsequent
   inactivity moves through 5 s → 15 s → 60 s → paused as the
   table in `slack-chat.md#cadence-ladder` describes.

### 11.2e — Panel-hidden pauses the loop

1. Open a thread, observe the 3 s polling tick.
2. Hide the chat panel (Ctrl+L or status-bar toggle).
3. **Expected**: polling stops immediately (no further
   `conversations.replies` calls in the network panel). Re-show
   the panel; the loop resumes within 3 s.

### 11.2f — No-session pauses the loop

1. With the panel open, click "Sessions" to leave the active
   thread.
2. **Expected**: polling stops. Picking a session re-arms the
   loop.

### 11.2g — Read receipt on view

1. From regular Slack, send a new message to the bot. Watch the
   unread badge appear in your other Slack client (mobile, web).
2. Switch to moon-ide and click the session. The thread loads.
3. **Expected**: within a couple of seconds, the unread badge
   clears in your other Slack client. moon-ide called
   `conversations.mark` on the latest message ts.

### 11.2h — Read receipt only when focused

1. Open the thread in moon-ide so it's the active session.
2. Click into another OS window (Slack desktop, the terminal,
   another browser tab — anything that takes focus away).
3. From regular Slack, send a new message. Note: the panel is
   still visible inside moon-ide, just unfocused.
4. **Expected**: the new message appears (polling still runs)
   but the unread badge in your other Slack client does **not**
   clear. Only when you alt-tab back to moon-ide does the next
   poll tick mark it read.

### 11.2i — Read receipt dedup

1. Open a thread; observe the badge clears.
2. Switch to a different session, then back. The thread reloads.
3. **Expected**: no extra `conversations.mark` call. The
   `#lastMarkedByThread` cache short-circuits the redundant
   write. Verify by capturing the network panel (or by tailing
   the `slack` debug logs).

### 11.2j — Auth failure mid-poll

1. With the panel open and a thread active, revoke your
   `xoxp-` token from Slack's app management page (or rotate
   the token without updating moon-ide).
2. **Expected**: within one poll tick the panel returns to the
   "Connect Slack" empty state, the keyring entry is dropped,
   and `app_state.json` no longer carries `slack.active_bot` /
   `slack.active_thread_ts`. Reconnecting with a fresh token
   relands at the bot picker.

### 11.2k — Cross-restart resume

1. With a thread open and the panel visible, quit moon-ide.
2. Relaunch.
3. **Expected**: the panel re-opens to the same thread, polling
   resumes within 3 s, and the read-receipt path fires for any
   message that arrived while the app was closed.

### 11.2l — No double-listen on HMR

(Dev-only.) Save any source file to trigger Vite HMR while the
chat panel is open and a thread is active.

- **Expected**: only one Tauri event listener for
  `slack:thread-update` is bound at any time (we'd see double
  `applyThreadUpdate` calls otherwise — visible as flicker if
  the second listener fires from an out-of-date closure). The
  `#runtimeWired` guard makes `wireRuntime` idempotent.

### 11.2m — Type-aware checks pass clean

```bash
bun run check && bun run lint && bun run test:js && cargo test --workspace && bun run fmt:check
```

All five commands exit 0. The new Rust unit test
`cadence_ladder_matches_spec` covers the cadence breakpoints
exhaustively; integration coverage of the loop itself happens
through manual scenarios above (the loop interacts with Slack's
live API, mocking it would be heavier than the value).

## Known limitations

- **No exponential backoff on transport / 5xx errors.** The
  cadence ladder already throttles cold threads to 60 s, which
  is plenty of headroom for transient outages. Add a real
  backoff once we see real outages bite.
- **No session-list polling.** Only the active thread is kept
  fresh; the session list refreshes on panel mount and via the
  manual "Refresh sessions" button. Polling sessions globally
  would burn quota for marginal benefit (the user can't see
  unread badges on other sessions yet — that's Phase 11.4).
- **No `conversations.mark` on subsequent reactions.** Slack
  treats reactions as separate events; we don't poll for them
  (`reactions:read` is granted but unused — Phase 11.4).
- **Window focus is checked at poll time, not at mark time.** If
  the user blurs between when the message arrived and when the
  next poll tick fires, we still mark. This is the right call
  for the dominant case (user reads, then switches windows) and
  the failure mode is innocuous (mark fires slightly too
  generously, never silently misses an unseen message — the
  poll-tick gate already requires the message to have arrived
  while focused).
