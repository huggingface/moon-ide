# ADR 0022 — Per-session host-mode override for the coder's `bash`

Date: 2026-06-09
Status: accepted; amends [`specs/coder.md`](../coder.md) (the
`bash` tool routing notes) and references the cross-project
networking work in
[`crates/moon-container/src/network.rs`](../../crates/moon-container/src/network.rs).
The turn-start snapshot posture for `resolve_bash_target` is
amended by [ADR 0041](0041-live-host-mode-toggle.md) — the toggle
now applies to the next tool dispatch of an in-flight turn.

## Context

The coder's `bash` tool routes to the workspace shell container
(`docker exec …`) whenever that container is `Running`, and to
the host (`bash -lc …`) otherwise. The decision is made by
`tools::resolve_bash_target`, which probes
`moon_container::Workspace::status()` — the same query terminals
and LSP use, so all three agree. File tools (`read_file` /
`edit_file`) are already host-direct through the container bind
mount and never cross into `docker exec`.

That auto behaviour is right almost always, but it has no escape
hatch, and one real workflow needs one: **diagnosing the host
from inside a containerized workspace.** When something is wrong
at the host-daemon layer — a project compose network that didn't
attach, a stuck container, host port conflicts — an agent whose
`bash` runs _inside_ the dev container can't see it. It can't run
`docker network inspect`, `docker ps -a`, or poke at host
networking, because the dev container has no Docker socket (by
design — ADR 0008's unprivileged threat model). The user has to
drop to a host terminal and do it by hand, which defeats the
point of having an agent.

## Decision

Add a **per-session** override that pins this session's `bash` /
shell tool to the host even while the workspace runs in a
container. Two states only:

- **Auto** (default, `None`): the historical probe-the-container
  behaviour.
- **Force host** (`BashTargetOverride::ForceHost`): short-circuit
  `resolve_bash_target` to host without probing.

### Why per-session, not per-workspace

Host-vs-container is a property of _what this conversation is
doing_, not of the workspace. The workspace as a whole still
wants the container (LSP, terminals, normal dev); only the one
diagnostic session wants the host. Per-session also composes with
concurrent sessions (ADR 0016): one session can debug host Docker
while another keeps doing normal in-container work in the same
folder. The override lives on `SessionHeader` so it persists; a
**fresh session always starts Auto** (force-host is a deliberate,
diagnostic act — silently inheriting it into new sessions is a
footgun), and **sub-agents always run Auto** (a forced parent
doesn't leak its override into delegated work).

This is deliberately _not_ modelled on `coder_provider_lock`
(which is per-workspace) — provider lock is a standing policy;
host-mode is a transient escape hatch.

### Why no `ForceContainer`

"Auto" already prefers the container when it's up. Forcing the
container while it's down only manufactures errors. We ship the
one variant that has a concrete need and add the other if one
ever shows up (scope discipline — hardcode/omit until asked).

### What the override touches — and what it doesn't

It flows into exactly three consumers, all reading one snapshot
taken at turn start (a mid-turn toggle applies to the _next_
turn, matching how model picks are snapshotted):

1. `resolve_bash_target` → the `bash` tool's `docker exec` vs
   `bash -lc` choice.
2. `compose_system_prompt`'s path advertising — host mode
   advertises real host paths, container mode advertises
   `/workspace/<name>`. **This must follow the override** or the
   model hands a host shell container paths that don't exist.
3. `CoderStatus.bash_target` / `force_host_override` — so the
   header pip shows the effective target and an off-default
   badge.

It deliberately does **not** relocate:

- **File tools** — already host-direct via the bind mount; in
  force-host mode the agent reads/writes the same bytes either
  way. The only behavioural change is that the shell moves.
- **Format-on-save** — runs through the global shell resolver on
  `LocalHost::format_file`, which has no per-call override hook.
  Re-plumbing `WorkspaceHost::format_file` to carry a per-session
  flag is a wide signature change for a diagnostic escape hatch,
  and it doesn't matter in practice: the formatter operates on
  the same bind-mounted bytes regardless of where it runs. The
  only edge case is a formatter binary present in the container
  but not on the host (or vice versa); rare enough to defer until
  someone actually hits it.

### Persistence

`SessionHeader` gains `bash_target_override: Option<BashTargetOverride>`,
serialised as the wire string `"host"` when forced and **omitted**
when auto (keeps existing headers byte-clean; unknown/future
values degrade to auto on load rather than erroring). Because the
header is written once on first persist, a toggle on an
already-persisted session rewrites line 1 in place via the new
`sessions::rewrite_header` (no-op for a not-yet-persisted session,
which carries the value in memory until first append).

### UX

The existing read-only bash-target pip in the coder panel header
becomes a button opening a two-row Auto / Force-host popover. The
pip glyph reflects the _effective_ target; an off-default badge
(a small dot + warning tint) marks a force-host session so the
escape hatch being left on is glanceable. Per-session, so the pip
reflects the active folder's _visible_ session.

## Consequences

- One extra container-status probe is already made per turn for
  the system prompt; the override just short-circuits it, so no
  new cost.
- A forced-host session whose workspace later stops the container
  is a no-op change (it was already going host).
- Format-on-save behaviour is unchanged; documented above as a
  known, intentional gap.
- When the final roadmap phase declares a stable surface, the
  header field is part of the persisted schema and gets the
  normal migration treatment; until then it's free to change.
