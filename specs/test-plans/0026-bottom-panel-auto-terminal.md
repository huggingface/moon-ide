# Test plan 0026: Bottom panel auto-spawns a terminal on launch

- **Date**: 2026-05-04
- **Phase**: post-Phase 3.0 polish

## What shipped

- When the IDE launches with the bottom panel restored as visible
  but no tabs in it (tab contents aren't persisted by design — see
  ADR 0009), `WorkspaceState.restoreAppState` now auto-spawns one
  terminal so the panel comes back with a sensible default body
  instead of an empty strip.
- Container terminal when the workspace shell reports `running`
  for the active workspace; host terminal otherwise (container
  absent, paused, stopped, failed, or no compose project at all).
- Auto-spawn is gated on a workspace being open (folder bound).
  No workspace → no terminal: a `$HOME`-rooted host shell with no
  folder context isn't useful.
- Auto-spawn waits for `container.refresh()` and the
  `terminal:output` listener bind to settle before deciding the
  target and opening, so the choice reflects daemon truth and the
  first prompt bytes aren't dropped.

## How to test

Prerequisites: `bun install`, host docker daemon ready, a fresh
`moon-base:dev` build, and a workspace with at least one bound
folder. Drive the IDE with `bun run dev`.

1. **Container running → container terminal.**
   1. Open the workspace; make sure the status-bar pip is green
      (workspace shell running). Open the bottom panel via
      `Ctrl+J` so it sticks visible. Quit the IDE.
   2. Relaunch. Expected:
      - Bottom panel comes back visible at its previous height.
      - Within ~1s of the editor area painting, exactly one tab
        appears in the strip with the container chip (accent
        colour) and a title equal to the active folder's
        basename.
      - The shell prompt lands inside `/workspace/<basename>`;
        `whoami` reports the in-container user.
      - Editor focus is **not** stolen — typing immediately
        enters the active editor buffer, not the terminal.
2. **Container down → host terminal.**
   1. From the status-bar pip, Stop the workspace shell. Verify
      the pip flips out of `running`. Bottom panel still
      visible. Quit.
   2. Relaunch. Expected:
      - Bottom panel comes back visible.
      - One tab appears with the host chip (monitor icon) and
        the active folder's basename. Hover shows
        `host: <absolute-path>`.
      - `pwd` returns the active folder's host path.
3. **No compose project → host terminal.**
   1. Bind a folder that has no `compose.yaml` (e.g. a fresh
      throwaway directory). Open the bottom panel. Quit.
   2. Relaunch. Expected: one host terminal, same as step 2.
4. **Panel hidden → no auto-spawn.**
   1. Toggle the bottom panel hidden via `Ctrl+J`. Quit.
   2. Relaunch. Expected: panel stays hidden, no terminal in the
      store. Toggling the panel back on later shows the existing
      empty-state copy ("No tabs open. Click + Terminal …") —
      auto-spawn only fires at launch, not on every show.
5. **No workspace → no auto-spawn.**
   1. Remove every folder from the workspace via the folder bar
      so the welcome screen shows. Toggle the bottom panel
      visible (it should still render alongside the welcome
      screen). Quit.
   2. Relaunch. Expected: welcome screen + visible bottom panel
      with the empty-state copy. No host or container terminal
      gets opened automatically.
6. **Panel visible with a manually-opened terminal → no double-spawn.**
   1. Launch with the panel visible (case 1 or 2 leaves the IDE
      in this state). Verify exactly one tab opened.
   2. Quit and relaunch. Expected: one tab, not two — auto-spawn
      checks `tabs.length === 0` both before and after waiting
      for the container probe, so a user opening a terminal
      themselves between hydrate and the await resolving doesn't
      cause a duplicate.
7. **Auto-spawned terminal behaves like a manual one.**
   1. After step 1's container terminal lands, use it: run a
      command, resize the panel, switch tabs (open a second
      terminal via `+ Terminal`, click between them). Expected:
      no behavioural difference from a tab opened by clicking
      the launcher — same chip, same exit-code suffix, same
      close-tab UX.

## What must keep working

- `Ctrl+J` toggles the bottom panel without dropping terminal
  state (existing behaviour from test plan 0016).
- Persistence file (`state.json`) still round-trips
  `bottom_panel.{visible,height}` — visibility and height are
  the only persisted bottom-panel state.
- Compose log tabs (`LogTab.svelte`) still receive lines on
  manual open. Auto-spawn deals only with terminals; it never
  starts a log stream.
- Editor focus on launch still lands in the active editor tab
  (not the auto-spawned terminal), so `Ctrl+S` / typing land
  where the user expects.
- Container status pip still updates from `container:state`
  events — capturing the `container.refresh()` promise didn't
  change the wiring of subsequent updates.

## Known limitations

- Toggling the panel **hidden → visible** mid-session does not
  auto-spawn a terminal. The launch-time check is one-shot. If a
  user reports wanting parity here, lift the call out of
  `restoreAppState` into `bottomPanel.show()`.
- A `paused` container falls back to a host terminal rather than
  resuming the container. Resuming on the user's behalf would be
  a side-effect we don't take without an explicit gesture.
- Same auto-spawn fires when the user closed every tab themselves
  before quitting (panel stayed visible, tabs went empty). That's
  arguably "the user explicitly emptied the panel"; we treat the
  visible-but-empty state uniformly because there's no on-disk
  signal distinguishing the two cases. Reconsider only if it
  actually annoys someone.

## Related

- ADRs: [`0009-terminal-pty-and-targets.md`](../decisions/0009-terminal-pty-and-targets.md)
  — host vs container target model, no-persistence rationale.
- Specs: [`roadmaps/phase-03-terminal.md`](../roadmaps/phase-03-terminal.md)
  — Phase 3.0 acceptance for the launcher this auto-spawn reuses.
- Prior test plans:
  [`0016-terminal-basics.md`](0016-terminal-basics.md) — manual
  terminal lifecycle that this plan piggybacks on for "behaves
  like a manual one".
