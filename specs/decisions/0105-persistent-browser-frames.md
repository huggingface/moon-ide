# ADR 0105: Browser-tab frames persist across tab and folder switches

Amends [ADR 0088](0088-in-ide-browser-tab.md) (known limit: "the
iframe reloads when its tab is re-shown").

## Context

A browser tab's iframe lived inside its editor view, so it was torn
down whenever the tab was hidden. The page reloaded on every switch
(losing state an agent had clicked into), and every `browser_page`
action on a hidden tab had to focus it first — pulling the user off
whatever they were doing while the agent worked.

## Decision

- Iframes live in one app-level layer (`BrowserFrames`), one per tab
  in the registry, for the tab's lifetime — whichever folder or pane
  the tab belongs to. The editor view only renders the URL bar and a
  slot; while a view shows the tab, its frame is positioned over the
  slot (`position: fixed`, following the slot's rect every animation
  frame, z-index just above the editor's plain content and below
  every overlay).
- A hidden frame stays loaded with `visibility: hidden`, at the size
  it was last shown (1280×800 before that), so the page keeps its
  state and layout.
- Page actions no longer focus the tab. A screenshot of a hidden tab
  raises its frame on top of everything for the capture (the platform
  snapshot only sees painted pixels) and drops it back immediately:
  a brief flash instead of a tab switch.

## Rejected alternatives

- **Keep the frame in the view, hidden with CSS** — the view itself is
  unmounted on tab and folder switches, taking the iframe with it.
- **Offscreen positioning instead of a flash** — the snapshot covers
  only the visible area.

## Known limits

- Every open browser tab keeps its page in memory.
- A visible frame swallows pointer events during drags that cross it
  (split / sidebar resizes), as before.
