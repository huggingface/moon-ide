# Test plan 0018: Theme gains a "System" mode; terminals follow the theme

- **Date**: 2026-05-03
- **Phase**: post-Phase 1.5 polish

## What shipped

- Theme mode is now three-way: **System** (follow the OS), **Light**,
  **Dark**. `System` is the new default for fresh installs.
- The status-bar toggle button is replaced with a three-option
  popover picker; the command palette gets explicit
  `Theme: System / Dark / Light` entries.
- Terminal (xterm.js) now tracks the active theme — was stuck
  on its mount-time palette before.
- OS-preference changes propagate live while the user is in
  `System` mode; explicit `Dark` / `Light` overrides that.
- OS preference is resolved on the Rust side via the
  `system_theme` Tauri command. On Linux / BSD it reads the XDG
  Desktop Portal `color-scheme` setting via ashpd; on macOS /
  Windows it forwards the webview's theme. WebKitGTK's `matchMedia`
  and `getCurrentWindow().theme()` both ignore the GTK/GNOME/KDE
  theme and default to light, which was flipping the UI to light
  on startup under a dark desktop.
- Live OS flips: on Linux a desktop-shell tokio task subscribes to
  the portal and re-emits a `system:theme-changed` Tauri event that
  the frontend listens for; macOS / Windows get webview
  `onThemeChanged` directly.

## How to test

Prerequisites: `bun install`, then `bun run dev` (or
`bun run dev:vite` + `bun run dev:tauri` per README). No
container or Slack setup required for this plan.

1. **Fresh install picks up the system preference.**
   1. Remove the existing state file so the default takes
      effect:
      `rm ~/.config/moon-ide/state.json` (Linux) or the
      platform equivalent of
      `<app.path().app_config_dir()>/state.json`.
   2. With your OS set to **dark** mode, launch moon-ide.
      Expected: IDE paints the dark palette. The status-bar
      theme trigger reads `◐ system`, tooltip:
      `Theme: System (currently dark) — click to change`.
   3. Quit. Flip the OS to **light** mode. Relaunch.
      Expected: the IDE opens in the light palette without
      any click on our side. The trigger tooltip now reads
      `(currently light)`.
2. **Popover picker works.**
   1. Click the theme trigger in the status bar. A popover
      opens above the trigger with three rows: System (has
      a muted `dark` / `light` sub-label showing the
      resolved value), Light, Dark. The currently-stored
      choice is highlighted and has `aria-checked="true"`.
   2. Click **Light**. Popover closes; IDE repaints light;
      trigger label now reads `☀ light`; tooltip `Theme:
light — click to change`.
   3. Click **Dark**. IDE repaints dark; trigger reads
      `☾ dark`.
   4. Click **System**. IDE repaints to whatever the OS
      currently prefers; trigger reads `◐ system` again.
   5. Pressing **Escape** with the popover open closes it
      without changing the theme, and restores focus to
      the trigger.
   6. Clicking anywhere outside the popover (e.g. in the
      editor, sidebar) also closes it without changing the
      theme.
3. **Terminal follows the theme.**
   1. Open at least one host terminal (status-bar
      terminal icon → **On host**). Type `ls` so there's
      coloured output visible.
   2. Flip to **Light** via the popover. Expected: the
      terminal background, foreground, cursor, and
      selection colours flip to the light palette in
      lockstep with the editor. ANSI colours in the
      `ls` output re-render against the new background.
   3. Flip back to **Dark**, then to **System**.
      Expected: same lockstep behaviour — no stale dark
      background on a light editor, and vice-versa.
   4. While in **System** mode, flip your OS theme. The
      terminal repaints automatically alongside the rest
      of the IDE.
4. **OS flip propagates in real time.**
   1. Put the IDE in **System** mode.
   2. Run a system command that toggles the OS colour
      scheme (on GNOME:
      `gsettings set org.gnome.desktop.interface
color-scheme prefer-dark` and back to `default` /
      `prefer-light`; macOS: `System Settings → Appearance`).
   3. Expected: the IDE (editor, sidebar, status bar,
      terminal) repaints within a few hundred ms of the
      OS flip. No click on the picker, no relaunch.
   4. Now set the IDE to **Dark** explicitly. Flip the
      OS again. Expected: the IDE _does not_ repaint —
      an explicit choice overrides the system preference.
