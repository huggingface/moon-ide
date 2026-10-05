# ADR 0089: Desktop notification + sound when a turn finishes unfocused

Supersedes the "Desktop notifications" rejected alternative of
[ADR 0058](0058-agent-activity-indicator.md); the tray icon, window-icon
dot and urgency hint stay as they are.

## Context

The team asked for it: the tray dot and taskbar flash say "something
finished" but not _what_, and they're silent. Someone who kicked off
an agent and switched apps wants to hear it finish and read the
answer without switching back first.

## Decision

When a **user-facing** session's turn settles while the moon-ide window
is unfocused:

- play the freedesktop `complete` sound (`pw-play`, falling back to
  `paplay`). We play the file ourselves instead of setting the
  notification's `sound-name` hint because not every notification
  server (COSMIC, Cinnamon) supports that hint;
- post a desktop notification. The summary is `<workspace>: <session title>`
  and the body is the session's last assistant message, with whitespace
  collapsed and truncated to 280 chars. When the turn failed, the
  summary ends in "(failed)" and the body is the error. Clicking the
  notification focuses the window.

One notification per settled turn, not per "all agents idle". Each
finished conversation has its own answer to show.

Excluded: sub-agents (`task`) and coordinator workers, which report to a
parent rather than the user, and aborts, since the user pressed stop. A
turn that settles while the window is focused stays silent.

The coder publishes a `TurnSettled` broadcast. The Tauri layer's
`AgentIndicator` already tracks focus, so it consumes the broadcast
and posts the notification via `notify-rust`.

## Rejected alternatives

- **Web Audio chime / Web Notification API from the webview**: depends
  on WebKitGTK's autoplay and notification-permission policy, and it
  puts OS side effects in the UI.
- **Notifying only when the running-turn count drops to 0** (the ADR
  0058 trigger): when several sessions run in parallel, the first
  finished answer would wait on the slowest one.
- **A user setting to toggle it**: hardcode first. Add a toggle once
  someone asks for one.
