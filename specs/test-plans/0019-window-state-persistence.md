# Test plan 0019: Window state persistence

- **Date**: 2026-05-03
- **Phase**: post-Phase 1 polish

## What shipped

- Window size, position, maximized, and fullscreen state survive
  quit / relaunch via `tauri-plugin-window-state`. No code on our
  side beyond registering the plugin.
- Persistence file is plugin-owned, written alongside
  `state.json`; `AppState` still owns everything else.

## How to test

Prerequisites: `bun install`, then `bun run dev`.

1. **Maximize survives relaunch.**
   1. Launch moon-ide. Maximize the window (WM shortcut or
      title-bar button). Quit.
   2. Relaunch. Expected: window comes back maximized.
2. **Size + position survive relaunch.**
   1. Unmaximize. Drag the window to a noticeable non-default
      position and resize to an obviously non-default size
      (e.g. ~1000x600 in the top-left corner).
   2. Quit and relaunch. Expected: window comes up at roughly
      the same position and size (within a few pixels; WM
      policy may snap).
3. **Fullscreen survives relaunch.**
   1. Toggle fullscreen (platform shortcut, e.g. F11 on Linux,
      Cmd+Ctrl+F on macOS). Quit.
   2. Relaunch. Expected: window comes up fullscreen.
4. **Fresh install uses `tauri.conf.json` defaults.**
   1. Delete the plugin's persistence file (location varies by
      platform; on Linux it's under
      `~/.local/share/moon-ide/`). Keep `state.json`.
   2. Launch. Expected: default 1280x800 window per
      `src-tauri/tauri.conf.json`, not maximized.

## What must keep working

- `AppState` round-trip: `state.json` still holds `last_session`,
  `theme`, `bottom_panel`, `slack` — the plugin file is a
  separate concern.
- Theme System / Dark / Light still restores as before; window
  state persistence is orthogonal.
- Min-size (`800x500`) from `tauri.conf.json` is still enforced —
  the plugin shouldn't be able to restore a window smaller than
  that after a config change.

## Known limitations

- Multi-monitor edge case: if the saved position is on a monitor
  that no longer exists at launch, the plugin falls back to its
  default placement. We don't intervene.
- The persistence file is not part of `AppState` and is therefore
  not covered by the "wipe `state.json` to reset" flow. Wiping it
  separately resets only window state.

## Related

- Specs: [`specs/frontend.md`](../frontend.md) — Window state
  section added.
- Prior test plans: [`0018-theme-system-mode.md`](0018-theme-system-mode.md)
  — closest precedent for UI state that survives relaunch.
