# Slack chat panel

The right-side chat panel is a thin Slack Web API client that lets a
moon-ide user DM a bot (Hugging Face's [Moonbot](https://github.com/huggingface/moon-bot)
by default, any DM-able bot in v1.4+) without leaving the IDE. We're a
chat client, not an agent host: the bot already runs somewhere (Pi
agent on a Hugging Face cluster, in Moonbot's case) and Slack is the
transport. The IDE's job is rendering the conversation cleanly,
posting messages, and keeping the bot's edits live.

The bot has **zero visibility into local IDE state**. No file context,
no LSP, no skill installation — anything the user wants the bot to
see they paste into the message themselves. Real "agent in the IDE
with context" is Phase 6 (ACP) and stays separate.

## Why Slack, why now

A team's bots already live in Slack. Adding a second deployment
target ("now also speak HTTP to moon-ide") means each bot grows a new
auth surface and we end up either reinventing the bot wheel
(authorization, audit, history retention, parallel sessions across
team members) or writing a moon-ide-specific bot. Both are large
projects. By treating Slack as the API, we get all of that for free —
including the property that the bot's behaviour is exactly the same
whether the user is in moon-ide, on their phone, or at lunch.

The trade-off: we don't get push events. See "Real-time" below.

## Auth model

The user pastes a Slack **user OAuth token** (`xoxp-…`). Not a bot
token (`xoxb-…`); not the Moonbot token. The token represents the
human, scoped to _their own DMs_ with the bots they want to talk to.

Why a user token instead of a moon-ide-specific Slack app:

- Posting must look like the user, not a second bot. Bot-token posts
  via `chat.postMessage` would show up in Slack as "moon-ide" in the
  thread, which is wrong — the user is the participant.
- Reading the user's DMs requires user-side scopes (`im:history`).
  The bot's token can read its own DMs, but only via the bot's
  identity, which is a different conversation tree (the bot sees
  _its_ DMs with everyone; the IDE wants _this user's_ DM with the
  bot).
- Skipping the moon-ide-shipped Slack app means we don't host an
  OAuth callback, don't embed a client ID/secret in the binary, and
  don't take on the support burden of "your Slack app is unavailable
  — please rotate the secret". When somebody asks for one-click
  install, we revisit.

### Required scopes (user token)

We ask for the **full Phase 11 scope set upfront**, not the
per-sub-phase minimum. Every scope add in Slack means revisiting
_OAuth & Permissions → Reinstall_, which is friction the user
absorbs every time we ship a sub-phase. Granting upfront trades a
slightly broader initial prompt for zero re-installs through 11.4.

| Scope             | Used by | What it gets us                                                   |
| ----------------- | ------- | ----------------------------------------------------------------- |
| `chat:write`      | 11.3    | Post messages as the user                                         |
| `im:history`      | 11.1    | Read DM history (= the bot's replies)                             |
| `im:read`         | 11.0    | List the user's DM channels (find the bot's DM)                   |
| `im:write`        | 11.2    | `conversations.mark` — clear the unread badge                     |
| `users:read`      | 11.0    | Resolve the bot's user ID, display name, avatar                   |
| `reactions:read`  | 11.4    | See Moonbot's status emoji (✅ / ⚠️ / ❌)                         |
| `reactions:write` | 11.4+   | React to messages from the IDE (👍 / 👎 / …)                      |
| `files:read`      | 11.4+   | Render image / file attachments the bot sends                     |
| `files:write`     | 11.4+   | Upload images / files (e.g. screenshots so the bot can read them) |

The walk-through in `ChatConnectModal.svelte` lists these and flags
which are exercised today vs. claimed upfront.

#### Scopes deliberately left out (for now)

These are not in the upfront grant — adding any of them later is a
one-time scope add + reinstall. Listed here so we don't have to
rediscover the option space when a request comes in. **Nothing
below is committed**; each entry is "would unlock", not "we'll do".

| Scope                                                   | Would unlock                                                                                                            |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `bookmarks:read` / `bookmarks:write`                    | Slack bookmarks as an IDE todo list (Eli's personal workflow). Render them in the chat panel sidebar / a dedicated tab. |
| `pins:read` / `pins:write`                              | Pin a Moonbot reply to keep it visible (e.g. "current branch plan"). Pins are per-channel including DMs.                |
| `reminders:read` / `reminders:write`                    | Surface Slack reminders in the status bar / panel; create a reminder from a chat message.                               |
| `search:read`                                           | Search across the whole DM history (and beyond) from the IDE — Slack server-side, not local.                            |
| `stars:read` / `stars:write` (legacy "saved items")     | Probably skip; bookmarks supersede this for our use case.                                                               |
| `mpim:read` / `mpim:history` / `mpim:write`             | Group DMs — relevant if we ever want a "shared bot session" across teammates.                                           |
| `channels:read` / `channels:history` / `channels:write` | Public channels. Heavy surface; would only make sense if the panel grows beyond DMs.                                    |
| `groups:read` / `groups:history` / `groups:write`       | Private channels. Same caveat as `channels:*`.                                                                          |
| `users:read.email`                                      | Resolve bots/users by email instead of display name (more stable IDs across renames).                                   |
| `usergroups:read`                                       | Render `@team` mentions correctly.                                                                                      |
| `emoji:read`                                            | Render custom workspace emoji in messages and reactions instead of falling back to `:shortcode:`.                       |
| `team:read`                                             | Workspace metadata (icon, domain) for nicer panel chrome.                                                               |
| `dnd:read` / `dnd:write`                                | Mute the unread pip while the user is in DND; let the IDE pause Moonbot's reactions during focus blocks.                |
| `links:read` / `links:write`                            | Custom link unfurls — probably never needed for our flow.                                                               |

Anything not in either table needs a one-line entry here before it
gets added — same rule as the rest of `specs/`.

### Setup walk-through (in-IDE)

The "Connect Slack" affordance opens an in-app modal with these
steps, each with a one-click "Open in browser" link:

1. Create a Slack app at <https://api.slack.com/apps> ("From scratch").
2. **OAuth & Permissions → User Token Scopes** → add the scopes
   listed above.
3. **Install App** → install to your workspace and authorize.
4. Copy the **User OAuth Token** (`xoxp-…`) — _not_ the Bot token.
5. Paste it into the field below and click "Connect".

We validate via `auth.test` before persisting. On success the panel
loads the bot picker — see [Bot resolution](#bot-resolution) — which
scans the user's DM list (not the workspace directory) and lets them
click the bot they want to chat with.

### Token storage

User tokens are credentials. They live in the **OS keyring** — never
in `app_state.json` and never in any session blob. The `keyring`
crate gives us libsecret on Linux, Keychain on macOS, and Credential
Manager on Windows. The keyring entry uses service `moon-ide` and
account `slack-user-token`.

> **Backend features matter.** keyring 3.x ships with **no platform
> backend** unless you opt in via Cargo features. Without those the
> mock backend silently no-ops on every save and every read, so
> tokens "succeed" but never persist. We pin
> `apple-native + windows-native + sync-secret-service + crypto-rust`
> in `workspace.dependencies` for that reason. On Linux the user
> needs a running Secret Service daemon (gnome-keyring, KWallet's
> compatibility shim, KeePassXC, …). All mainstream desktops ship
> one by default — but if a user reports tokens not persisting,
> check that first.

Clearing happens on:

- Explicit "Disconnect" from the chat panel.
- `auth.test` returning `not_authed` / `invalid_auth` (token revoked
  by the user, app uninstalled, workspace SSO timeout). The panel
  drops back to the "Connect Slack" empty state.

`AppState` only stores derived, non-secret pointers: the resolved
bot user ID, DM channel ID, last active thread ts, panel-visible
flag.

## Bot resolution

### What doesn't work

The intuitive design — "user types `@Moon Bot`, we look it up" —
falls apart on real workspaces:

- **No public user-search endpoint.** `users.search`, `search.users`,
  `search.modules.users` all return `unknown_method` for `xoxp-`
  tokens. Slack's web Ctrl+K is instant only because the web client
  preloads the entire user directory and keeps it warm via Socket
  Mode events; we have neither.
- **`users.list` doesn't scale.** Hugging Face's workspace has more
  than 5 000 users and pagination starts hitting tier-2 rate limits
  after ~3 000 entries. Even with backoff and full pagination, a
  first-time connect would take 30–60 s and burn the rate-limit
  budget for the rest of the session.
- **`users.lookupByEmail`** exists but bots don't have public emails,
  and asking the user for a bot's email is a UX dead-end.

### What we do instead: pick from your 50 most recent DMs

The user's **own DM list** is several orders of magnitude smaller
than the workspace directory and naturally scoped to bots they
already use. The connect flow becomes:

1. Pre-condition (surfaced in the connect modal): the user has DMd
   the bot at least once from regular Slack, and the conversation
   sits in their **50 most recent DMs**. The "50 most recent" cap is
   stated upfront so users with stale bot DMs know to send a quick
   "hi" from regular Slack to bump it before connecting.
2. After token validation, we call `conversations.list?types=im&limit=50`
   to get the 50 most recently active DMs (Slack returns them
   newest-first).
3. For each DM partner, we call `users.info` and keep only those
   with `is_bot: true && !deleted`.
4. The chat panel renders these as a picker. The user clicks the
   bot they want; we store the resolved `(user_id, dm_channel_id,
real_name, display_name, image_url)` tuple in `AppState` so the
   picker doesn't reappear on next launch.

This works for any DM-able bot — Moonbot, Cursor, GitHub, Linear,
custom team bots — without code changes. The original "default to
@Moon Bot for HF" is gone; the team's bot landscape is what it is.

The single source of truth for the cap is the
[`DM_SCAN_LIMIT`](../crates/moon-slack/src/client.rs) constant in
`moon-slack`. The number is hardcoded in the connect-modal copy and
the picker UI for clarity; if the constant ever moves, those two
strings update too.

### Cost

One `conversations.list?types=im&limit=50` call (tier-2,
20+ /minute — fine for an interactive flow) plus 50 sequential
`users.info` calls (tier-4, 100+ /minute — well inside budget).
End-to-end the discovery finishes in ~10–20 s on a warm network.
The picker shows a single spinner card while it runs; streaming
bots into the UI as each `users.info` returns is a 11.1 polish
item.

#### Why 50 and not 200 or 1 000?

We tried 200 first. The honest answer is: bigger windows trade
mediocre wins ("we'd find a bot the user hasn't DM'd in a year")
for a much worse first-connect experience (slow scan, ambient API
spend, and a bigger picker list to scroll). 50 covers anyone
actively using the bot, the cap is small enough to disclose
upfront without it sounding scary, and the recovery story for
older bot DMs ("send a hi from Slack first") is one gesture. If
this turns out to be too tight, we revisit — but only with a
concrete report, not on a hunch.

### Persistence

`AppState.slack` carries everything needed to put the chat panel
back where the user left it:

- `active_bot`: the picked bot's ID + DM channel ID + display
  metadata. On launch, if it's set, the panel shows that bot
  directly and skips the picker. Switching bots ("Pick a different
  bot" affordance) clears the field and re-runs discovery.
- `panel_visible`: whether the right-side chat panel was open at
  shutdown. Restored verbatim. Defaults to `false` so first-run
  users don't get a chat panel they haven't asked for.

Both fields are non-secret. The token itself stays in the keyring;
nothing about it (or its hash, or its prefix) ends up in
`app_state.json`.

#### Multi-writer story

Two paths write to `AppState`:

- The frontend session-persist path (`app_state_save`) owns
  `last_session` and `theme`.
- The Slack tauri commands (`slack_select_bot`, `slack_clear_bot`,
  `slack_set_panel_visible`, `slack_set_active_thread`, plus the
  auth-failure cleanup in `slack_status` / `slack_clear_token`) own
  the `slack` slice (`active_bot`, `panel_visible`,
  `active_thread_ts`).

`app_state_save` merges: it loads the on-disk state, takes
`last_session` + `theme` from the payload, and **preserves the
on-disk `slack` slice verbatim**. The frontend still has to send a
placeholder `slack` field to satisfy the shared TS type, but the
backend ignores it. This stops a session-persist coalesce from
clobbering a bot pick that just landed.

### Why not also paginate `users.list` for bots the user hasn't DMd?

Because the constraint "you've DMd the bot at least once from
regular Slack" is fine — every bot we want to support has a UI in
Slack for starting a DM, the gesture is one click, and it gives us a
universal resolution path that doesn't depend on workspace size. If
this turns out to be wrong (someone wants to chat with a bot from
the IDE without ever opening regular Slack), we add a
`users.list`-scan fallback later — but only when that request comes
in, with concrete UX requirements (small workspace? cache the result?).

## Data model

In Slack a **DM channel** has a flat list of top-level messages, each
of which can have a **thread**. Moonbot uses one thread per agent
session: it replies in-thread to the user's first message and keeps
all subsequent context inside that thread. So:

| moon-ide concept | Slack concept                                                          |
| ---------------- | ---------------------------------------------------------------------- |
| **Bot profile**  | `(user_id, dm_channel_id)` + display metadata, picked from the DM list |
| **Session**      | A thread in the DM channel (`thread_ts`)                               |
| **Message**      | A Slack message inside the thread                                      |
| **New session**  | Posting a top-level message (no `thread_ts`)                           |

The "session list" in the panel is `conversations.history` of the
DM channel, filtered to messages where `ts === thread_ts || !thread_ts`
(top-level only), newest first. Each row shows the first ~80 chars
of the user's prompt and the time of the latest reply.

The "active session" is `conversations.replies(channel, ts)` for the
selected thread.

## Mrkdwn rendering

### Block Kit precedence

Bots that use rich layouts (moon-bot, Cursor, GitHub Slack app, …)
put the _real_ message body in [Block Kit blocks][bk] and a flattened
notification fallback in the `text` field. Slack's UI renders the
blocks; the `text` field commonly loses newlines and structure.

The Rust client (`moon-slack::client::text_from_blocks`) extracts a
single Slack-mrkdwn string from the supported block types before
returning a `SlackMessage` to the frontend, so the rest of the
pipeline only ever sees one representation:

| Block type                                                  | Behaviour                                                                                                                                                                                                                          |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `section`                                                   | Use `text.text` when `text.type == "mrkdwn"`. `plain_text` is skipped.                                                                                                                                                             |
| `markdown`                                                  | Forwarded as-is (CommonMark). `**bold**` / fenced language tags will leak.                                                                                                                                                         |
| `divider`                                                   | Emits `———`.                                                                                                                                                                                                                       |
| `actions`                                                   | Link buttons (`button` elements with a `url`) become `SlackAction`s on the message. Interactive `value`-only buttons and other element types (datepicker, select, …) are dropped — a read-only panel can't dispatch them anywhere. |
| Anything else (image, header, context, rich_text, table, …) | Skipped silently.                                                                                                                                                                                                                  |

Blocks are joined with a blank line. If no block contributed any text
(e.g. only `rich_text` from a human typer), the raw `text` field is
used as fallback — this is the common path for human DMs.

Same precedence applies to the session-list preview (`to_session`),
so a bot's session row also reads its blocks instead of the flattened
fallback.

`actions` blocks contribute to a separate `actions: Vec<SlackAction>`
field on `SlackMessage` rather than the body text. The frontend
renders them as a row of pill buttons under the message body, opened
externally via `tauri-plugin-opener` (same `http(s)` / `mailto`
allowlist as inline links). Slack's `style: "primary" | "danger"`
hint tints the button.

[bk]: https://api.slack.com/reference/block-kit/blocks

### Frontend tokenizer

Slack's `text` field is **mrkdwn**, not CommonMark. The differences
matter enough that `markdown-it` would mis-render every other token:
single asterisks for bold (`*bold*`), single underscores for italic
(`_italic_`), single tildes for strike (`~strike~`), and every link
/ mention / channel / broadcast / date as an angle-bracket token
(`<@U123>`, `<https://…|label>`, `<#C123|name>`, `<!here>`,
`<!date^…>`).

We hand-roll a parser in `src/lib/util/slackMrkdwn.ts`. Pure function;
returns a `BlockNode[]` tree that `SlackMessageBody.svelte` walks.
Two-layer design:

- **Block layer:** split on triple-backtick fences (code blocks
  are top-priority and short-circuit further parsing); group
  remaining lines into text vs. `>`-prefixed quotes.
- **Inline layer:** scan for `<…>` structured tokens (recognising
  user/channel/broadcast/usergroup/date/link shapes) and inline
  `` `code` ``; everything else feeds a recursive-descent
  formatter that handles `*`/`_`/`~` with Slack's
  word-boundary rules. Same marker can't nest inside itself
  (Slack's parser agrees); different markers nest freely.

`<@U…>` mentions resolve through a per-process reactive cache
(`SlackPanelState.userCache`). Reads and writes are split to keep
Svelte's render path pure (mutating `userCache` from a snippet trips
`state_unsafe_mutation`):

- `slack.peekUser(user_id)` — synchronous read, no side effects.
  Safe to call from `$derived` or template expressions.
- `slack.requestUser(user_id)` — idempotent fetch trigger; mutates
  the cache to `loading` and fires `slack_get_user`. Must be called
  from outside render (a Svelte `$effect`, an event handler, or
  after a network response).

The mrkdwn renderer collects user IDs via `collectMentionedUserIds`
in a `$derived`, kicks off `requestUser` for each from an `$effect`,
and reads results back through `peekUser` in the template. Once
`users.info` resolves the cache write triggers a re-render and labels
swap in. The connected user (`auth.test`) and active bot are seeded
into the cache eagerly on connect / bot select / hydrate, so they
never go through the network at all. The cache flushes on disconnect.

Session-list previews flatten the same tree to plain text via
`slackPlainText`. The preview accepts an optional `resolveUserId`
hook so cached `<@U…>` mentions render as `@username` instead of
`@U123`; misses fall back to the embedded `|label`, then the raw ID.
`ChatPanel.svelte` parses every visible session preview once, calls
`slack.requestUser` for each unique ID from an `$effect`, and reads
back through `peekUser` in `previewOf`.

`slackPlainText` also strips the trailing `<…` when Slack's preview
truncation cuts mid-token: an unclosed `<` would otherwise leak as
literal text into the row. The cut tail is replaced with an ellipsis.

Standard Unicode emoji shortcodes (`:tada:`, `:robot_face:`, …) are
resolved by `src/lib/util/slackEmoji.ts` in a final tree-walk pass
on the parser output. The resolver layers a small Slack-only alias
table on top of `node-emoji` (Slack still uses the gemoji-era
`_face` / `tools` / `thinking_face` names that CLDR has since
renamed), and skips `code` / `codeblock` nodes so a literal `:foo:`
inside source the bot pasted survives intact. Mention / link /
channel _labels_ do go through the rewriter because Slack itself
substitutes shortcodes inside them. Unknown names (typos, custom
workspace emoji) pass through as the original `:shortcode:` text.

Out of scope: custom workspace emoji (would need `emoji:read` and
an `<img>` resolver — Phase 11.4+; we surface the raw `:shortcode:`
until then), channel name resolution (we trust Slack's `|name`
cache and fall back to `#C123` when absent), Block Kit attachments
/ files / images (Phase 11.4+).

### Known limitations of the deferred markdown-block path

`markdown` blocks (Slack's CommonMark variant, used by moon-bot for
messages > 3000 chars) are forwarded to the frontend tokenizer
unchanged. The tokenizer renders most of CommonMark correctly through
overlap (text, links, inline `` `code` ``, fenced code), but:

- `**bold**` / `__italic__` show literal asterisks / underscores
  (the tokenizer only knows single-marker forms).
- `[label](url)` is not parsed (only the Slack `<url|label>` shape is).
- A fenced code block with a language tag (` ```rust `) keeps the
  tag as the first line of the rendered code block.

Fix is a Rust `markdown_to_mrkdwn` that runs server-side before the
frontend ever sees the text — deferred until a real long-message
report comes in. See test `forwards_markdown_block_text_unchanged`
in `crates/moon-slack/src/client.rs` for the current behaviour
contract.

### Link safety

`<URL|label>` only emits a `link` node when the scheme is one of
`http://`, `https://`, `mailto:`. Anything else is rendered as
literal text. The renderer's click handler routes through
`URL` parsing as a second line of defence and only opens externally
when `protocol` is in the same allowlist (`opener:default` capability
permits the same set). `javascript:` is impossible at both layers.

### HTML entity decoding

Slack escapes only `<`, `>`, `&` (per their docs), but we accept
numeric `&#NN;` / `&#xHH;` for legacy messages. Decoding happens at
text emission only — never inside a structured token's body — so a
URL containing `&amp;` round-trips safely (`?a=1&amp;b=2` → real `&`
in the rendered href).

## Real-time

Slack's three push paths don't work for a user-token desktop app:

- **Events API** needs a public HTTPS endpoint. We're a desktop app.
- **Socket Mode** is bot-token + app-level-token only (`xoxb-` /
  `xapp-`). Won't accept `xoxp-`.
- **RTM** used to support user tokens but new apps can't enable it,
  it's been "deprecated" for years, and Slack signals it's going.

So we **poll**. The cost we're paying: ~12 requests/min while
actively waiting on a reply, dropping to ~1 / 5 min once the thread
goes quiet. Slack's tier-3 limit (`conversations.replies`,
`conversations.history`) is 50+ req/min, so we're an order of
magnitude under even with three threads being watched.

### What we poll

Only the **currently selected thread** in the **currently visible
panel**. One `conversations.replies(channel, thread_ts,
oldest=last_seen_ts)` call per tick. Edits to existing messages
surface via Slack's `edited.ts` field on the message — same call,
no separate "edits" endpoint.

We do _not_ poll the DM channel itself for new top-level messages.
Top-level messages only appear when the user starts a new session,
which the IDE itself initiates (Phase 11.3). If the user starts a
session from another Slack client (mobile, web), we'll discover it
the next time they open the IDE and the panel runs the one-shot
`conversations.history` that populates the session list. Tracking
that in real time would mean polling the channel as well — not
worth the budget for an edge case.

### Cadence ladder

Per-thread cadence based on time since the last new message or
edit on that thread:

| Time since last activity | Poll every                                                                                |
| ------------------------ | ----------------------------------------------------------------------------------------- |
| < 30 s ("hot")           | 3 s                                                                                       |
| 30 s – 2 min ("warm")    | 5 s                                                                                       |
| 2 min – 10 min           | 15 s                                                                                      |
| 10 min – 1 h             | 60 s                                                                                      |
| > 1 h ("cold")           | paused — refresh on user interaction (focus the panel, click the thread, switch sessions) |

The clock resets on any new message or edit, so a thread bumps
straight from "cold" back to "hot" the moment the bot replies.
The whole loop pauses when the chat panel is hidden, on `unfocus`
of the OS window, or when no session is selected.

When we resume a "cold" thread (user clicks back into it), we run
one immediate `conversations.replies` to catch up before re-entering
the cadence — that's the read-receipt trigger too (next section).

### Read receipts

`conversations.mark(channel, ts=last_visible_message_ts)` runs in
three spots so the unread badge in the user's actual Slack client
(mobile, web, desktop Slack app) clears as soon as they've seen the
message in moon-ide:

1. On opening the panel, for the active session.
2. On switching sessions.
3. After each poll tick where new messages arrived _and_ the panel
   is currently focused (= the user is actively reading).

If the panel is visible but the OS window is unfocused, we _don't_
mark — the user hasn't actually seen the message yet, and silently
clearing the badge in that case loses information.

Phase 11.0 doesn't ship this — it lands in 11.2 alongside the
polling loop. The `im:write` scope is already in the upfront grant
so the user doesn't reinstall.

## Sending messages (Phase 11.3)

The composer is a single fixed-bottom textarea — no separate
Send button. Enter is the send affordance and we'd rather not
double up on the click target. The textarea auto-grows from a
single-line resting height up to a ~120 px cap as the user
types, then scrolls internally.
**Enter** fires `slack_post_message` (the team's preference —
one-key send beats Slack's own Ctrl+Enter default for the
short, conversational messages this panel is built around).
**Shift+Enter** inserts a newline; **Ctrl/Cmd+Enter** also
sends, so users carrying muscle memory from Slack don't have to
relearn anything. Esc cancels the new-session composer and
returns to the session list. The composer disables itself while
`chat.postMessage` is in flight; on success the textarea clears,
on failure the draft sticks around with a small error line above
the input.

Two posting modes:

- **Reply** — there's an active thread (`activeThreadTs !== null`)
  and the composer is below the message list. The post carries
  `thread_ts = activeThreadTs`. The frontend appends the returned
  `SlackMessage` to `threadMessages` immediately (optimistic). The
  next poll tick sees the same `(ts, edited_ts)` fingerprint and
  no-ops, so there's no flicker.
- **New session** — `+ New session` toggles
  `composingNewSession = true`, hiding the session list and showing
  an empty composer. The post is top-level (`thread_ts = None`).
  On success, the panel pivots into the new thread:
  `activeThreadTs` becomes the returned `ts`, `threadMessages`
  becomes `[message]`, and `loadSessions()` is re-run so the new
  row shows up at the top of the session list. The poller's
  `set_active_thread_ts` is updated through the existing
  `slack_set_active_thread` path, so the cadence ladder kicks in
  immediately.

We deliberately do _not_ render the user's optimistic message
with a "sending..." pip — the API call is fast enough (~200 ms)
that the round-trip feels instant, and adding a separate "in
flight" UI state for that brief window costs more than it pays.
A failed post leaves the draft in the textarea with the error
above; the user retries with another Enter.

Slack's `chat.postMessage` accepts mrkdwn directly (the API doc
calls it `text`, but renders `*bold*`, `<https://x|y>`, code
fences, etc., the same way it renders incoming messages). We
don't translate the user's input — what they type is what the
bot sees, and what they see in the rendered bubble after
reconciliation.

## UI placement

A right-side panel docked to the editor area. Resizable horizontal
splitter (will use `paneforge` once Phase 1.5 adopts it; hand-rolled
splitter until then). Toggleable from:

- Status bar button (chat-bubble icon, active when connected).
- Command palette: "Chat: Toggle Panel".
- Keyboard: TBD in 11.1 — the focus-region cycle (`F6`) gains the
  chat panel as a region when it's visible.

The panel itself, top-to-bottom:

```
┌──────────────────────────────────────┐
│ Bot tabs (11.4): [Moonbot] [+]       │
├──────────────────────────────────────┤
│ Sessions ▾  | + New session          │  ← session picker (11.1)
│ "list new files…"  · 2 min           │
│ "weekly report…"   · yesterday       │
├──────────────────────────────────────┤
│                                      │
│  [user] you said                     │
│  …                                   │
│  [bot]  moonbot replied              │  ← active thread (11.1)
│  …                                   │
│                                      │
├──────────────────────────────────────┤
│ ┌──────────────────────────────────┐ │
│ │ Type a message — Enter to send   │ │  ← input (11.3)
│ └──────────────────────────────────┘ │
└──────────────────────────────────────┘
```

In 11.0 the panel renders the connect/disconnect state and the
resolved bot profile only. 11.1 adds the sessions list (top-level DM
messages with the active bot, newest-first; preview text capped to
80 chars server-side) and the active-thread view (read-only message
bubbles, bot bubbles get a tinted background). 11.1.1 then layers
Slack mrkdwn rendering on top — see [Mrkdwn rendering](#mrkdwn-rendering)
— so mentions/links/formatting render properly instead of as raw
`<@U…>` tokens. The active thread's `thread_ts` round-trips through
`AppState.slack.active_thread_ts` so a relaunch with the panel open
lands the user back inside the same conversation.

The panel uses two generation counters (one for the sessions list,
one for the active thread) so a stale `conversations.history` /
`conversations.replies` response can't repaint the panel when the
user has moved on (different bot, different thread). Switching bots
clears both — sessions live inside one bot's DM channel, so the new
bot has no business inheriting the previous one's open thread.

Sending and polling join in 11.2/11.3, but the data model already
supports them: `SlackMessage.edited_ts` is exposed so the future
poll loop can diff successive snapshots without re-comparing
message bodies.

## Frontend ↔ backend boundary

Tauri commands in `src-tauri/src/commands/slack.rs`:

| Command                                                | Purpose                                              |
| ------------------------------------------------------ | ---------------------------------------------------- |
| `slack_set_token(token)`                               | Validate, persist to keyring, return `SlackIdentity` |
| `slack_status()`                                       | `connected: bool`, identity if connected             |
| `slack_clear_token()`                                  | Drop keyring entry; return to disconnected state     |
| `slack_list_dm_bots()`                                 | Scan user's DMs, return the bot users among them     |
| `slack_select_bot(profile)`                            | Persist user's pick into `AppState.slack.active_bot` |
| `slack_clear_bot()`                                    | Drop the saved pick; trigger picker on next render   |
| `slack_get_active_bot()`                               | Read the persisted bot pick, if any                  |
| `slack_set_panel_visible(visible)`                     | Persist the chat panel's open/closed state           |
| `slack_set_window_focused(focused)` (11.2)             | OS focus signal for the read-receipt gate            |
| `slack_list_sessions(channel)`                         | `conversations.history` filtered to top-level        |
| `slack_get_thread(channel, ts)`                        | `conversations.replies` for one thread               |
| `slack_set_active_thread(thread_ts \| null)`           | Persist the open thread in `AppState.slack`          |
| `slack_get_user(user_id)`                              | `users.info` — resolve a `<@U…>` mention             |
| `slack_post_message(channel, thread_ts?, text)` (11.3) | Post a message; returns the new `SlackMessage`       |
| `slack_mark_read(channel, ts)` (11.2)                  | `conversations.mark`                                 |

Push events from backend → frontend (11.2):

- `slack:thread-update` — `{ channel, thread_ts, messages: [...] }`
  (full thread snapshot, frontend reconciles by replacing
  `threadMessages` iff `(channel, thread_ts)` matches the open
  session — stale pushes for previously-open threads are dropped).
- `slack:disconnected` — token went bad, panel returns to empty.
  The keyring + persisted bot pick are already cleared on the
  Rust side by the time this fires; the frontend just mirrors the
  in-memory disconnect.

## Failure modes

| Scenario                            | UI behaviour                                                                                |
| ----------------------------------- | ------------------------------------------------------------------------------------------- |
| User has no token configured        | "Connect Slack" empty state                                                                 |
| Token rejected by `auth.test`       | Inline error in connect modal; token not saved                                              |
| Token revoked mid-session           | Toast + panel returns to empty state; keyring entry cleared                                 |
| User has no bots in their DMs       | Picker shows "No bots found in your DMs. DM your bot from Slack first, then click Refresh." |
| Network down                        | Polling backs off (5s → 15s → 30s) and shows a small "offline" pip in the bot tab           |
| `chat.postMessage` rate-limited     | Surface Slack's `Retry-After`; queue locally; toast on retry                                |
| Bot's reply spans multiple messages | Render in order; no special grouping (Slack already chunks for us)                          |

## What this phase deliberately doesn't do

- **No agent context bridge.** The bot doesn't see open files, the
  diagnostic stream, the cursor, anything. Phase 6 (ACP) is where we
  build "agent that has the IDE under its hands". 11 stays chat-only.
- **No moon-ide Slack app.** Until somebody asks for one-click
  install, the user installs their own personal Slack app and pastes
  a user token. Documented in the connect walk-through.
- **No file/image attachments.** Slack supports both, we don't render
  them in v1. The conversation degrades to "(image)" placeholder for
  now.
- **No multi-account.** One Slack workspace per moon-ide install.
  Multi-workspace is a Phase 12 problem.
- **No reaction add/remove from the panel yet.** 11.3.1 ships
  _display_ (small chips with emoji + count below the message
  body). Tapping a chip to toggle the user's own reaction needs
  `reactions:write` (already in the upfront grant), an emoji
  picker, and a "you reacted" highlight; deferred until somebody
  asks. Skin-tone modifiers (`::skin-tone-N`) render as the base
  emoji because `node-emoji` doesn't speak the colourised
  variants — close enough.
- **No custom workspace emoji.** Slack lets workspaces upload
  their own emoji (`:moonbot-thumbsup:`); we render those as the
  raw `:shortcode:` text. Resolving them needs `emoji.list`
  (cheap) and an `<img>` per chip, which is more code than the
  feature currently warrants.
