# Test plan 0017: Slack workspace + bot card chrome rework

- **Date**: 2026-05-03
- **Phase**: post-Phase 11.4 polish

## What shipped

- `team:read` is now part of the upfront Slack scope grant
  (`specs/slack-chat.md`). The connect-modal scope checklist
  in `src/lib/components/ChatConnectModal.svelte` lists it
  with the other nine.
- `SlackIdentity` (`crates/moon-protocol/src/slack.rs`) gained
  an `icon_url: Option<String>` field. TS bindings regenerated
  via `cargo test -p moon-protocol`; `src/lib/protocol.ts`
  hand-mirror updated to match.
- `SlackClient::auth_test` (`crates/moon-slack/src/client.rs`)
  now calls `team.info` after `auth.test` and populates the
  workspace icon URL. `team.info` failures (e.g. workspace
  revoked the scope) log a warning and fall back to
  `icon_url = None` rather than failing the connect handshake
  — chrome shouldn't gate auth.
- Icon picking lives in a free `pick_team_icon` helper so the
  preference order (132 → 102 → 88 → 68 px, drop
  `image_default: true`) is unit-testable without HTTP.
- Chat panel workspace card (`src/lib/components/ChatPanel.svelte`):
  - Layout is now `[avatar] [workspace name / user name stack]
[icon-button disconnect]`, mirroring the bot card.
  - The avatar uses the real `team.info` icon when available
    and falls back to an initial-letter placeholder (same
    `.avatar-placeholder` style as the bot card) when the
    workspace uses Slack's auto-generated default icon, when
    `team:read` is missing, or when `team.info` fails.
  - The Disconnect text link became a square icon button
    (`disconnectIcon` snippet + the existing `.icon-button`
    class) with an accessible `aria-label` and tooltip. The
    confirm dialog wording is unchanged.
  - Same treatment for the bot card's "Switch bot" text
    link: now an icon button using a horizontal two-arrow
    swap glyph (`switchBotIcon` snippet) with the same
    `.icon-button` class and an aria-label / tooltip pair.
    Both card "action" slots now read identically as
    square icon buttons.
  - Sessions list moved out of its `.card.sessions-card`
    wrapper into a `.sessions` section that flows directly
    in the panel, mirroring how the active-thread view is
    laid out. The sessions header is now sticky and edge-
    to-edge (sharing styles with `.thread-header`) so the
    "New session" / "Reload sessions" controls stay
    reachable while the bot card scrolls up. Empty / error
    states use `.thread-empty` / `.thread-error` for
    consistency with the thread view's flush copy.
  - Removed unused `.card-row` / `.card-label` /
    `.card-value` / `.section-header` CSS — those classes
    were only used by the old workspace-card and sessions-
    card markup.

## How to test

Prerequisites:

- A Slack workspace where you own a `xoxp-…` user token.
- The Slack app the token belongs to has been (re)installed
  with the full ten-scope grant from
  `ChatConnectModal.svelte`, including the new `team:read`.

1. **Fresh connect with the new scope.**
   1. From a freshly-launched IDE (no token in the keyring),
      open the chat panel and click **Connect Slack**.
   2. The modal scope list now shows `team:read — show your
workspace icon on the chat panel` between `users:read`
      and `reactions:read`.
   3. Paste your `xoxp-…` token and click Connect. The
      connect handshake should succeed without surfacing a
      `missing_scope` error.
   4. Expected: the chat panel renders the workspace card
      with the workspace's real icon on the left, the
      workspace display name as the bigger line, and your
      Slack username as the muted secondary line.
   5. Disconnect button is now a small icon (door + arrow)
      in the right-hand slot. Hovering shows a "Disconnect
      Slack" tooltip.
