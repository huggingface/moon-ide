# Test plan 0008: Slack chat panel foundation

- **Date**: 2026-04-27
- **Phase**: Phase 11.0

## What shipped

- New right-side chat panel gated on an opt-in Slack connect
  flow. Empty / connecting / picker / active-bot / error states
  all live in `ChatPanel.svelte`; status-bar pip turns green
  when connected.
- New `moon-slack` crate: hand-rolled Web API client wrapping
  `auth.test`, `conversations.list` (DM types), `users.info`,
  plus `list_dm_bots` which scans the 50 most recent DMs and
  filters to bots. Errors classify into `is_auth_failure()` to
  drive the clear-and-reset path.
- Tokens live in the OS keyring (`libsecret` / Keychain /
  Credential Manager) under `moon-ide` / `slack-user-token`;
  never written to `state.json`. `AppState.slack` holds only
  IDs + display metadata for the picked bot plus the panel's
  open/closed state.
- Bot discovery is bounded at 50 DMs (`DM_SCAN_LIMIT`) with the
  cap surfaced upfront in the connect modal and picker copy;
  there's no hardcoded default bot — the user picks from their
  own DM list.
- Auth-failure recovery is consistent: any `auth.test` that
  returns an auth-family error (revoked, expired, invalid)
  clears keyring + cache + persisted bot in one shot, so the
  next render drops back to the empty state.
- Persisted slack slice and the frontend session persist are
  disjoint writers; `app_state_save` merges so neither can
  clobber the other.

## What must keep working

- The first launch with no keyring entry shows the empty state in the
  panel ("Connect Slack to chat with a bot from the IDE."). No network
  calls fire on startup.
- After a successful connect, the panel shows:
  - "Connected as &lt;real_name&gt;" + workspace name.
  - A spinner card "Scanning your 50 most recent DMs for bots…" while
    `slack_list_dm_bots` is in flight. The "50" matches `DM_SCAN_LIMIT`
    in `moon-slack` and the connect-modal copy.
  - On success: a list of bot rows (avatar + display name + `@username`)
    in Slack's DM order. Clicking a row makes that bot the active bot
    and replaces the picker with the active-bot card.
  - On a DM window with zero bots: "No bots in your 50 most recent
    DMs." with a Refresh button — copy explicitly mentions the cap so
    the user knows to bump an older bot DM by sending a "hi" from
    regular Slack.
  - On a scan error: the error message + a Retry link.
- The token never appears in `app_state.json`, in the persisted
  session blob, or in any frontend state save. Inspect the AppState
  file after connecting:
  ```bash
  cat ~/.config/dev.moon-ide.desktop/state.json | jq
  ```
  Confirm there's no `xoxp-` substring anywhere. The `slack.active_bot`
  field should hold IDs + display metadata only (after picking). Inspect
  the keyring:
  ```bash
  secret-tool search service moon-ide account slack-user-token
  ```
  (Linux). The token should be the only entry.
- Disconnect (panel header button or palette command) confirms via the
  native dialog, drops the keyring entry, drops the persisted bot pick,
  and the panel returns to the empty state. Inspect the keyring as above
  to confirm no entries remain; `cat state.json | jq .slack` should show
  `{"active_bot": null}`.
- Closing the panel and reopening it does **not** re-trigger
  `slack_list_dm_bots` if a bot is already picked. Discovery only fires
  when there's no active bot.
- `Esc` inside the connect modal closes it without affecting state.
- The chat panel resize drag works between 240 and 640 px and survives
  a workspace switch (the panel itself is workspace-independent).

## How to test

### 11.0a — disconnected + empty state

