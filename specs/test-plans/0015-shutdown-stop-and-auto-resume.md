# Test plan 0015 — shutdown stop + auto-resume

Manual end-to-end checks for the IDE-as-command-centre
shutdown / startup sequence:

- On quit, moon-ide hides the window then `docker compose
stop`s the workspace shell **and** every bound-folder
  compose project before exiting.
- On next launch, the workspace shell auto-resumes (`setup`)
  if it was left in `stopped`. Per-folder projects stay
  stopped — the user starts them individually.

Architecture context:

- [`containers.md` § Why moon-ide owns shutdown](../containers.md#why-moon-ide-owns-shutdown).
- Implementation: `src-tauri/src/shutdown.rs`,
  `src-tauri/src/lib.rs` (the `RunEvent::ExitRequested`
  branch and the post-`setup` `auto_resume_shell` spawn).

## Setup

1. Daemon ready: `docker info` succeeds.
2. moon-ide built and runnable (`bun run dev` or a release
   build).
3. At least one bound folder with a real `docker-compose.yml`
   at its root. `moon-landing` is the canonical reference.
4. Bring the workspace shell up via the status-bar pip
   ("Start"). Open the folder bar's popover and Start the
   per-folder services too. `docker compose ls` should show
   both:

   ```
   moon-ws-default              running(1)
   moon-ws-default-moon-landing running(N)
   ```

## A. Quit while everything is up

A.1. With both projects running, click the close button on
the moon-ide window (Linux: window manager X, macOS: red
traffic light **then** Cmd+Q if window-close doesn't quit).

A.2. Expectation:

- The window disappears within ~100 ms (it's hidden, not
  destroyed).
- For the next ~10–30 s, `docker compose ls` still shows
  the projects, transitioning their containers to `exited`.
- moon-ide's process exits cleanly. No zombie tauri
  processes (`pgrep -af moon` returns nothing).

A.3. After exit, `docker compose ls -a` (note the `-a`)
should show both projects still **listed** but with all
containers in `exited(N)` / `exited(0)` states.

A.4. `docker ps` (running only) shows nothing
moon-ws-default-prefixed.

## B. Auto-resume on next launch

B.1. Re-launch moon-ide.

B.2. Expectation:

- The status-bar pip starts at `setting up…` (or `stopped`
  briefly) and flips to `running` within seconds — without
  the user clicking anything. Logs (`tail -f
~/.local/share/moon-ide/.../*.log` or the dev-server
  console) include `auto_resume_shell: previous session
left the workspace shell stopped, resuming`.
- The folder bar popover for the bound folder shows the
  per-folder project as **stopped** (containers exist,
  state aggregates to `Stopped`). It does **not**
  auto-start.
- `docker ps` shows `moon-ws-default-dev-1` running, no
  per-folder service containers.

B.3. Click "Start" on the folder bar popover. Containers
come up as before. Workflow restored.

## C. Quit with only the workspace shell up

C.1. Tear down the per-folder project ("Down" in the
folder bar popover) so only the workspace shell is running.

C.2. Quit moon-ide. Expectation:

- Window hides, exit happens within a few seconds (only one
  project to stop).
- `docker compose ls -a` shows `moon-ws-default` exited.
- No warning lines in the log about the per-folder project
  — `stop_all` resolves it as `Ok(None)` (no compose file
  registered) and short-circuits cleanly.

## D. Quit with workspace shell `Down`ed

D.1. Click "Down" on the workspace shell pip popover so the
container itself is removed from the daemon.

D.2. Quit moon-ide.

D.3. Expectation:

- Exit is essentially instant — there's nothing to stop.
- The shutdown log includes a warn from
  `stop_all: workspace shell stop failed` (compose stop
  against a non-existent project errors). That's expected
  and harmless; the IDE still exits cleanly.
- On next launch, the pip is `not set up` (absent). It does
  **not** auto-start — `auto_resume_shell` only resumes
  from `Stopped`, not `Absent`.

## E. SIGKILL escape hatch

E.1. With everything running, kill moon-ide hard:

```sh
pkill -9 -f moon-ide
```

E.2. Expectation:

- `docker ps` shows containers still running. The graceful
  hook never fired — that's the deliberate "best effort"
  fallback.

E.3. Re-launch moon-ide. Expectation:

- The pip notices the container is **already running**
  (`auto_resume_shell` short-circuits because state is not
  `Stopped`); the IDE picks up where it was, no recreate.
- Per-folder popover similarly shows the project as
  `running`.

## F. Failure modes during shutdown

F.1. With everything running, `docker stop -s SIGSTOP $(docker
ps -q --filter "name=^moon-ws-default-")` to freeze
containers (simulate an unresponsive daemon).

F.2. Quit moon-ide. Expectation:

- The window hides immediately.
- The shutdown task waits up to compose's default 10s SIGTERM
  grace per container, then force-kills. The IDE still exits
  (likely after ~30–60s for a few-container project).
- Log shows a warn from `stop_all` per project that timed
  out, but no panic.

F.3. `docker unpause` the containers and re-launch. They
should be in `exited(137)` (SIGKILL); auto-resume sets up
fresh containers because state aggregates to `Stopped` once
they're not running.

## G. Signal-termination exits don't poison the state

G.1. With everything running, click "Stop" on the per-folder
popover (e.g. moon-landing, which has JVM services like
`dynamo`, `mongo`, `meilisearch`).

G.2. Wait for the `compose stop` to complete. Expectation:

- The project-level state in the popover header reads
  **stopped**, not failed.
- Per-service rows for the JVMs show `exited (143)` (or
  `exited (137)` if SIGKILL escalation kicked in) but
  the dots are **muted** (the same shade as the
  exited-clean rows like `cas-deps` / `gitaly-init`),
  **not red**. The exit code text remains visible for
  diagnostic transparency.
- The folder bar pip is grey/muted, not red.

G.3. Click "Stop" on the workspace shell pip popover.
Expectation:

- Same behaviour: `dev` shows `exited (143)` but the
  pip is grey/stopped, not red.

G.4. Quit moon-ide. Re-launch. Expectation:

- The workspace shell auto-resumes (covered in B). The
  signal-exit treatment is what unblocks this — without
  it, the `Failed` state would prevent auto-resume.

G.5. Real-failure regression: with the workspace shell
running, manually `docker compose -p moon-ws-default kill
--signal SIGSEGV dev` (or any non-stop signal). Expectation:

- The pip flips to **failed** (red) — SIGSEGV is exit
  `139`, which is _not_ on the stop-signal list and
  must still surface as a real failure.
- Recover by clicking "Recreate" or "Down" → "Start".

## What must keep working

- The status pip and per-folder popovers continue to reflect
  reality on next launch — auto-resume doesn't break the
  status-poll feedback loop.
- `compose_logs:line` streams from the previous session don't
  resurrect — that's covered by `kill_on_drop(true)` in
  `commands::compose_logs` and is unchanged.
- Per-folder compose projects with no compose file at the
  bound folder's root are silently skipped during shutdown
  (no spurious "stop failed" warns).
- The Slack panel, persisted app state, last-session
  workspace and tabs all reload exactly as they did before
  this change. The shutdown hook only touches Docker.
