# ADR 0114: Sessions live in the sidebar; files move to an optional right panel

Amends [ADR 0111](0111-coder-in-the-middle.md) (the coder panel's
persisted visibility, and what the sidebar holds).

## Context

After [ADR 0111](0111-coder-in-the-middle.md) the coder panel sat in
the middle but still swapped between two views: the sessions list and
one session's transcript. Opening a session hid the list, and picking
another meant going back first. With several agents running, the list
is the thing to keep an eye on; the file tree, which held the
sidebar's largest area, is consulted far less now that the coder and
`Ctrl+P` do most of the navigating.

## Decision

- **Sidebar**, top to bottom: the Inbox button with its counts
  ([ADR 0112](0112-github-inbox.md)), the folder bars, source control,
  and the sessions list of the active folder's project. The list is
  always visible; clicking a row opens that session in the coder panel
  and marks the row current.
- **Dismiss.** A session whose last turn failed, was interrupted, or
  finished unseen can be dismissed from "Needs you" (per row, or all
  at once) without deleting it. It moves to "Earlier" until it has
  new activity. A session waiting on an answer cannot be dismissed:
  the agent is blocked either way. Dismissals are per workspace
  (`WorkspaceSession.dismissed_coder_sessions`) and change nothing
  about the session itself.
- **Coder panel**: one session's transcript and composer, nothing
  else. With no session open it shows an empty state with "New
  session". It is open at every launch; `Ctrl+L` hides it for the run.
- **Files panel**: the file tree, in an optional panel at the far
  right, closed by default (`AppState.files_panel_visible`). `Ctrl+0`
  opens it and focuses the tree; so do the status bar's `files` entry
  and turning on source control's changes-only filter, whose result is
  that tree.
- The window's columns are separate rounded panels on a darker
  canvas; the gaps between them are the resize handles. The status bar
  sits on the canvas and no longer carries the workspace-coloured
  stripe: the canvas itself, the active folder and the active tab carry
  the workspace's hue ([ADR 0047](0047-workspace-chrome-tint.md)).

## Rejected alternatives

- **Keep the list inside the coder panel as a left rail.** Two columns
  inside a panel that is already one of three; the sidebar has the room.
- **A Files / Sessions switch in the sidebar.** One of them is hidden
  at any time, which is the complaint this fixes.
- **Persist the coder panel's visibility.** A panel that is the main
  surface should not stay hidden because it was hidden last week.
- **Sessions of every folder in one list.** A separate question, and
  the per-project list is what the rest of the coder assumes; the
  folder bars already show which projects have an agent that needs
  attention.

## Known limit

With no folder bound there is no sessions list, so the folder-less
scratch sessions can be created but not browsed.
