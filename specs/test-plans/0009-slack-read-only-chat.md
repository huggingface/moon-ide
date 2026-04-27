# Test plan 0009: Slack read-only chat (sessions + threads)

- **Date**: 2026-04-27
- **Phase**: Phase 11.1

## What shipped

- Two new endpoints on `SlackClient`
  ([`crates/moon-slack/src/client.rs`](../../crates/moon-slack/src/client.rs)):
  - `list_sessions(channel)` — wraps `conversations.history?limit=100`
    (`SESSION_HISTORY_LIMIT`), filters to top-level messages
    (`thread_ts == ts || thread_ts is None`), maps to
    [`SlackSession`](../../crates/moon-protocol/src/slack.rs)
    `(thread_ts, latest_ts, preview, reply_count, user_id)`. No
    cursor walks — one page is enough until somebody asks for more.
  - `get_thread(channel, ts)` — wraps `conversations.replies?limit=200`
    (`THREAD_REPLY_LIMIT`), maps to
    [`SlackMessage`](../../crates/moon-protocol/src/slack.rs)
    `(ts, user_id, text, edited_ts, is_bot)`. `is_bot` comes
    straight off Slack's `bot_id` field.
  - Preview text is truncated server-side at 80 chars
    (`PREVIEW_MAX_CHARS`) with whitespace runs collapsed; the panel
    just renders what it gets.
- New protocol types
  ([`crates/moon-protocol/src/slack.rs`](../../crates/moon-protocol/src/slack.rs)):
  `SlackSession`, `SlackMessage`. Both are `#[derive(TS)]`-exported.
- `AppState.slack` grows an `active_thread_ts: Option<String>`. Cleared
  on bot switch (sessions live inside one bot's DM channel — a new
  bot inherits no thread) and on disconnect.
- Three new tauri commands
  ([`src-tauri/src/commands/slack.rs`](../../src-tauri/src/commands/slack.rs)):
  - `slack_list_sessions(channel)` — fronts `list_sessions`.
  - `slack_get_thread(channel, threadTs)` — fronts `get_thread`.
  - `slack_set_active_thread(threadTs | null)` — load-mutate-save of
    `AppState.slack.active_thread_ts`. Idempotent (early return if
    unchanged).
  - `slack_select_bot` now also clears `active_thread_ts` whenever
    the bot identity actually changes.
- Frontend
  ([`src/lib/slack.svelte.ts`](../../src/lib/slack.svelte.ts)):
  - `SlackPanelState` grows `sessions`, `loadingSessions`,
    `sessionsError`, `activeThreadTs`, `threadMessages`,
    `loadingThread`, `threadError`. Two private generation
    counters discard late responses when the user has switched
    bots or threads.
  - `loadSessions()`, `selectThread(threadTs | null)`,
    `loadThread(threadTs)`. `selectThread` persists via
    `slack_set_active_thread` fire-and-forget.
  - `hydrate(state)` now restores `active_thread_ts` alongside
    `active_bot` and `panel_visible`.
- UI ([`src/lib/components/ChatPanel.svelte`](../../src/lib/components/ChatPanel.svelte)):
  - Sessions section under the bot card: clickable rows with
    preview + relative time + reply count.
  - Thread view: bubble per message, bot bubbles get a tinted
    background, `(edited)` marker on `edited_ts`, monospace
    timestamp, "← Sessions" back button.
  - `$effect` block auto-loads sessions / the persisted thread
    whenever the panel becomes visible with a connected bot.
  - Per-minute `nowTick` re-renders relative timestamps.
  - Tiny new utility
    [`src/lib/util/slackTime.ts`](../../src/lib/util/slackTime.ts):
    `formatSlackRelative` (cap at "1 week ago", switches to date
    after) and `formatSlackTime` (24-hour `HH:MM`).
- Spec updates: roadmap row for 11.0 was rewritten to reflect what
  actually shipped (DM-first picker, keyring backends), and the 11.1
  description points at this test plan.

## Out of scope (deferred to later 11.x)

- **No polling.** The thread snapshot is what the user got at
  load time. Refresh button is wired but it's a manual one-shot.
  Polling + edit detection lands in 11.2.
- **No `conversations.mark`.** The Slack unread badge in the user's
  other clients does not clear when reading in moon-ide. Same
  phase.
- **No sending.** The input box doesn't exist yet (11.3).
- **No mrkdwn rendering.** Bot messages render as raw text with
  `\n` preserved. Code fences, links, mentions, custom emoji all
  show as their literal `\``-style source. Markdown rendering is
11.4 and will reuse `markdown.ts`.
- **No attachments.** Images, files, blocks all render as their
  text fallback or empty. Attachment rendering is 11.4+.
- **No avatars on messages.** The bot's avatar lives on the bot
  card up top. Per-bubble avatars wait until we have a real
  multi-author thread story (group DMs are out of scope).

## Pre-test setup

Same one-time setup as plan 0008. You should already have:

- A Slack workspace with at least one bot you've DM'd (Moonbot for HF).
- A `xoxp-` user token with the scopes from `slack-chat.md`.
- The IDE running, Slack connected, and the bot picked from the picker.

If you're starting from a fresh clone, run plan 0008's setup section
first — 0009 builds on top of it.

For the polling/sending tests in later plans you'll want a real bot
that replies. For 0009, any thread with at least one user→bot turn
is enough; you can also just look at threads created by past
moon-ide use.

## Tests

### 11.1a — Sessions list paints

1. Launch with the chat panel open and a bot already picked from
   plan 0008.
2. **Expected**: under the bot card, a "Sessions" section appears
   with a "Refresh" button on the right. While loading, a spinner +
   "Loading sessions…". Within ~2s, replaced by a list of session
   rows, each showing:
   - Preview text (one or two lines, ellipsised).
   - Relative time ("just now" / "5 min" / "yesterday" / "Apr 14").
   - Reply count when > 0 (e.g. "3 replies").
