# ADR 0094: Terminal tabs close whenever their shell ends

Supersedes the "auto-close on shell exit; offer respawn on environment
loss" section of [ADR 0050](0050-terminal-persistence-and-restart.md).

## Context

ADR 0050 closed a terminal tab only when its shell provably exited on
its own, and kept every other tab — container stopped, `docker exec`
refused, exit code lost — behind an "environment lost" banner with a
respawn button, so scrollback never vanished on an ambiguous exit. In
practice the dead tabs were clutter: after a container stop or
recreate the strip filled with banners the user closed one by one, and
reopening a terminal is one click.

## Decision

- **A tab closes when its shell ends, whatever the cause.** The same
  for a container stop: when the workspace container reports
  non-running, every container terminal tab closes right away.
- **No close classification.** `terminal:closed` carries only the
  stream id and exit code; the backend no longer probes container
  liveness or sniffs `docker exec` refusals on exit.
- The respawn banner and the frontend restart path are removed. Agent
  terminals keep their backend in-place restart (ADR 0090).

Accepted cost: a terminal killed by an environment change takes its
scrollback and its persisted recipe with it — including on a quick
IDE relaunch where the previous instance's `compose stop` lands after
the new one restored its terminals.

## Rejected alternatives

- **Keep the banner, auto-dismiss it after a delay.** Still clutter for
  a while, and a timer that closes tabs is more surprising than closing
  at once.
- **Close but keep the recipe for the next launch.** A tab that's gone
  but comes back after a restart is its own kind of surprise.
