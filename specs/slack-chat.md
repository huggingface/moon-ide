# Slack chat panel

The right-side chat panel is a thin Slack Web API client that lets a
moon-ide user DM a bot (Hugging Face's
[Moonbot](https://github.com/huggingface/moon-bot) or any DM-able
bot) without leaving the IDE. We're a chat client, not an agent
host: the bot already runs somewhere and Slack is the transport.

The bot has **zero visibility into local IDE state** — no file
context, no LSP. Real "agent in the IDE with context" is the
[coder panel](coder.md) and stays separate.

## Why Slack, why now

A team's bots already live in Slack. Adding a second deployment
target means each bot grows a new auth surface, or we reinvent
authorization / audit / history / parallel sessions ourselves.
Treating Slack as the API gets all of that for free, and the bot
behaves identically whether the user is in moon-ide, on their phone,
or at lunch. The trade-off: no push events — see
[Real-time](#real-time).

## Auth model

The user pastes a Slack **user OAuth token** (`xoxp-…`), not a bot
token:

- Posting must look like the user, not a second "moon-ide" bot in
  the thread.
- Reading the user's DMs requires user-side scopes; a bot token
  sees the _bot's_ conversation tree, not this user's.
- Skipping a moon-ide-shipped Slack app means no OAuth callback to
  host, no client secret in the binary, no "rotate the secret"
  support burden. Revisit when somebody asks for one-click install.

### Required scopes (user token)

We ask for the **full Phase 11 scope set upfront** — every later
scope add forces a reinstall in Slack's UI, so one slightly broader
prompt beats repeated friction:

| Scope             | Used by | What it gets us                                                   |
| ----------------- | ------- | ----------------------------------------------------------------- |
| `chat:write`      | 11.3    | Post messages as the user                                         |
| `im:history`      | 11.1    | Read DM history (= the bot's replies)                             |
| `im:read`         | 11.0    | List the user's DM channels (find the bot's DM)                   |
| `im:write`        | 11.2    | `conversations.mark` — clear the unread badge                     |
| `users:read`      | 11.0    | Resolve the bot's user ID, display name, avatar                   |
| `team:read`       | 11.0    | Workspace icon + name on the workspace card                       |
| `reactions:read`  | 11.4    | See Moonbot's status emoji                                        |
| `reactions:write` | 11.4+   | React to messages from the IDE                                    |
| `files:read`      | 11.4+   | Render image / file attachments the bot sends                     |
| `files:write`     | 11.4+   | Upload images / files (e.g. screenshots so the bot can read them) |

Scopes deliberately left out (bookmarks, pins, reminders, search,
channels/groups, custom emoji, DND, …) are non-commitments; any of
them is a one-time scope add + reinstall when a concrete request
comes in. Anything new needs a row in the table above before it gets
added.

### Setup walk-through (in-IDE)

The "Connect Slack" modal walks the user through creating a personal
Slack app, adding the user-token scopes, installing, and pasting the
`xoxp-` token. We validate via `auth.test` (plus `team.info` for the
workspace card) before persisting, then load the bot picker — see
[Bot resolution](#bot-resolution).

### Token storage

Tokens live in the **OS keyring** (service `moon-ide`, account
`slack-user-token`) — never in `app_state.json` or any session blob.

> **Backend features matter.** keyring 3.x ships with no platform
> backend unless you opt in via Cargo features — without them the
> mock backend silently no-ops and tokens "save" but never persist.
> We pin `apple-native + windows-native + sync-secret-service +
crypto-rust`. On Linux the user needs a running Secret Service
> daemon; if tokens don't persist, check that first.

The entry clears on explicit Disconnect and on `auth.test` returning
`not_authed` / `invalid_auth`. `AppState` stores only derived,
non-secret pointers (bot ID, DM channel ID, active thread ts).

## Bot resolution

### What doesn't work

"User types `@Moon Bot`, we look it up" fails on real workspaces:
there is no public user-search endpoint for `xoxp-` tokens,
`users.list` doesn't scale (HF's workspace has 5 000+ users and
pagination hits tier-2 rate limits — a first connect would take
30–60 s), and `users.lookupByEmail` is useless for bots without
public emails.

### What we do instead: pick from your 50 most recent DMs

The user's own DM list is orders of magnitude smaller and naturally
scoped to bots they already use:

1. Pre-condition (stated in the connect modal): the user has DM'd
   the bot at least once and it sits in their 50 most recent DMs —
   a quick "hi" from regular Slack bumps a stale one.
2. `conversations.list?types=im&limit=50` (newest-first), then
   `users.info` per partner, keeping `is_bot && !deleted`.
3. The panel renders the picker; the picked
   `(user_id, dm_channel_id, display metadata)` persists in
   `AppState` so the picker doesn't reappear.

Works for any DM-able bot without code changes. The cap lives in one
constant (`DM_SCAN_LIMIT` in `moon-slack`); the connect-modal copy
hardcodes the number for clarity.

### Cost

One tier-2 call plus 50 tier-4 `users.info` calls — well inside rate
budgets, ~10–20 s end-to-end behind a spinner card.

**Why 50 and not 200?** We tried 200: bigger windows trade mediocre
wins for a slower first connect and a bigger picker. 50 covers
anyone actively using the bot, and the recovery story for older DMs
is one gesture. Revisit on a concrete report, not a hunch.

### Persistence

`AppState.slack` carries `active_bot` and `active_thread_ts` — both
non-secret. The right-panel open state lives at
`AppState.right_panel` (`'chat' | 'coder' | null`), shared with the
coder panel.

#### Multi-writer story

Three writers touch `AppState`: the frontend session-persist path
(`last_session`, `theme`, `bottom_panel`), the Slack commands (the
`slack` slice), and `ui_set_right_panel` (`right_panel`).
`app_state_save` merges — it takes its own fields from the payload
and **preserves `slack` and `right_panel` verbatim from disk** — so
a session-persist coalesce can't clobber a bot pick or panel toggle
that just landed.

### Why not also paginate `users.list` for bots the user hasn't DMd?

The "you've DM'd the bot once" constraint is fine — the gesture is
one click in Slack, and it gives a resolution path independent of
workspace size. A `users.list` fallback gets added when a concrete
request arrives.

## Data model

Moonbot uses one thread per agent session, so:

| moon-ide concept | Slack concept                                 |
| ---------------- | --------------------------------------------- |
| **Bot profile**  | `(user_id, dm_channel_id)` + display metadata |
| **Session**      | A thread in the DM channel (`thread_ts`)      |
| **Message**      | A Slack message inside the thread             |
| **New session**  | Posting a top-level message (no `thread_ts`)  |

The session list is `conversations.history` filtered to top-level
messages, newest first; the active session is
`conversations.replies` for the selected thread.

## Mrkdwn rendering

### Block Kit precedence

Rich-layout bots put the real body in Block Kit blocks and a
flattened fallback in `text` (which commonly loses newlines). The
Rust client (`moon-slack::client::text_from_blocks`) extracts one
mrkdwn string before the frontend ever sees the message: `section`
mrkdwn text is used, `markdown` blocks forward as-is, `divider`
becomes a rule, link buttons in `actions` become `SlackAction`s
(rendered as pill buttons, opened externally; interactive
value-only elements are dropped — a read-only panel can't dispatch
them), everything else is skipped. If no block contributed text,
the raw `text` field is the fallback — the common path for human
DMs. The session-list preview uses the same precedence.

### Frontend tokenizer

Slack's `text` is **mrkdwn**, not CommonMark — single-marker
`*bold*` / `_italic_` / `~strike~` and angle-bracket tokens for
links / mentions / channels / broadcasts / dates — so `markdown-it`
would mis-render every other token. We hand-roll a pure parser in
`src/lib/util/slackMrkdwn.ts` (block layer: fences + quotes; inline
layer: structured `<…>` tokens, inline code, then recursive-descent
formatting with Slack's word-boundary rules).

`<@U…>` mentions resolve through a reactive per-process user cache
with split read (`peekUser`, safe in render) and fetch
(`requestUser`, effect/event-time only) so Svelte's render path
stays pure. The connected user and active bot are seeded eagerly;
the cache flushes on disconnect. Session-list previews flatten the
same tree to plain text (with the same mention resolution, and
truncation-safe handling of a `<…` cut mid-token).

Emoji shortcodes resolve via `node-emoji` plus a small Slack-only
alias table (Slack kept gemoji-era names CLDR renamed), skipping
code nodes so literal `:foo:` in pasted source survives. Unknown
names pass through as text.

Out of scope: custom workspace emoji (needs `emoji:read` + an
`<img>` resolver), channel-name resolution beyond Slack's embedded
`|name`, attachments/files/images (11.4+).

### Known limitations of the deferred markdown-block path

`markdown` blocks (CommonMark, used by moon-bot for >3000-char
messages) hit the mrkdwn tokenizer unchanged: `**bold**` shows
literal asterisks, `[label](url)` isn't parsed, fenced-block
language tags leak as a first line. The fix is a server-side
`markdown_to_mrkdwn` — deferred until a real long-message report
comes in (the behaviour contract is pinned by
`forwards_markdown_block_text_unchanged` in `moon-slack`).

### Link safety

Only `http://` / `https://` / `mailto:` schemes become link nodes;
everything else renders as literal text. The click handler
re-validates the scheme via `URL` parsing, and the
`opener:default` capability allows the same set — `javascript:` is
impossible at every layer.

### HTML entity decoding

Slack escapes only `<`, `>`, `&`; we also accept numeric entities.
Decoding happens at text emission only — never inside a structured
token's body — so a URL containing `&amp;` round-trips safely.

## Real-time

Slack's push paths don't work for a user-token desktop app: the
Events API needs a public HTTPS endpoint, Socket Mode rejects
`xoxp-` tokens, and RTM is deprecated and closed to new apps. So we
**poll** — ~12 req/min while hot, an order of magnitude under the
tier-3 limits even with several threads watched.

### What we poll

Only the currently selected thread in the currently visible panel —
one `conversations.replies(…, oldest=last_seen_ts)` per tick; edits
surface via `edited.ts` on the same call. We don't poll the channel
for new top-level messages: the IDE itself initiates new sessions,
and sessions started from another Slack client get discovered on
the next panel open. Not worth the budget for an edge case.

### Cadence ladder

| Time since last activity | Poll every                           |
| ------------------------ | ------------------------------------ |
| < 30 s                   | 3 s                                  |
| 30 s – 2 min             | 5 s                                  |
| 2 min – 10 min           | 15 s                                 |
| 10 min – 1 h             | 60 s                                 |
| > 1 h                    | paused — refresh on user interaction |

The clock resets on any new message or edit (cold → hot the moment
the bot replies). The loop pauses when the panel is hidden, the OS
window unfocuses, or no session is selected. Resuming a cold thread
runs one immediate catch-up poll.

### Read receipts

`conversations.mark` runs on panel open, on session switch, and
after any poll tick that brought new messages **while the window is
focused** — an unfocused-but-visible panel doesn't mark, because
the user hasn't actually seen the message.

**What `conversations.mark` actually clears**: only the
channel-level cursor (sidebar unread counts). The per-thread unread
that drives Slack's Activity feed uses an internal endpoint
(`subscriptions.thread.mark`) that requires a browser-session
`xoxc` cookie and rejects `xoxp-` tokens. Consequence: the Activity
badge for a bot reply stays red until the user opens the thread in
real Slack. We accept the gap — capturing a session cookie is too
invasive, and Slack actively breaks third-party `xoxc` use. If the
public API ever grows thread-mark, wire it into `mark_as_read`.

## Sending messages (Phase 11.3)

A single fixed-bottom auto-growing textarea. **Enter** sends (the
team's preference for short conversational messages),
**Shift+Enter** newline, **Ctrl/Cmd+Enter** also sends (Slack
muscle memory), Esc cancels the new-session composer. The composer
disables while the post is in flight; a failed post keeps the draft
with an inline error.

Two posting modes:

- **Reply** — posts with `thread_ts`, appends the returned message
  optimistically; the next poll tick sees the same fingerprint and
  no-ops, so no flicker.
- **New session** — posts top-level, then pivots the panel into the
  new thread and refreshes the session list; the poller picks up
  the new thread immediately.

No "sending…" pip — the round-trip is ~200 ms and an extra UI state
costs more than it pays. User input ships as-is:
`chat.postMessage` accepts mrkdwn directly, so what they type is
what the bot sees.

## UI placement

A right-side panel docked to the editor area, sharing the
right-panel slot with the coder. Toggleable from the status bar,
the command palette, and the `F6` focus rotation. Top-to-bottom:
session picker → active thread (bot bubbles tinted) → composer.
The active thread's `thread_ts` round-trips through `AppState` so a
relaunch lands back in the same conversation.

The panel keeps two generation counters (sessions list, active
thread) so a stale response can't repaint after the user moved on;
switching bots clears both.

## Frontend ↔ backend boundary

Tauri commands in `src-tauri/src/commands/slack.rs`:

| Command                                                         | Purpose                                              |
| --------------------------------------------------------------- | ---------------------------------------------------- |
| `slack_set_token(token)`                                        | Validate, persist to keyring, return `SlackIdentity` |
| `slack_status()`                                                | `connected: bool`, identity if connected             |
| `slack_clear_token()`                                           | Drop keyring entry                                   |
| `slack_list_dm_bots()`                                          | Scan user's DMs, return the bot users                |
| `slack_select_bot` / `slack_clear_bot` / `slack_get_active_bot` | Bot-pick persistence                                 |
| `slack_set_window_focused(focused)`                             | OS focus signal for the read-receipt gate            |
| `slack_list_sessions(channel)`                                  | Top-level DM messages                                |
| `slack_get_thread(channel, ts)`                                 | One thread's replies                                 |
| `slack_set_active_thread(thread_ts \| null)`                    | Persist the open thread                              |
| `slack_get_user(user_id)`                                       | Resolve a `<@U…>` mention                            |
| `slack_post_message(channel, thread_ts?, text)`                 | Post; returns the new `SlackMessage`                 |
| `slack_mark_read(channel, ts)`                                  | `conversations.mark`                                 |

Push events: `slack:thread-update` (full thread snapshot,
reconciled only when it matches the open session) and
`slack:disconnected` (keyring already cleared Rust-side).

## Failure modes

| Scenario                            | UI behaviour                                                     |
| ----------------------------------- | ---------------------------------------------------------------- |
| No token configured                 | "Connect Slack" empty state                                      |
| Token rejected by `auth.test`       | Inline error in connect modal; token not saved                   |
| Token revoked mid-session           | Toast + return to empty state; keyring cleared                   |
| No bots in the user's DMs           | Picker shows "DM your bot from Slack first, then click Refresh." |
| Network down                        | Polling backs off (5s → 15s → 30s); small "offline" pip          |
| `chat.postMessage` rate-limited     | Surface Slack's `Retry-After`; queue locally; toast on retry     |
| Bot's reply spans multiple messages | Render in order; no special grouping                             |

## What this phase deliberately doesn't do

- **No agent context bridge** — that's the [coder](coder.md).
- **No moon-ide Slack app** — personal app + pasted token until
  somebody asks for one-click install.
- **No file/image attachments** — image-only messages degrade to an
  empty preview today; the scopes are already granted for when
  moon-bot starts attaching things users want to see.
- **No last-read marker line** — opening a thread snaps to the
  bottom. Cheap to add later (persist `last_seen_ts`, render a
  divider).
- **No AI-generated session titles** — raw first line for now.
- **No multi-account** — one workspace per install.
- **No reaction toggling** — display only (chips with emoji +
  count); writing needs an emoji picker and nobody's asked.
  Skin-tone modifiers render as the base emoji.
- **No custom workspace emoji** — raw `:shortcode:` text until the
  feature warrants `emoji.list` + an `<img>` per chip.