3. **Expected**: order is newest-first (Slack's natural order).
4. **Expected**: the bot card itself is unchanged from 11.0.

If you don't have any DM history with the bot, the section reads
"No sessions yet. Start one by DMing **\<bot name\>** from regular
Slack — sending will land in the IDE in 11.3." That's also a pass.

### 11.1b — Thread opens

1. From 11.1a, click any session row.
2. **Expected**: the sessions section is replaced by a thread
   section: a "← Sessions" back button at the top-left, a Refresh
   button at the top-right, the session's preview as a muted
   subtitle, then a list of message bubbles.
3. **Expected**: bubbles alternate user / bot:
   - User bubbles say "You" in the header.
   - Bot bubbles say the bot's display name (same as the bot card)
     and have a tinted background.
   - Each header has an `HH:MM` timestamp on the right.
4. **Expected**: text is rendered with newlines preserved but no
   markdown processing — backticks, asterisks, link syntax are
   visible verbatim. (That's 11.4.)
5. Pick a thread you know contains a Slack edit (or have the bot
   edit a message via Slack). **Expected**: the edited message has
   "· edited" after its timestamp.
6. Click "← Sessions". **Expected**: back to the sessions list,
   thread is gone, scroll position of the sessions list is preserved.

### 11.1c — Active thread persists across restart

1. Open a thread (11.1b).
2. Quit the app cleanly (Cmd/Ctrl+Q or window close).
3. Relaunch.
4. **Expected**: chat panel opens with the bot card visible, and
   the same thread you had open is open again — no flash of the
   sessions list, no manual click. (Sessions list still loads in
   the background so "← Sessions" works.)
5. **Verify on disk**:

   ```bash
   cat ~/.config/dev.moon-ide.desktop/state.json | jq .slack
   ```

   Expected to look like:

   ```json
   {
     "active_bot": { "user_id": "U…", "dm_channel_id": "D…", … },
     "panel_visible": true,
     "active_thread_ts": "1700000001.000100"
   }
   ```

6. Click "← Sessions" so no thread is selected. Quit + relaunch.
7. **Expected**: panel opens on the sessions list (no thread).
   `state.json` shows `"active_thread_ts": null`.

### 11.1d — Switching bots clears the thread

1. Open a thread for bot A (11.1b).
2. Click "Switch bot" on the bot card.
3. **Expected**: picker reappears with the DM scan results.
4. Pick bot B (different `user_id`).
5. **Expected**: sessions list for bot B paints, no carry-over of
   bot A's thread, `state.json` `active_thread_ts` is `null` or
   absent.

(If you only have one bot, this test is best skipped — you can
also fake it by `slack_clear_bot` from the command palette and
re-picking the same bot, but the bot identity hasn't changed so
the thread is preserved by design.)

### 11.1e — Disconnect clears the thread

1. Open a thread.
2. Click "Disconnect" → confirm.
3. **Expected**: panel returns to the empty "Connect Slack" state.
4. Reconnect with the same token, pick the same bot.
5. **Expected**: panel opens on the sessions list, no thread
   selected (the disconnect cleared the persisted thread).

### 11.1f — Generation counters: rapid bot switch

(Stress test for the race-condition guard. Skip if you don't have
two bots.)

1. With bot A's thread open, click "Switch bot" → pick bot B
   immediately, before bot A's session list (if it was reloading)
   completes.
2. **Expected**: the panel shows bot B's sessions list. No flash
   of bot A's sessions, no "thread not found" stall, no
   intermittent error toast.
3. Click a session under bot B. **Expected**: bot B's thread
   paints; no carry-over of bot A's messages.

### 11.1g — Refresh buttons are one-shot

1. On the sessions list, click "Refresh".
2. **Expected**: list reloads (spinner briefly, list repaints).
   Active thread, if any, is unaffected.
3. Open a thread, click "Refresh" inside the thread view.
4. **Expected**: thread messages reload from Slack — useful when
   you've edited from regular Slack and want to see the change
   without restart. (Auto-detection comes in 11.2.)

### 11.1h — Empty / error states

1. Disconnect Wi-Fi (or any other network kill switch). Click
   "Refresh" on the sessions list.
2. **Expected**: red error message under the section header (Slack's
   error text or a transport message). The previously-loaded
   sessions stay rendered if any — no flicker to empty.
3. Reconnect, click Refresh again. **Expected**: error clears,
   list paints normally.

### 11.1i — Relative timestamps tick

1. Open the panel with sessions visible. Note the relative time on
   the most-recent row ("just now" / "5 min").
2. Wait at least one minute.
3. **Expected**: the time updates ("just now" → "1 min", "5 min" →
   "6 min") without manual refresh. No layout jank, no spinner.

### 11.1j — Type-aware checks pass clean

```bash
bun run check && bun run lint && cargo test --workspace --exclude moon-desktop && bun run fmt:check
```

All four commands exit 0. The Rust check should include `moon-slack`
test cases for the new history/replies deserialisers:

- `deserializes_history_and_filters_top_level`
- `deserializes_replies_with_edits_and_bots`
- `preview_truncates_and_collapses_whitespace`

## Known limitations

- The thread view scrolls naturally inside the panel; no
  jump-to-latest button (the panel is short enough that scrolling
  is cheap). Add when polling lands and scroll position becomes
  load-bearing.
- The session-list preview can be empty for image-only / file-only
  parents — Slack returns no `text` field in that case. We render
  "(empty message)" so the row stays clickable.
- Refresh buttons are explicitly manual. Phase 11.2 replaces them
  with the polling loop and removes them (or keeps them as power-user
  affordances — TBD).
