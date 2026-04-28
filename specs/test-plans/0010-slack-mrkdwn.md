# 0010 — Slack mrkdwn rendering

Brings rendered links / mentions / formatting / code / quotes to the
chat panel's read-only thread view. Brought forward from 11.4 because
the raw `<@U…>` and `<https://…|label>` tokens were unreadable.

## Setup

- Connect Slack and pick a bot (per `0008-slack-foundation.md`).
- Open at least one thread that contains the test message types
  below — easiest path is to send them to the bot from regular Slack
  yourself, then refresh the IDE thread.

## Scenarios

### Plain text

- A message with only plain text renders verbatim, with newlines
  preserved (the `<br>`-equivalent comes from `white-space: pre-wrap`
  in `.paragraph`).
- HTML entities decode: `a &amp; b` shows as `a & b`,
  `&lt;tag&gt;` as `<tag>`.

### Inline formatting

- `*bold*`, `_italic_`, `~strike~` each render with the right tag
  (`<strong>`, `<em>`, `<s>`).
- Mid-word markers don't trigger: `2*3=6` and `snake_case_word` stay
  literal.
- Nested formatting works: `*bold _italic_ bold*` shows bold with an
  italic island; same marker can't re-nest (`*outer *inner* outer*`
  treats the inner pair as literal).
- Inline `` `code` `` renders in monospace and skips formatting
  inside (`` `*not bold*` `` shows the asterisks).

### Code blocks

- ` ```code``` ` (single line) renders as a code block.
- Multi-line fenced blocks preserve indentation; one leading newline
  after the opening fence is trimmed (matches what Slack actually
  sends).
- Unclosed fences fall back to literal text — message body still
  shows the rest of the conversation, no swallowed lines.

### Block quotes

- Lines prefixed with `>` (with or without a space) become a
  quote block. Consecutive quote lines merge into one block. A
  non-quote line ends the quote.

### Links

- `<https://example.com>` → clickable link, label = URL.
- `<https://example.com|Open this>` → clickable link, label = "Open
  this". Click opens in OS default browser (Tauri opener), not the
  IDE webview.
- `<mailto:foo@bar.com>` opens the mail client.
- A non-allowlisted scheme (e.g. `<file:///etc/passwd>`) renders as
  literal text and is **not** clickable.

### User mentions

- `<@U…>` without a label resolves through `users.info` and
  initially shows the user_id, then upgrades to `@DisplayName` once
  the cache populates (typically <500 ms).
- `<@U…|alice>` shows `@alice` immediately, no `users.info` call.
- The connected user (the human whose token is configured) renders
  with their own name from `auth.test` — no extra round-trip.
- The active bot's mention renders with the bot's display name from
  the picker — no extra round-trip.
- A user who's been deactivated (returns `user_not_found`) falls
  back to the raw user_id (no infinite loading state).

### Channel and broadcast mentions

- `<#C…|general>` renders as `#general`.
- `<#C…>` (no label) renders as `#C…` (we don't fetch channel info).
- `<!here>`, `<!channel>`, `<!everyone>` render as `@here` /
  `@channel` / `@everyone` and use the warning-colour tint.
- `<!subteam^S…|@team>` renders as `@team`.
- `<!date^TIMESTAMP^FORMAT|FALLBACK>` renders as `FALLBACK` (we
  don't reformat).

### Session preview

- A session whose top message is `here is <https://example.com|a doc>`
  shows the preview as `here is a doc` — no `<…|…>` syntax leaking
  into the row.
- A preview cut mid-token (`hello <https://example.com/very-long-pa`)
  renders as `hello…`. The dangling `<` is never visible.
- Mentions in a preview (`<@U…>` from another user) initially show
  the raw ID, then swap to `@username` once `users.info` lands —
  same caching path as the thread view.
- A preview that's empty after stripping renders the fallback
  ("(empty message)" / "(no preview, see thread)") same as before.

### Disconnect resets the cache

- After disconnect → reconnect, the user cache is empty. Mentions
  re-fetch on the next render. (Verify by clearing `~/.config/dev.moon-ide.desktop/state.json`'s
  active bot pick or by toggling the connect modal disconnect path.)

### Block Kit precedence

- A bot reply that uses `section` blocks for content (typical
  moon-bot output ≤ 3 kB) renders with proper newlines, paragraphs,
  fenced code blocks and `*bold*` mrkdwn — even when the message's
  `text` field is the flattened single-line fallback. Verify by
  picking a recent multi-paragraph moon-bot answer and confirming
  the IDE matches Slack's own rendering line-for-line.
- A `divider` block renders as a centred `———` between sections.
- A moon-bot reply with the standard footer renders three pill
  buttons under the body — "Response" / "Download" / "Session" —
  each opening the underlying URL externally via the OS browser.
  `style: "primary"` / `"danger"` buttons use the accent / danger
  tint. Buttons without a URL (interactive `value`-only) and other
  element types (datepicker, select, overflow) are silently dropped
  from the row.
- A bot reply long enough to use `markdown` blocks (> ~3 kB
  post-conversion, unusual but possible) shows literal `**` and
  `[label](url)` until the deferred Rust converter lands. This is a
  known limitation, not a bug; do not file.
- A _human_ DM (typed in Slack) still renders correctly: Slack
  auto-attaches a `rich_text` block, our extractor skips it, and we
  fall back to the user's typed `text`.
- A session-row preview for a bot session reflects the blocks too
  (try a session whose top message uses `section` blocks — the
  preview should be the readable summary, not the flattened
  fallback).

### Unit tests

- `bun run test:js` exercises the frontend parser end-to-end:
  `src/lib/util/slackMrkdwn.test.ts` covers formatting rules,
  angle-token shapes, entity decoding, mention collection, and the
  preview flatten path (including dangling-token trimming).
  Re-run after any change to `slackMrkdwn.ts`.
- `cargo test -p moon-slack` covers Block Kit extraction (`section`
  / `markdown` / `divider`, fallback to `text`, `rich_text`
  skipping, mrkdwn-vs-plain_text) — see `crates/moon-slack/src/client.rs`
  tests. Re-run after any change to `text_from_blocks` or the
  `RawBlock` deserialisation.

## Known limitations

- Custom emoji (`:tada:`) render as their `:shortcode:` text.
- Channel names without Slack-cached labels show `#C…` instead of
  the human name. Adding a `conversations.info` lookup would mean a
  second async cache and more API calls; skipped until requested.
- Block Kit messages (rich attachments) only render the top-level
  `text` field; structured blocks are ignored.
