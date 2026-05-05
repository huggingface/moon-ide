# Test plan 0041: right-side panel — single shared slot for chat / coder

- **Date**: 2026-05-05
- **Phase**: Phase 6 (Coder) polish

## What shipped

- Chat and coder are now mutually exclusive tenants of a single
  right-side panel slot. Opening one swaps the other out instead
  of rendering two narrow columns.
- The pick is persisted as `AppState.right_panel: 'chat' |
'coder' | null` and restored across launches.
- Backend writer is the new `ui_set_right_panel` Tauri command.
  The slack poller's `panel_visible` input is fed from this
  exact value (chat-only), so the polling loop pauses immediately
  when the user swaps over to the coder panel.
- Both panels share one width — toggling between them no longer
  reflows the editor area to a different column size.
- Coder panel header now uses the same uppercase / letter-spaced
  font treatment as the chat panel (the rest of the coder
  controls — dot, identity, target chip, stop, sign-out — stay
  intact, the change is purely typographic).

## How to test

Prerequisites: `bun install`, `bun run dev`, fresh `state.json`.

1. **First-run defaults**

   Quit the app, delete `~/.config/dev.moon-ide.desktop/state.json`
   (or the platform-equivalent), relaunch.

   Expected: neither right-side panel renders. The chat / coder
   pips in the status bar are inactive.

2. **Open chat**

   Click the `chat` pip in the status bar.

   Expected:
   - Chat panel appears on the right.
   - `cat ~/.config/dev.moon-ide.desktop/state.json | jq .right_panel`
     prints `"chat"`.

3. **Swap to coder (mutual exclusion)**

   With chat still open, click the `coder` pip.

   Expected:
   - Chat panel unmounts; coder panel mounts in the same slot
     **without flicker or width change** — they share one
     `rightPanelWidth`.
   - Status bar: only the `coder` pip is in the active state.
   - Disk: `right_panel == "coder"`.

4. **Toggle coder closed**

   Click the `coder` pip again.

   Expected: slot is empty; `right_panel == null`.

5. **Restore on relaunch**

   With chat open, quit and relaunch the app.

   Expected: chat panel is open on first paint. Same for coder.
   Same for the closed state.

6. **Slack polling pauses when chat isn't mounted**

   Connect Slack, pick a bot, open a thread (so the polling loop
   is active — see test plan 0011). Open `tracing` logs at
   `info` and watch for `slack: poll` ticks.
   - Swap to coder. Expected: polling stops within 1 tick (~3 s
     at the default cadence).
   - Swap back to chat. Expected: polling resumes within ~3 s.
   - Close the panel entirely. Expected: polling stays paused.

7. **`Ctrl+L` still toggles chat**

   Press `Ctrl+L` from any focus. Expected: chat panel toggles in
   the right slot, swapping coder out if it was open.

8. **Width is shared**

   With chat open, drag the splitter to a wide setting (~600 px).
   Swap to coder. Expected: coder mounts at the same width.
   Drag coder to a narrow setting (~280 px). Swap back to chat.
   Expected: chat mounts at the new narrow width.

   (Width itself is not persisted across launches — that's
   covered in the "Known limitations" section.)

9. **Header consistency**

   Open the chat panel: header reads `CHAT` (uppercased,
   letter-spaced, muted).

   Swap to coder: header reads `CODER` in the same uppercased /
   letter-spaced font, with the existing dot + identity + target
   chip + stop / sign-out controls layered alongside.

## What must keep working

- The slack poller's `panel_visible` input is the boolean
  `right_panel == 'chat'`. Coder being mounted must not keep
  slack polling.
- `app_state_save` (the frontend's session-persist call) merges
  `slack` and `right_panel` from disk and ignores whatever the
  payload contained for those fields. A persist tick during a
  panel toggle must not clobber the just-written `right_panel`.
- Command palette: `Chat: Show Panel` / `Chat: Hide Panel` and
  `Chat: Connect Slack…` still work — they call
  `slack.togglePanel()` / `slack.setPanelVisible(true)`, which
  now route through `rightPanel`.
- F6 focus cycle still picks up whichever surface is mounted in
  the right slot.

## Known limitations

- Width is not persisted across launches. Both panels share one
  in-memory width, but it resets to the 360 px default on every
  launch. Adding it to `AppState` is a 5-minute follow-up; left
  out for now because no user has asked.
- We're keeping `slack.svelte.ts:setPanelVisible` (a thin wrapper
  around `rightPanel.set('chat')`) for the `Chat: Connect Slack…`
  command-palette entry. That entry wants the modal to come up
  with the chat panel, not in isolation. Future cleanup could
  collapse it into `rightPanel.set` if more callers materialise.

## Related

- Specs: [`specs/coder.md`](../coder.md#layout),
  [`specs/slack-chat.md`](../slack-chat.md#multi-writer-story).
- Prior test plans:
  [0008-slack-foundation.md](./0008-slack-foundation.md),
  [0011-slack-polling.md](./0011-slack-polling.md),
  [0039-coder-skeleton.md](./0039-coder-skeleton.md),
  [0040-coder-write-tools.md](./0040-coder-write-tools.md).
