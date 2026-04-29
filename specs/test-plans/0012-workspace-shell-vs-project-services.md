# Test plan 0012 — workspace shell vs project services

Manual end-to-end checks for the Phase 2.0.6 split between
the workspace's `dev` container (managed by the IDE, status
in the bottom-bar pip) and per-folder compose projects
(managed by the user from each folder bar).

Architecture context:

- ADR 0007's
  [workspace-shell-vs-project-services amendment](../decisions/0007-compose-and-moon-base.md#amendment-2026-04-29--workspace-shell-vs-project-services).
- [`containers.md` § Workspace shell vs project services](../containers.md#workspace-shell-vs-project-services).
- Phase 2 roadmap section
  [2.0.6 — workspace shell vs project services](../roadmaps/phase-02-containers.md#206--workspace-shell-vs-project-services-shipped).

The previous status-aggregation test plan
[`0011-container-state-dir.md`](0011-container-state-dir.md)
still applies for the workspace shell's lifecycle; this one
covers everything specific to the per-folder addition.

## Setup

1. Daemon ready: `docker info` succeeds.
2. Built `moon-base:dev` locally (or available in the
   workspace `compose.yaml` reference).
3. At least one bound folder with a real
   `docker-compose.yml` at its root (moon-landing is the
   canonical reference; any compose-using project works).

If you're upgrading from a previous version that ran a
unified `moon-ws-default` with included project services:

```sh
# One-time cleanup of the orphaned services from the old
# include:-based layout. Re-run with --remove-orphans on a
# stopped project so compose drops everything not declared
# in the new dev-only compose.yaml.
docker compose -p moon-ws-default down --remove-orphans
```

After this the daemon should show no `moon-ws-default`
containers; `docker compose ls` is empty until the user hits
"Set up" again.

## A. Workspace shell stays decoupled from project health

A.1. Open a folder with a known-broken compose file (e.g.
moon-landing where `gitaly` exits 1 on volume permission, or
a temp folder with `services: [oops: { image: bogus:nope }]`).

A.2. Hit "Set up" on the bottom-bar pip. Expectation:

- The workspace `compose.yaml` is dev-only (one bind mount
  per bound folder, no `include:`).
- `docker compose ls` shows `moon-ws-default` running with
  one container (`moon-ws-default-dev-1`) — no project
  services.
- The status pip flips to `running` within seconds. No
  `setting up…` hang.

A.3. With the workspace shell up, terminal availability
isn't blocked by anything project-side. (Phase 2.1 will
make this terminal-real; for now check it via
`docker exec moon-ws-default-dev-1 echo ok`.)

## B. Per-folder indicator surfaces

B.1. Bind a folder that has a root `docker-compose.yml`.
Expectation:

- The folder bar shows a small dot indicator to the right
  of the name. Hover tooltip says `Services: not running`
  (compose file present, project never brought up).
- Bind a folder without a root compose (or with only
  sub-directory composes). Its bar's indicator slot stays
  empty.

B.2. Click the indicator. Expectation:

- A popover anchored to the indicator opens.
- "Compose file" shows the absolute path.
- "Project name" shows `moon-ws-default-<slug>`.
- The action area offers "Start services".

## C. Per-folder lifecycle

C.1. From the popover hit "Start services". Expectation:

- Button label flips to "Starting…"; the indicator dot
  pulses.
- `docker compose ls` adds a new
  `moon-ws-default-<slug>` project alongside the workspace
  shell.
- On success, the dot turns green, popover state line says
  `running`, the service list lists each declared service
  with its raw state and (where applicable) exit code +
  health.

C.2. With services up, hit "Pause". Indicator turns warning-
coloured (yellow), popover offers Resume + Stop services.
`docker compose ls` shows `paused(N)`.

C.3. Hit "Resume". Back to running.

C.4. Hit "Rebuild". `docker compose up -d --force-recreate
--pull always --wait` runs; on completion the dot is back
to green and the service list reflects the recreated
containers.

C.5. Hit "Stop services". `docker compose down` runs;
indicator goes muted, popover state line says `not
running`, service list is empty.

## D. Failure surfacing

D.1. Make a project service exit with a non-zero code (e.g.
gitaly's permission-denied). Bring services up:

- The dot turns red.
- The popover state line says `failed`.
- The service list shows `gitaly · exited (1)`. Other
  services that came up fine show `running` (muted) or
  `running · healthy` (green) as applicable.
- The action area offers Rebuild + Tear down.

D.2. Tear down → re-launch with the issue still present:

- Same `failed` end state.
- The workspace shell's bottom-bar pip is **unaffected**
  — it stays `running` even with the per-folder failure.

## E. Multi-folder independence

E.1. Bind two folders, each with their own compose. Start
folder A's services. Expectation:

- Folder A's indicator dot turns green.
- Folder B's indicator stays muted (services not running).
- `docker compose ls` shows three projects:
  `moon-ws-default`, `moon-ws-default-<slug-a>`,
  `moon-ws-default-<slug-b>` (the last only if it was
  brought up too).

E.2. Open folder A's popover, then click folder B's
indicator. Folder A's popover closes (single-panel UX);
folder B's opens.

E.3. Hit "Stop services" on folder A. Folder B is
unaffected.

## F. Folder-bar workflow corner cases

F.1. **Remove a folder while its services are running**.
Click `×` on the bar, confirm the dialog. Expectation:

- The folder leaves the workspace.
- The folder's compose project is **not** torn down on
  the daemon — `docker compose ls` still shows
  `moon-ws-default-<slug>` running.
- Re-binding the same folder shows the indicator restored
  (refresh polls hit the existing project).
- Documented behaviour: removing a folder is a sidebar
  declutter, not a destructive op against running services.

F.2. **Switch active folder while another's services are
in flight**. Click "Start services" on folder A's popover,
then click folder B's bar to make it active before A's
`up -d --wait` finishes. Expectation:

- Folder A's startup keeps running in the background.
- The IDE's tabs / file tree swap to folder B.
- When folder A's startup completes, the
  `project_compose:state` event lands and folder A's
  indicator updates without needing it to be active.

F.3. **Add a folder while the workspace shell is running**.
The shell's `compose.yaml` regenerates with the new bind
mount; `docker compose up -d --wait` recreates `dev`
without touching any per-folder project. The new bar's
indicator paints with `not running` (or `absent` if it has
no compose file) on the next refresh.

## G. Migration from a unified project

G.1. Pre-upgrade, the user had `moon-ws-default` running
with `dev` + ten included services from moon-landing.

G.2. After upgrade + first IDE relaunch:

- The workspace's `compose.yaml` regenerates as dev-only.
- `docker compose ls` still shows `moon-ws-default` with
  eleven containers (the rewritten compose declares only
  `dev`, but the existing containers are orphaned, not
  removed).

G.3. User runs the documented one-time:

```sh
docker compose -p moon-ws-default down --remove-orphans
```

The orphaned services drop. `moon-ws-default` is now
`Stopped` (no `dev` either, since it was part of the same
project recreation). The user clicks "Set up" again; the
IDE brings up `moon-ws-default` with just `dev` and the
folder bar's indicator now drives moon-landing's services
independently.

G.4. On the moon-landing folder bar the user clicks "Start
services" → moon-landing's ten services come up under
`moon-ws-default-moon-landing`. From this point onwards
each lifecycle is independent.

## Known limits (deliberately out of scope)

- Sub-directory `docker-compose.yml`s aren't picked up by
  the per-folder UX. The user can bind the sub-directory
  as its own workspace folder if they want to manage it.
- Two folders whose basenames slug to the same value
  (e.g. `code/My Stuff` and `archive/my_stuff`) would
  collide on `moon-ws-default-my-stuff` and step on each
  other on the daemon. The frontend doesn't refuse the
  add today; pick differently-named folders or rename
  one. The registry will refuse it explicitly when this
  surfaces in practice.
- No periodic poller of per-folder status. If the user
  runs `docker compose stop` from a terminal, the
  indicator stays stale until the next click on the
  folder bar refreshes it. Phase 2.2's docker events
  watcher closes this loop.
- Networking between workspace shell and per-folder
  services. Documented as "isolated by default; use
  host ports for cross-talk". Phase 2.2 formalises
  routing.

## Pass criteria

- A.2 / A.3 confirm the workspace shell starts in seconds
  and is decoupled from project failure.
- B and C confirm the per-folder UX (indicator → popover →
  start/stop/etc.) works end-to-end on a healthy project.
- D confirms unhealthy projects fail loudly without
  poisoning the workspace shell.
- E confirms two project compose lifecycles are
  independent.
- F covers the workflow edges (remove during run, switch
  during in-flight, add while running).
- G confirms the migration story is documented and works.
