# ADR 0107: Browser tabs stay in their agent's folder

Amends [ADR 0088](0088-in-ide-browser-tab.md) ("focusing a tab parked
in another folder moves it over").

## Context

An agent's tab is filed under the folder its session runs in. But
`open_browser` reuse, `browser_tab navigate/focus` and reloads all
focus the tab, and a focused tab sitting in a non-active folder was
moved into the active one. If the user had switched to another project
while the agent kept working, the agent's tab landed in the project the
user was looking at. Reuse also matched tabs in any folder, so an agent
could take over another project's tab on the same URL.

## Decision

- Reuse only matches a tab filed under the same folder (and side).
- Focusing a tab filed under a non-active folder selects it in that
  folder's strip; it shows up when the user switches there. Only tabs
  with no folder (opened by the user) still move to the active folder.

## Rejected

- Switching the active project to the focused tab: yanks the user away
  from what they're doing whenever a background agent touches its tab.
