# ADR 0058: OS-level agent-activity indicator (tray icon + window-icon dot)

## Context

With process-per-workspace (ADR 0014) and concurrent coder sessions
(ADR 0016), a user routinely kicks off agents in one workspace and
switches to another — or to a different app entirely. Nothing at the
OS level says "an agent is still running over there" or "your agents
finished, come look". The in-window pips (status bar, folder bar)
only help if the window is visible. The team runs Linux (Cinnamon /
X11); the desktop's grouped window list resolves icons from the
`.desktop` file, so the per-workspace `_NET_WM_ICON` badge is not a
reliable notification surface on its own.

## Decision

One process-wide "running turns" count in `moon-coder` (a `watch`
channel incremented/decremented by an RAII guard around every
`spawn_turn_loop` task) drives an `AgentIndicator` in the Tauri
layer with three states: idle, running, finished-unseen. Observable
behaviour:

- **Window icon**: the workspace badge gains an amber dot while any
  turn runs, a green dot when the last turn settles while the window
  is unfocused. Visible in alt-tab and ungrouped X11 taskbars.
- **Transient tray icon** (StatusNotifierItem): exists only in the
  running / finished-unseen states, painted with the same badge +
  dot, removed when idle. With several workspaces open, an icon
  that only appears when there is something to say _is_ the
  notification; no permanent tray presence. Menu carries "Focus
  window" (appindicator trays deliver no left-click events; the
  click handler is still installed for platforms that have them).
- **Taskbar flash**: a turn settling while unfocused raises the WM
  urgency hint (`request_user_attention`), so grouped taskbars that
  never show per-window icons still signal. Focusing the window
  acknowledges: green dot and tray icon clear.

A turn that settles while the window is focused skips the finished
state entirely — the user is watching the panel.

Counted: everything through `spawn_turn_loop` (visible sessions,
worktree sessions, coordinator workers). Not counted: detached
`task` sub-agents (ADR 0053) — fire-and-forget helpers the parent
collects later, not something the user waits on.

## Rejected alternatives

- **Badge counts / progress on the launcher** (`setBadgeCount`):
  rides the Unity launcher D-Bus API, which Cinnamon's grouped
  window list ignores.
- **Always-visible tray icon per workspace**: N workspaces = N
  icons of permanent clutter to convey "nothing happening".
- **Per-workspace `WM_CLASS` to ungroup taskbar buttons and defeat
  `.desktop` icon matching**: breaks launcher pinning and startup
  notification; the tray + urgency hint deliver the signal without
  fighting the WM.
- **Desktop notifications**: heavier interruption than the state
  being conveyed; a finished agent is a status, not an event that
  deserves a popup. Revisitable if the team asks.

## Consequences

On Wayland the window-icon dot is invisible (compositors resolve
icons by `app_id`), but the tray icon and urgency hint are D-Bus /
protocol driven and keep working. GNOME needs the AppIndicator
extension for SNI; the team's DEs (Cinnamon, KDE) support it
natively. The `.deb` gains the appindicator dependency Tauri's
bundler adds automatically when the tray feature is enabled.
