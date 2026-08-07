# ADR 0061: Always-visible per-workspace tray icon

Partially supersedes [ADR 0058](0058-agent-activity-indicator.md):
the tray icon is now permanent instead of transient. Everything
else in 0058 (window-icon dot, urgency hint, running-turns count,
what is / isn't counted) stands.

## Context

ADR 0058 made the tray icon transient — present only while agents
run or after they finish unfocused — on the theory that N
workspaces of permanent icons would be clutter conveying "nothing
happening". In practice the team wants the opposite: a stable
per-workspace presence in the tray, requested explicitly after
living with the transient version.

## Decision

Each workspace process shows its tray icon (StatusNotifierItem)
for its whole lifetime, painted with the workspace badge. Agent
activity is conveyed by the same amber (running) / green
(finished-unseen) dot as the window icon; idle shows the plain
badge. Focusing the window still clears the green dot — it no
longer removes the icon. Tooltip is `moon-ide — <workspace>`,
suffixed with `: agent running` / `: agents finished` when a dot
is up. The "Focus window" menu item (and left-click where the
platform delivers it) now doubles as a per-workspace "bring me
back" affordance, which is most of the point of keeping the icon
around. The menu also carries "Close window" — the same exit path
as closing the window (one window per process, so it exits the
process and runs the `stop_all` teardown).

## Rejected alternatives

- **Keep the transient icon** (ADR 0058's choice): the
  appear/disappear churn was less legible than a fixed badge whose
  dot changes, and idle workspaces had no tray affordance to
  re-focus them.
- **One shared icon for all workspaces with a workspace submenu**:
  a single process would have to own it and hand it off on exit;
  per-process icons are trivially correct under
  process-per-workspace (ADR 0014).
