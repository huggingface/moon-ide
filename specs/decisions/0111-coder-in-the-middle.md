# ADR 0111: Coder panel in the middle; Slack chat and local autocomplete removed

## Context

The window was sidebar | editor | side panel, with the side panel a
single slot shared by two tenants: the coder and a Slack chat client
(Phase 11). Coder sessions are the primary surface, yet they sat in a
380 px column at the far edge while the editor took the centre.

Two features were no longer wanted:

- **Slack chat** — a Web API client that DMed a bot from the side
  panel (`moon-slack`, a background poller, a keyring token).
- **Local autocomplete** — `Ctrl+T` asked a managed `llama-server`
  (Sweep next-edit model) for a patch and applied it to the buffer.

## Decision

- The layout is sidebar | coder | editor. The coder column opens at
  600 px and is resizable; the editor area (files, diffs, browser
  tabs, with the bottom panel under it) takes the rest. This is a
  trial: the order and the default width are expected to be tuned
  after living with them.
- Slack chat and local autocomplete are deleted outright: crates,
  commands, protocol types, persisted state, status-bar controls,
  palette entries and the `Ctrl+T` binding. `Ctrl+Space` LSP completion
  is unaffected.
- With one tenant left, the slot is a flag. `AppState.right_panel`
  (`'chat' | 'coder' | null`) becomes `AppState.coder_panel_visible`,
  written by the frontend through `app_state_save` like the bottom
  panel. The dedicated `ui_set_right_panel` command existed so the
  Slack poller could react to the panel closing without waiting for a
  debounced save; nothing needs that now.

Old `state.json` files keep their `slack`, `next_edit` and
`right_panel` keys until the next save drops them; the coder panel
starts closed once. A Slack token left in the OS keyring
(`moon-ide` / `slack-user-token`) is not cleaned up.

## Rejected alternatives

- **Keep `right_panel` as a one-variant enum.** Leaves a name that
  describes neither the position nor a choice.
- **Persist the coder width.** Nobody has asked; the default is the
  thing being tuned.
- **Move the bottom panel under the coder as well.** Terminals and
  logs go with the files they act on; spanning both columns would take
  height from the transcript.
