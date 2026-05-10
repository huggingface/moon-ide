# Test plan 0041: right-side panel slot + read_file ranges

- **Date**: 2026-05-05
- **Phase**: Phase 6 (Coder) polish

## What shipped

- Chat and coder are now mutually exclusive tenants of a single
  right-side panel slot. Opening one swaps the other out instead
  of rendering two narrow columns.
- The pick is persisted as `AppState.right_panel: 'chat' | 'coder'
| null` and restored across launches.
- Backend writer is the new `ui_set_right_panel` Tauri command.
  The slack poller's `panel_visible` input is fed from this exact
  value (chat-only), so the polling loop pauses immediately when
  the user swaps to the coder panel.
- Both panels share one width — toggling between them no longer
  reflows the editor area.
- Coder panel header now uses the same uppercase / letter-spaced
  font treatment as the chat panel. Both headers gained an icon
  button that swaps the slot to the _other_ surface (chat header
  → `</>` to coder, coder header → speech bubble to chat). The
  green connection dot on the coder header was dropped — the
  identity username already conveys signed-in status.
- `read_file` agent tool gained optional `start_line` /
  `end_line` parameters (1-based, inclusive, `end_line` clamped
  to EOF) and now emits each line prefixed with `<line_no>|`.
  Response echoes the effective range plus `total_lines` so the
  model can detect short reads. `grep`'s tool description nudges
  the model to feed its line numbers back into `read_file` for
  surgical follow-ups.

## How to test

Prerequisites: `bun install`, `bun run dev`, fresh `state.json`.

### Right-side panel slot

1. **First-run defaults**

   Quit the app, delete `~/.config/moon-ide/state.json`
   (or the platform-equivalent), relaunch.

   Expected: neither right-side panel renders. The chat / coder
   pips in the status bar are inactive.

2. **Open chat**

   Click the `chat` pip in the status bar.

   Expected:
   - Chat panel appears on the right.
   - `cat ~/.config/moon-ide/state.json | jq .right_panel`
     prints `"chat"`.

3. **Swap to coder via header icon**

   With chat still open, click the `</>` icon on the right side
   of the **chat panel header**.

   Expected:
   - Chat panel unmounts; coder panel mounts in the same slot
     **without flicker or width change**.
   - Status bar: only the `coder` pip is active.
   - Disk: `right_panel == "coder"`.

4. **Swap back via the coder header icon**

   Click the speech-bubble icon on the right side of the **coder
   panel header**.

   Expected: chat replaces coder in the slot. Disk:
   `right_panel == "chat"`.

5. **Toggle coder closed**

   Click the `coder` pip in the status bar twice (open, close).

   Expected: slot is empty; `right_panel == null`.

6. **Restore on relaunch**

   With chat open, quit and relaunch the app.

   Expected: chat panel is open on first paint. Same for coder.
   Same for the closed state.

7. **Slack polling pauses when chat isn't mounted**

   Connect Slack, pick a bot, open a thread (so the polling loop
   is active — see test plan 0011). Open `tracing` logs at
   `info` and watch for `slack: poll` ticks.
   - Swap to coder. Expected: polling stops within 1 tick (~3 s
     at the default cadence).
   - Swap back to chat. Expected: polling resumes within ~3 s.
   - Close the panel entirely. Expected: polling stays paused.

8. **`Ctrl+L` still toggles chat**

   Press `Ctrl+L` from any focus. Expected: chat panel toggles in
   the right slot, swapping coder out if it was open.

9. **Width is shared**

   With chat open, drag the splitter to ~600 px. Swap to coder
   via the header icon. Expected: coder mounts at 600 px.

10. **Header consistency**

    Open the chat panel: header reads `CHAT` (uppercased,
    letter-spaced, muted) with a `</>` button at the right.

    Swap to coder: header reads `CODER` in the same font, with
    the speech-bubble swap button, target chip, stop, and
    sign-out controls. No green status dot.

### `read_file` ranges + line numbers

Sign in to Hugging Face (per test plan 0039). Send the agent the
prompts below and inspect the tool calls in the panel.

11. **Plain `read_file` returns numbered lines**

    Prompt: `read AGENTS.md`.

    Expected tool call result:
    - `content` shows lines like `  1|# Agent instructions`.
    - `start_line == 1`, `end_line == total_lines`,
      `truncated == false`.

12. **Range read**

    Prompt: `show me lines 5–15 of specs/coder.md`.

    Expected: the agent calls `read_file` with `start_line: 5`
    and `end_line: 15`. Result has 11 numbered lines, all in the
    requested range, and `total_lines` reflects the full file.

13. **Out-of-range end clamps**

    Prompt: `read lines 1–100000 of README.md`.

    Expected: tool call succeeds; `end_line` in the response
    equals `total_lines` (clamped silently).

14. **Grep → narrow read loop**

        Prompt: `find every place we call write_file in moon-coder

    and show me the lines around the third hit`.

        Expected: the agent runs `grep` first, picks a `path:line:`
        hit, then calls `read_file` with `start_line` / `end_line` a
        few lines around it. Tool result for the read is small and
        explicitly bounded.

15. **Invalid range rejected cleanly**

    From a Rust integration / curl, fire a tool call with
    `start_line: 0` or `end_line < start_line`. Expected: an
    `InvalidArgs` error with a message about 1-based indexing.

## What must keep working

- The slack poller's `panel_visible` input is the boolean
  `right_panel == 'chat'`. Coder being mounted must not keep
  slack polling.
- `app_state_save` (the frontend's session-persist call) merges
  `slack` and `right_panel` from disk and ignores whatever the
  payload contained for those fields.
- Command palette: `Chat: Show Panel` / `Chat: Hide Panel` /
  `Chat: Connect Slack…` still work — they call
  `slack.togglePanel()` / `slack.setPanelVisible(true)`, which
  now route through `rightPanel`.
- F6 focus cycle still picks up whichever surface is mounted in
  the right slot.
- `read_file` calls without `start_line` / `end_line` keep
  working; only the response shape changed (added line-number
  prefix, `start_line` / `end_line` / `total_lines` fields).

## Known limitations

- Width is not persisted across launches. Both panels share one
  in-memory width that resets to the 360 px default on every
  launch. Adding it to `AppState` is a 5-minute follow-up; left
  out for now because no user has asked.
- We're keeping `slack.svelte.ts:setPanelVisible` (a thin wrapper
  around `rightPanel.set('chat')`) for the `Chat: Connect Slack…`
  command-palette entry, which wants the modal to come up _with_
  the chat panel.
- `read_file`'s byte cap is now applied to the rendered numbered
  output (which is ~5–10 % bigger than the source for typical
  source files). On a file that hits the 200 kB cap mid-stream,
  `truncated == true` and the agent should retry with a
  narrower range.

## Related

- Specs: [`specs/coder.md`](../coder.md#tool-surface),
  [`specs/slack-chat.md`](../slack-chat.md#multi-writer-story).
- Prior test plans:
  [0008-slack-foundation.md](./0008-slack-foundation.md),
  [0011-slack-polling.md](./0011-slack-polling.md),
  [0039-coder-skeleton.md](./0039-coder-skeleton.md),
  [0040-coder-write-tools.md](./0040-coder-write-tools.md).