2. **Disconnect still works.**
   1. Click the icon button. The native confirm dialog
      ("Disconnect Slack? Your token will be removed from
      the OS keyring.") should appear unchanged.
   2. Confirm. Panel returns to the empty state. Open the
      OS keyring (`secret-tool lookup service moon-ide
account slack-user-token` on Linux) and verify the
      entry is gone.
3. **Switch bot icon-button.**
   1. Connect with a workspace where you've DM'd at least
      two bots in your last 50 DMs. Pick one in the bot
      picker.
   2. The bot card now shows the bot avatar + name on the
      left and a small swap-arrow icon button on the
      right (no "Switch bot" text). Hover shows the
      "Switch bot" tooltip.
   3. Click the icon button. The bot picker should
      reappear. Pick a different bot.
   4. Expected: the bot card swaps to the new bot's
      avatar/name and the session list reloads for the
      new bot's DM channel.
4. **Sessions list flows in the panel.**
   1. Pick a bot with enough sessions to overflow the
      panel.
   2. Expected: the sessions list is no longer wrapped
      in a card — only the workspace and bot cards remain
      cards; below them the "Sessions" header sits
      directly on the panel's `--m-bg-1` background and
      session rows flow underneath. Visual weight roughly
      matches the active-thread view.
   3. Scroll the panel down. The "Sessions" header stays
      pinned at the top of the scroll area (same sticky
      behaviour as the thread header) with its bottom
      border showing once content scrolls under it; the
      bot card scrolls up behind it.
   4. Click into a session, then "← Sessions" to come
      back. The header layout / sticky behaviour should
      be visually continuous between the two views — same
      vertical position, same divider, same icon-button
      slot on the right.
   5. With zero sessions, the empty-state copy ("No
      sessions yet…") sits flush in the panel rather
      than indented inside a card.
5. **Default icon falls back to the placeholder.**
   1. Find or create a workspace where the admin hasn't
      uploaded a custom icon (Slack falls back to its
      auto-generated glyph there, with `image_default:
true`).
   2. Connect with a token in that workspace.
   3. Expected: the avatar slot shows the workspace's
      initial letter on a tinted square (same look as the
      bot picker placeholder), not Slack's generic glyph.
6. **`team:read` not granted.**
   1. In the Slack app's OAuth & Permissions, remove
      `team:read` and reinstall.
   2. Disconnect from the IDE, paste the new token, and
      connect.
   3. Expected: connect still succeeds (`auth.test` works
      without `team:read`). The workspace card renders
      with the initial-letter placeholder. A
      `team.info failed; falling back to placeholder
workspace icon` line appears in the dev tracing logs
      (`RUST_LOG=warn`). The panel is otherwise fully
      functional.
   4. Re-add `team:read`, reinstall, reconnect: the real
      icon should reappear after the next `slack_status`
      tick (panel reload is enough).
7. **Long workspace / user names truncate cleanly.**
   1. Connect to a workspace whose display name is longer
      than ~30 chars.
   2. Expected: the workspace name truncates with `…`
      rather than wrapping or pushing the disconnect button
      off-screen. Same for the username line.

## What must keep working

Regression checks. If any of these break, the commit needs a
follow-up.

- Existing connect modal copy still claims all required
  scopes upfront — users that haven't reinstalled since
  before this change will see `missing_scope (need
team:read)` from `team.info`, but `auth.test` itself does
  not require `team:read`, so the handshake still succeeds
  and only the icon is missing. (The warning log + fallback
  placeholder cover that case.)
- The bot card layout below the workspace card is unchanged.
- The Sessions / thread headers and their icon-button
  styling are unchanged — the new disconnect button reuses
  the same `.icon-button` class for consistency.
- `cargo test -p moon-slack` covers `pick_team_icon` for
  the four cases (132 preferred, fallback to smaller,
  `image_default: true` returns `None`, missing `icon`
  block returns `None`).

## Known limitations

- The icon URL is fetched fresh on every `slack_status`
  call (panel mount, post-disconnect reconnect). Cheap and
  uncached today; if the round-trip ever shows up in
  profiles, cache it on `SlackState` keyed by `team_id`.
- Workspace-icon updates done in Slack while the IDE is
  running aren't picked up until the panel hides + shows
  again (or the user disconnects/reconnects). Not worth a
  poller of its own.
- The icon currently renders at the same 32 px size as the
  bot avatar. We pull `image_132` for crisp rendering on
  HiDPI displays; if a future redesign uses a larger slot,
  bump the preferred size to `image_230` or
  `image_original`.

## Related

- Specs: `specs/slack-chat.md` (required scopes table,
  connect handshake section).
- Prior test plans: `specs/test-plans/0008-slack-foundation.md`
  (original `auth.test` + identity wiring),
  `specs/test-plans/0009-slack-read-only-chat.md` (panel
  layout this rework sits in).