5. **Command palette entries.**
   1. Open `Ctrl+Shift+P`. Type `theme`. Expected: three
      entries, `Theme: System`, `Theme: Dark`,
      `Theme: Light`. The currently-active one is
      suffixed with `(current)`.
   2. Pick one that isn't current. Expected: the theme
      flips and the `(current)` suffix moves to the new
      pick next time the palette opens.
6. **Persistence and restart.**
   1. Pick **Light** via the popover.
   2. Quit the IDE. Inspect
      `<config_dir>/state.json`: the `theme` field should
      be the string `"light"`.
   3. Relaunch. Expected: opens directly in light mode,
      trigger reads `☀ light`.
   4. Repeat with **System** — on-disk value is
      `"system"` and the next launch re-resolves based on
      the current OS preference.
7. **F6 focus cycle still lands on the status bar.**
   1. Open a folder.
   2. Press `F6` until focus reaches the status bar.
      Expected: a focus ring lands on the theme trigger
      button (the right-most interactive control). Enter
      opens the popover. `Escape` closes it and returns
      focus to the trigger.

## What must keep working

Regression checks. If any of these break, the commit needs a
follow-up.

- `cargo test -p moon-core` passes the `load_default_when_missing`
  and `corrupt_state_falls_back_to_default` tests — both now
  assert `ThemeMode::System`. `save_then_load_roundtrip`
  continues to round-trip `ThemeMode::Light` verbatim.
- Existing syntax-highlight behaviour (per test plan 0014)
  still flips with the resolved theme. Each CodeMirror view
  receives a fresh theme compartment on every
  `effectiveTheme` change.
- Corrupt `state.json` (or a state written by an older
  build where `theme` is one of `"dark" | "light"`) still
  parses and round-trips without crashing — the three
  existing values are still valid and `"dark"` / `"light"`
  continue to be honoured verbatim.
- The scrollbar-corner moon SVG, tab-marker SVG, and other
  data-URL assets that can't read CSS variables behave no
  better (but no worse) than before; this plan doesn't try
  to fix them.
- Focus-region cycling behaviour unchanged aside from the
  focus target on the status region being the new picker
  trigger rather than the old toggle.
- `Ctrl+J` bottom-panel toggle, `Ctrl+\` split, `Ctrl+L`
  chat toggle etc. all unchanged.

## Known limitations

- xterm.js re-applies the theme only on the `$effect`
  firing for that specific `TerminalTab` instance. If a
  tab is hidden (`display: none`) when the theme flips,
  it still re-applies immediately — xterm is happy to
  paint with `display: none` — but if a future lazy-mount
  optimisation gates the whole component on visibility, the
  effect will naturally run when the component mounts.
- System-preference detection relies on the `system_theme`
  Tauri command: ashpd on Linux / BSD, the webview's
  `theme()` on macOS / Windows. In a vite-only dev shell
  (no Tauri runtime) we fall back to `matchMedia`, and beyond
  that to `true` (dark) as a safe default in
  `detectSystemPrefersDark`.
- Per-workspace theme overrides aren't on the table —
  theme is a per-user preference per
  [ADR 0006](../decisions/0006-no-settings-file.md), and
  that hasn't changed.
- The popover glyphs are plain Unicode (`◐ ☀ ☾`). Phase 10
  will bring a real icon set alongside the rest of the
  theming work.

## Related

- Specs: [`specs/frontend.md`](../frontend.md) — Theming
  section updated for the three-mode model.
- ADRs: [`specs/decisions/0006-no-settings-file.md`](../decisions/0006-no-settings-file.md)
  — `AppState.theme` is still the single storage surface;
  no new setting was introduced.
- Prior test plans: 0014 (CodeMirror syntax themes) for the
  editor side of a theme flip; 0016 (terminal basics) for
  the xterm mount the new `$effect` hangs off.
