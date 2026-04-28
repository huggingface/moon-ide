# 0012 — Slack send messages (Phase 11.3)

The chat panel can finally talk back. Composer at the bottom,
Enter to send (Shift+Enter for a newline), "+ New session" to
start a fresh top-level thread. Detailed design lives in
[`slack-chat.md`](../slack-chat.md#sending-messages-phase-113).

## What ships

- New `SlackClient::post_message(channel, thread_ts?, text)`
  wrapping
  [`chat.postMessage`](https://api.slack.com/methods/chat.postMessage).
  The `chat:write` scope was granted upfront in 11.0 so no
  reinstall is needed. Returns the freshly-posted
  [`SlackMessage`] (normalised through the same `to_message` path
  every other read goes through, so the frontend doesn't need a
  reconciliation special case).
- New tauri command `slack_post_message(channel, thread_ts?, text)`
  - matching `ipc.slack.postMessage` binding.
- Frontend composer in `ChatPanel.svelte`:
  - Textarea pinned to the bottom of the panel; visible when
    `activeThreadTs !== null` (reply mode) or
    `composingNewSession === true` (new-session mode).
  - **Enter** sends; **Shift+Enter** inserts a newline;
    **Ctrl/Cmd+Enter** also sends (muscle-memory carry-over from
    other tools); **Esc** cancels the new-session composer.
  - Disabled while a post is in flight (the textarea greys out
    and rejects keypresses; no send button to click).
  - Send error renders inline above the textarea; draft is
    preserved so the user can retry.
  - Auto-grows from 1-line height up to a 120 px cap as the user
    types; shrinks back when lines are deleted.
  - Pinned to the bottom of the panel via the `connected-scroll`
    - `composer` flex split — empty thread space lives above
      the textarea, never between the textarea and the bottom of
      the window.
- "+ New session" button at the top of the session list when no
  thread is open. Toggles `composingNewSession`; first post pivots
  the panel into the new thread (sets `activeThreadTs` to the
  returned `ts`, replaces `threadMessages` with `[message]`,
  re-runs `loadSessions()`).
- New `slack.svelte.ts` actions: `sendMessage(text)`,
  `startNewSession()`, `cancelNewSession()`. State additions:
  `composingNewSession`, `sending`, `sendError`. All cleared by
  `disconnect` / bot-change so a stale draft can't leak across
  workspaces.
- Unit tests on the request shape:
  - `post_message_request_omits_thread_ts_for_top_level` — the
    JSON body MUST NOT contain `thread_ts: null` (Slack rejects
    that with `invalid_thread_ts`).
  - `post_message_request_includes_thread_ts_for_replies`.
  - `post_message_response_yields_a_normalised_slack_message` —
    the `chat.postMessage` echo round-trips through `to_message`
    cleanly.

## Setup

1. `bun run dev` (Tauri dev shell + Vite).
2. Connect Slack and select moon-bot from the picker (Phase 11.0
   flow, unchanged).
3. Open the chat panel (`Ctrl+L`). At least one existing session
   helps for reply-flow scenarios; if there are none, scenarios A
   and B below cover starting from scratch.

## Scenarios

### A — Send a reply in an existing thread

1. Click any session in the list. Wait for messages to load.
2. The textarea appears below the message list with placeholder
   `Reply…`. Click into it.
3. Type `hello world`. Press **Enter**.
   - Expected: the textarea clears and shrinks back to its 1-line
     resting height; the user's message appears immediately at
     the bottom of the message list with the **"You"** label
     (regular text colour, not the accent reserved for the bot
     name); a `formatSlackTime` stamp on the right.
   - **Regression watch — `bot_id`-on-self attribution.** Slack
     attaches a `bot_id` to messages posted via `chat.postMessage`
     from a user token bound to an app (which is what our
     `xoxp-` flow installs). The naive `is_bot = bot_id.is_some()`
     heuristic in `to_message` therefore flags our own messages
     as bot-authored. The frontend's `senderLabel` checks
     `user_id == self` _first_ so this still renders as "You";
     verify both immediately after send (optimistic append) and
     ~3 s later when the poll tick reconciles the same message
     from `conversations.replies` (which also carries the
     `bot_id`).
4. Verify in another Slack client (mobile, web, desktop Slack)
   that the message landed in the same thread.
5. Wait ~3 s for the next poll tick.
   - Expected: no flicker, no duplicate. The fingerprint
     `(ts, edited_ts)` matches what the panel already has, so
     the polling reconciliation no-ops.

### B — Start a new session

1. With no thread open, click **+ New session** at the top right
   of the session list.
   - Expected: the session list disappears; the panel shows a
     "New session" card explaining the new top-level post will
     create a thread, plus an empty composer focused
     automatically. The placeholder reads `Message Moon Bot` (or
     whatever the active bot's display name is — short,
     Slack-style).
2. Type `please review src/lib/foo.ts`. Press **Enter**.
   - Expected: the new-session card disappears; the panel pivots
     into a fresh thread containing only your message. The
     session list (when you go back to it) now has a new row at
     the top with your message as the preview.
3. Wait for moon-bot's reply. The cadence ladder is at 3 s
   because the new session is "hot"; the bot's response should
   appear within one tick.

### C — Cancel the new-session composer

1. Click **+ New session**. Don't type anything (or type then
   delete).
2. Press **Esc** _or_ click **← Cancel**.
   - Expected: returns to the session list. No new top-level
     message in Slack. The session list is unchanged.

### D — Multi-line message

1. Open any thread. Click into the composer.
2. Type `line one`, press **Shift+Enter**, type `line two`.
   - Expected: a literal newline goes in the textarea — Send is
     **not** triggered.
3. Press **Enter** (without Shift).
   - Expected: the message is sent with the newline preserved.
     In the rendered bubble after reconciliation, the two lines
     show on separate visual lines.
4. Optional muscle-memory check: instead of plain Enter, finish
   with **Ctrl+Enter** / **Cmd+Enter**.
   - Expected: also sends. The send chord is preserved for users
     coming from Slack's defaults.

### E — Empty / whitespace draft

1. Open a thread. Click into the composer.
2. Without typing, press **Enter**.
   - Expected: nothing happens. `sendMessage` short-circuits on
     `text.trim().length === 0`, so the keypress is just
     swallowed.
3. Type three spaces, press **Enter**.
   - Expected: same — nothing is posted, the spaces stay in the
     textarea.

### F — Send while disconnected

This one is fiddly to reproduce because the disconnect path
clears the panel back to the empty state, which removes the
composer. Two ways to hit it:

1. Block `slack.com` (e.g. via `/etc/hosts`). Try to send.
   - Expected: an error string ("Transport: …") appears above
     the composer; the draft is preserved; the Send button
     re-enables. Re-sending after restoring connectivity
     succeeds.
2. Manually expire the token (revoke the user app from Slack's
   account settings). The next poll tick or status probe will
   fire `slack:disconnected` and pull the panel back to the
   empty state — at which point the composer is gone, no draft
   to preserve. Acceptable: this is the "user explicitly broke
   their token" case.

### G — Switch session mid-draft

1. Open thread A. Type a partial draft, do not send.
2. Click another session in the back-button view (need to first
   click ← Sessions, type a draft, then go back).
   - Expected: drafts are **not** preserved across thread
     switches in v1. The textarea binds to a single `draft`
     state on the panel, not per-thread. Documented limitation;
     come back when somebody asks.

### H — New-session post + immediate reply

End-to-end "first conversation with the bot" flow:

1. Disconnect + reconnect Slack to start with a clean panel.
2. **+ New session** → type `ping` → Enter.
3. Watch the panel pivot into the new thread.
4. Without further interaction, wait for moon-bot's reply.
   - Expected: within one to two cadence ticks (3–6 s) the bot's
     response shows up in the panel; `conversations.mark`
     fires (verifiable: open the same DM in mobile Slack — no
     unread badge).

### I — Spec round-trip

After exercising the above, hard-reload the dev shell
(Ctrl+Shift+R or close + relaunch). The session created in
scenario B / H survives — `slack.active_thread_ts` is persisted
in `app_state.json`, so the panel reopens on the same thread.

## Known limitations (deliberate)

- No drafts persistence; navigating away from the composer
  loses the textarea content. Cheap to add — `localStorage` or
  `AppState` — but not requested.
- No optimistic "in flight" pip on the user's bubble. The
  `chat.postMessage` round-trip is fast enough that adding a
  pending state costs more than it pays. Fail goes through the
  inline error.
- No file / image attachments yet (deferred per `slack-chat.md`).
- Slack's mrkdwn is sent as-is. We don't translate Markdown
  (`**bold**` etc.) into mrkdwn (`*bold*`) — what the user types
  is what the bot sees.
- Plain Enter is Send, not a newline — diverges from Slack's
  own composer (where Enter is a newline and Ctrl+Enter sends).
  We followed team feedback and prioritised one-key send;
  Shift+Enter handles the multi-line case. Ctrl/Cmd+Enter is
  also accepted for users carrying Slack's muscle memory.