1. Start with no Slack connection (`secret-tool clear service moon-ide
account slack-user-token` if you've already connected before).
2. Launch the IDE. The chat panel should not be visible by default.
3. Click the `● chat` button in the status bar (pip is muted). The
   panel slides in. It shows "Connect Slack to chat with a bot from
   the IDE." with a button.
4. Click "Connect Slack". Modal appears with a setup walk-through.
   Each `api.slack.com/apps` link opens the system browser, not the
   webview.
5. Close the modal with `Esc`. State unchanged.

### 11.0b — invalid token

1. Open the connect modal.
2. Paste `not-a-token`. Click Connect. Inline error: "token must
   start with 'xoxp-' (Slack User OAuth Token)". No keyring write.
3. Paste `xoxp-deadbeef`. Click Connect. Inline error from Slack
   (e.g. `Internal: Slack API error (auth.test): invalid_auth`). No
   keyring write.

### 11.0c — successful connect + bot picker

1. Create a Slack app per the modal's walk-through. Add the nine
   user token scopes the modal lists — `chat:write`, `im:history`,
   `im:read`, `im:write`, `users:read`, `reactions:read`,
   `reactions:write`, `files:read`, `files:write`. (Phase 11.0 only
   exercises the first five; the rest are claimed upfront so later
   sub-phases don't need a Slack-app re-install — see
   `specs/slack-chat.md`.) Install to your workspace.
2. **Pre-condition**: DM the bot you want to chat with at least once
   from regular Slack (web, mobile, or desktop), and make sure that DM
   is in your 50 most recent. The picker walks the 50 newest DMs only
   — older bot DMs won't be found unless you send a quick "hi" from
   regular Slack to bump them.
3. **Verify the modal flags the cap.** Open the connect modal and
   confirm the lede mentions "50 most recent" upfront. The number
   should match `DM_SCAN_LIMIT` in `crates/moon-slack/src/client.rs`.
4. Copy the **User OAuth Token** (starts with `xoxp-`). Paste into the
   modal. Click Connect.
5. Modal closes. Panel shows:
   - "Connected as &lt;your real name&gt;" + "Workspace: &lt;team name&gt;".
   - A spinner card "Scanning your 50 most recent DMs for bots…". This
     should finish in ~10–20 s on a warm network.
   - When discovery returns: a list of bot rows. The bot you DM'd
     should be among them. Each row shows avatar + display name + a
     `@username` slug.
6. Click your target bot's row. The picker disappears, replaced by an
   active-bot card with the same identity + a "Switch bot" link.
7. The status-bar chat pip turns green.
8. Confirm persistence:
   ```bash
   cat ~/.config/dev.moon-ide.desktop/state.json | jq .slack
   ```
   should print the picked bot's `user_id`, `dm_channel_id`,
   `username`, `real_name`, `display_name`, `image_url`. No `xoxp-`
   anywhere in the file.
   ```bash
   secret-tool lookup service moon-ide account slack-user-token
   ```
   should print the `xoxp-…` token.

### 11.0d — bot picker edge cases

1. **No bots in the 50-DM window.** Connect with an account whose 50
   most recent DMs are all human — for example, on a workspace where
   you've never DM'd a bot. The picker card should read "No bots in
   your 50 most recent DMs." with a Refresh button. DM a bot from
   regular Slack, then click Refresh; the bot should appear.
2. **Bot lives outside the window.** If you DM a bot once and then
   accumulate 50+ newer DMs with humans, the bot won't show up in the
   scan. From regular Slack, send the bot a fresh "hi" to bump its DM
   into the recent window, then click Refresh in the picker. The bot
   should now appear. (This validates the disclosure in the modal /
   picker copy is accurate.)
3. **Scan failure.** Disconnect from the network mid-discovery (or
   revoke the token while the scan runs). The picker card should show
   the error message with a Retry link. Click Retry once the network
   is back; discovery should succeed.
4. **Switch bot.** With a bot active, click "Switch bot" in the
   active-bot card. The picker re-runs discovery and the active-bot
   card disappears. `state.json` should now have `slack.active_bot:
null`. Pick a different bot; persistence updates accordingly.

### 11.0e — restart persists token, bot pick, and panel visibility

1. With a valid connection and a picked bot, **leave the chat panel
   open** and fully quit the app (`Ctrl+Q`).
2. Relaunch.
3. The chat panel should already be visible on first paint (no need
   to click the status-bar pip). Without prompting for a token _and
   without re-running discovery_, it should jump straight to the
   active-bot card with the same bot you picked before quitting.
4. Confirm the keyring entry is intact:
   ```bash
   secret-tool lookup service moon-ide account slack-user-token
   ```
   should print the same `xoxp-…` token. If `secret-tool` itself is
   missing on Linux, `apt install libsecret-tools` (the underlying
   D-Bus interface is what keyring talks to and is already present).
5. Confirm `state.json`:
   ```bash
   cat ~/.config/dev.moon-ide.desktop/state.json | jq .slack
   ```
   should show `panel_visible: true`, the same `active_bot` you
   picked, and no `xoxp-` substring anywhere in the file.
6. Hide the panel (status-bar pip), quit, relaunch. Panel should
   stay closed. `state.json`'s `slack.panel_visible` should now be
   `false`.

### 11.0f — token revoked externally

1. With a valid connection running, go to the Slack app's "OAuth &
   Permissions" page in your browser and **Revoke Tokens**.
2. In the IDE, close and reopen the chat panel (forces a status
   refresh).
3. The panel should drop back to the empty state ("Connect Slack…")
   within one poll. Confirm the keyring entry is gone:
   ```bash
   secret-tool lookup service moon-ide account slack-user-token
   ```
   should print nothing (and exit non-zero). Confirm the persisted
   bot pick is gone:
   ```bash
   cat ~/.config/dev.moon-ide.desktop/state.json | jq .slack
   ```
   should print `{"active_bot": null}`.

### 11.0g — disconnect

1. From the connected state, click "Disconnect" in the panel header.
   Native confirm appears.
2. Confirm. Panel returns to the empty state. Keyring entry is gone.
   `state.json`'s `slack.active_bot` is `null`.
3. Open the command palette and search "Chat". Only `Chat: Show Panel`
   / `Chat: Hide Panel` and `Chat: Connect Slack…` are listed —
   `Chat: Disconnect Slack` is hidden.

### 11.0h — concurrent writers don't clobber

1. Connect, pick a bot. Confirm `state.json`'s `slack.active_bot` is
   set.
2. Open a few files (each open triggers a session-persist).
3. Re-read `state.json`. `slack.active_bot` should be unchanged —
   `app_state_save` preserves the slack slice across frontend writes.
4. Run `slack_clear_bot` (via the "Switch bot" affordance), then
   immediately switch tabs (forces another session-persist). Confirm
   `slack.active_bot` is `null` and stays `null`. The next bot pick
   should write through.

### 11.0i — quality gates

```bash
bun run check     # tsgo + svelte-check + cargo check (workspace)
bun run lint      # oxlint + cargo clippy -D warnings
bun run test      # cargo test --workspace --exclude moon-desktop
bun run fmt:check
```

All four must pass cleanly.

## Known limitations (deferred to 11.1+)

- **No DM-channel scanning for new top-level messages.** The DM
  channel is known (`active_bot.dm_channel_id`) but no
  `conversations.history` call fires. Sessions / threads / messages
  are 11.1.
- **No polling** loop. Phase 11.0 does on-demand `slack_status`
  checks only (panel mount + first toggle).
- **Discovery is bounded at 50 DMs.** Bots that live outside your 50
  most recently active DMs won't appear in the picker. The connect
  modal and picker UI both flag this upfront so the user knows to
  bump older bot DMs by sending a "hi" from regular Slack. No
  pagination, no streaming — keep it simple. If 50 turns out to be
  too tight, we revisit with a concrete report (not a hunch).
- **No fallback for users who haven't DM'd the bot.** If the bot you
  want isn't in your DM list at all, you have to DM it from regular
  Slack first and Refresh. There's no "paste user ID manually" escape
  hatch yet. We'll add it when somebody actually hits this — see
  `specs/slack-chat.md#why-not-also-paginate-userslist-for-bots-the-user-hasnt-dmd`.
- **No reactions, attachments, or file uploads.** Slack supports them;
  we don't render them in v1. See `specs/slack-chat.md` for the full
  list of deliberate omissions.
