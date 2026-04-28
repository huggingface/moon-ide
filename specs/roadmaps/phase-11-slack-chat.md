# Phase 11 — Slack chat panel

A right-side panel that DMs a Slack bot (defaults to Hugging
Face's [Moonbot](https://github.com/huggingface/moon-bot),
pluggable to any DM-able bot — Cursor, GitHub, etc.). One Slack
thread = one bot session; each top-level DM message starts a
new session, replies stay inside the thread. We don't pretend
to host the agent — we're a chat client over the Slack Web API.
The bot has zero visibility into local IDE context; this is
pure pass-through.

User-facing setup is a one-time `xoxp-` user OAuth token paste
with an in-IDE walk-through (the Slack app the user installs is
theirs, not ours — we don't ship a moon-ide Slack app yet). The
token lives in the OS keyring (libsecret / Keychain / Credential
Manager). Real-time updates run on `conversations.history`
polling (~5 s, gated on panel-visible + active-thread) since
Slack's push paths (Events API, Socket Mode, RTM) aren't
workable for a desktop user-token client.

Architectural spec: [`slack-chat.md`](../slack-chat.md).
Deliberately-deferred features:
[`slack-chat.md` § "What this phase deliberately doesn't do"](../slack-chat.md#what-this-phase-deliberately-doesnt-do).

## Sub-phases

### 11.0 — Foundation

`moon-slack` crate (Web API client: `auth.test`,
`conversations.list?types=im`, `users.info`). Token storage in
the OS keyring (`keyring` crate, `apple-native +
windows-native + sync-secret-service + crypto-rust`). Tauri
commands `slack_set_token` / `slack_status` /
`slack_clear_token` / `slack_list_dm_bots` / `slack_select_bot`
/ `slack_clear_bot` / `slack_get_active_bot` /
`slack_set_panel_visible`. Right-side panel scaffolding with a
"Connect Slack" walkthrough listing all upfront-granted scopes,
validation via `auth.test`, and a "DM-first" bot picker that
scans the user's 50 most recent DMs (`DM_SCAN_LIMIT`). End
state: the panel says "Connected as Eli — Moon Bot" and
persists token + bot pick + panel visibility across restarts.
Test plan:
[`0008-slack-foundation.md`](../test-plans/0008-slack-foundation.md).

### 11.1 — Read-only chat

Render the DM session list (top-level messages, newest first)
and the active thread (read-only message bubbles). Bot tile
uses the avatar resolved during 11.0's DM scan; user/bot
bubbles distinguished by `bot_id`. New tauri commands
`slack_list_sessions` / `slack_get_thread` /
`slack_set_active_thread` and a new
`SlackAppState.active_thread_ts` so the open thread + panel
visibility both round-trip across restarts. No polling, no
edits, no sending — those land in 11.2 / 11.3. Test plan:
[`0009-slack-read-only-chat.md`](../test-plans/0009-slack-read-only-chat.md).

### 11.1.1 — Slack mrkdwn rendering

Hand-rolled tokenizer + Svelte renderer for Slack's mrkdwn
dialect (links `<URL|label>`, mentions `<@U…>`, channel refs
`<#C…|name>`, broadcasts `<!here>`, dates `<!date^…>`,
bold/italic/strike, inline + fenced code, block quotes).
Mention names resolve through a per-process `users.info` cache
(new `slack_get_user` command). Session-list previews flatten
the same tree to plain text. Brought forward from 11.4 because
raw `<@U…>` tokens were unreadable.

### 11.2 — Polling + read receipts

Background tokio loop in `src-tauri/src/slack_poller.rs` driven
by panel-visible + active-thread + OS focus, with a per-thread
cadence ladder (3 s hot → 5 s warm → 15 s → 60 s → paused
cold — see
[`slack-chat.md`'s cadence ladder](../slack-chat.md#cadence-ladder)).
Detects new messages and `edited.ts` edits; pushes the full
thread snapshot to the frontend via the `slack:thread-update`
Tauri event, plus `slack:disconnected` on auth failure.
`conversations.mark` fires on view, on session switch, and on
poll-tick-while-focused so unread badges clear in the user's
actual Slack client. Test plan:
[`0011-slack-polling.md`](../test-plans/0011-slack-polling.md).

### 11.3 — Send messages

`chat.postMessage` wired to a textarea-based composer at the
bottom of the panel, sent on Enter (Shift+Enter for newline;
Ctrl/Cmd+Enter also sends). "+ New session" toggles a
fresh-conversation mode: posting creates a top-level message in
the bot's DM and pivots the panel into the resulting thread;
otherwise the post becomes a reply with the open thread's `ts`.
Optimistic append for replies (the next poll tick re-syncs from
Slack's view), full state reset + sessions reload for the
new-session pivot. Test plan:
[`0012-slack-send.md`](../test-plans/0012-slack-send.md).

### 11.3.1 — Reaction display

Render the `reactions` array on each message as a row of small
`<emoji> <count>` chips below the message body and above any
action buttons. Slack shortcodes go through the existing
`slackEmoji.resolveReactionName` helper (which strips
`::skin-tone-N` modifiers and falls back to `:name:` for
custom workspace emoji). Read-only — tapping a chip doesn't
toggle the user's own reaction yet; that needs an emoji picker
and lives behind the next concrete request.

### 11.4 — Multi-bot + polish

Configurable bot profiles (Moonbot is the default; user adds
Cursor / GitHub / any DM-able bot by handle). Tab strip inside
the panel when there's more than one.

## Out of scope

The canonical deferred-features list (with per-feature design
notes) lives in
[`slack-chat.md` § "What this phase deliberately doesn't do"](../slack-chat.md#what-this-phase-deliberately-doesnt-do).
At a glance: file/image attachments, auto-scroll to the latest
message, AI-generated session titles, OAuth flow that ships a
moon-ide Slack app, local IDE context for the bot (that one
belongs to ACP — Phase 6).
