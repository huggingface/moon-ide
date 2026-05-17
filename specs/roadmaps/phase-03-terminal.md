# Phase 3 — Terminal

The IDE gets PTY-backed terminals. Architecture lives in
[ADR 0009](../decisions/0009-terminal-pty-and-targets.md);
this file owns the work breakdown and sub-phase acceptance.

The roadmap line is one sentence:

> xterm.js + portable-pty terminals, multiple sessions,
> splits. Spawned via active host so they run inside the
> container when remote.

We're refining "active host" into an explicit two-target
model — **host** (the user's machine) and **container**
(the workspace shell, `moon-ws-<id>-dev-1`) — chosen at
open-time per terminal. Terminals don't migrate between
hosts; the target is part of the tab's identity.

## Sub-phases

### 3.0 — Host vs container terminals in the bottom panel

**Acceptance**: opening the bottom panel exposes a "+ Terminal"
button. Clicking it brings up a small popover with two
options — "On host" / "In container". The status bar gets
a matching terminal icon that opens the same popover.

- Picking "On host" spawns the user's `$SHELL`
  (fallback `/bin/bash`) on the host with cwd = active
  folder's absolute path (or `~` if no folder is bound).
- Picking "In container" runs `docker exec -it
moon-ws-<id>-dev-1 bash` with cwd = `/workspace/<basename>`
  for the active folder (or `/workspace` if no folder).
  The "In container" button is disabled with copy
  "Workspace container is not running" when the workspace
  shell isn't `running`.
- Each terminal opens as a new tab in the bottom panel
  with a chip in the title showing `host` or `container`.
  Container chips use the workspace pip's accent colour
  for at-a-glance differentiation.
- Multiple terminals can run side-by-side. Tab close
  kills the PTY (and the underlying `docker exec` for
  container terminals). The shell exiting flips the tab
  body to `[exited (N)]` and disables input — the user
  closes the tab to remove it.
- Resize follows panel resize and tab activation;
  xterm-addon-fit recomputes cols/rows and pushes them
  back through `terminal_resize`.
- Keyboard: standard xterm.js keybindings, `Ctrl+C` /
  `Ctrl+D` go to the shell as expected. Copy/paste
  uses `Ctrl+Shift+C` / `Ctrl+Shift+V` (xterm default).
- File-link navigation: stack-trace-shaped paths in the
  output (`file:///abs/path:line:col`, bare absolute
  `/abs/path:line:col`, container `/workspace/<basename>/...`)
  underline on hover and open in the editor on
  Ctrl/Cmd-click — same modifier as the editor's
  goto-definition. Container paths reverse-map through the
  bound-folder list by basename, mirroring the forward
  `containerCwdFor` rule the terminal opens with. Bare
  clicks stay inert so drag-selection across a path keeps
  working. Relative paths are out of scope (no shell
  integration to follow `cd`); the path either has to be
  absolute or carry the `/workspace/...` prefix. See
  [`src/lib/terminalLinks.ts`](../../src/lib/terminalLinks.ts)
  for the matcher.

What ships:

- New crate `crates/moon-terminal/`: `TerminalTarget`
  enum, `pty.rs` wrapper around
  [`portable-pty`](https://docs.rs/portable-pty), spawn
  helper that returns a tokio-friendly read/write/resize
  handle.
- Tauri commands in `src-tauri/src/commands/terminal.rs`:
  `terminal_open`, `terminal_write`, `terminal_resize`,
  `terminal_close`. Mirrors the `compose_logs` shape —
  one supervisor task per stream id, registry of
  `AbortHandle`s in `AppState`, child spawned with
  `kill_on_drop(true)` so the abort frees the PTY
  immediately.
- Two Tauri events: `terminal:output` (stream id +
  base64 bytes) and `terminal:closed` (stream id +
  exit code).
- Frontend: `xterm`, `@xterm/addon-fit`,
  `@xterm/addon-web-links` deps; `terminal.svelte.ts`
  store; `TerminalTab.svelte` body; new
  `TerminalTab` kind in `BottomPanelTab`; `+ Terminal`
  popover wired into the panel strip and the status
  bar.

What doesn't ship in 3.0 (and when to revisit):

- **Terminal persistence across IDE restart.** Reopening
  shells with stale state would surprise the user; if
  someone asks for it specifically, we can persist tab
  metadata (title, target, cwd) and re-spawn fresh
  shells on next launch. (Post-3.0 polish: when launch
  finds the bottom panel visible-but-empty, we now
  auto-spawn one default terminal — container if the
  workspace shell is up, host otherwise — to avoid the
  "empty strip" UX. That's a default, not persistence,
  and it sidesteps the surprise risk above. See
  [test plan 0026](../test-plans/0026-bottom-panel-auto-terminal.md).)
- **Splits.** xterm.js doesn't have a built-in pane
  manager; we'd need to layer one. Defer until someone
  wants two terminals visible at once badly enough to
  flip from tabs.
- **Per-folder "Open terminal here" from the folder
  bar.** Probably the next thing to add, behind a small
  context-menu affordance. Defer until we have a
  multi-folder workflow that asks for it.
- **Search in scrollback.** xterm-addon-search is a
  one-line add when someone needs it.
- **Custom shell selection.** Hardcoded `$SHELL` /
  `bash`. Flip to a settings entry the moment a second
  shell is wanted (per ADR 0006: hardcode first).
- **A "Terminal" tab kind that's not in the bottom
  panel.** Floating / sidebar terminals are explicitly
  out — the bottom panel is the home.
- **Local terminals in non-bound folders.** A host
  terminal opened with no active folder lands in `~`;
  we don't try to be cleverer.

## Completion checklist

Stop and wait for human review before starting Phase 3.1.

- [ ] `cargo test --workspace` and `bun run check` /
      `bun run lint` clean.
- [ ] Test plan
      [`0016-terminal-basics.md`](../test-plans/0016-terminal-basics.md)
      walked end-to-end on Linux.
- [ ] Quitting the IDE with N terminals open kills all
      N PTYs cleanly (no orphaned `docker exec` or shell
      processes — verified with `pgrep -af`).
- [ ] Container terminal opens behave correctly across
      the full workspace shell lifecycle: stopped →
      auto-resume → running, manual Stop → terminal dies
      with exit code, Recreate → terminal dies and a
      fresh one opens cleanly.

## Open questions for later sub-phases

- Should `+ Terminal` remember the last-picked target
  per-folder so the popover defaults to "container" if
  that's what the user usually picks for moon-landing?
  Probably yes once we have the data; not now.
- When (not if) we add splits, do they live inside one
  bottom-panel tab (xterm panes) or as siblings in the
  panel strip (multi-tab side-by-side rendering)? The
  latter generalises better to "compose logs next to a
  shell" but requires layout work the panel doesn't have
  yet.
