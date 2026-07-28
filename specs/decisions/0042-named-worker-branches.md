# ADR 0042 — `spawn_worker` names the worker's branch

Date: 2026-07-28
Status: accepted; implemented.

## Context

[ADR 0030](0030-orchestrator-sessions.md) gives every coordinator-
spawned worker its own worktree, and worktree branches default to
`moon/agent-<timestamp-hex>` ([ADR 0028](0028-coder-worktree-sessions.md)).
That default is fine for the UI path — the user clicks the worktree
button before anyone has described the work — but a coordinator
running five workers produces five `moon/agent-*` branches, five
`.worktrees/moon-agent-*` directories, and five sessions-list rows
whose branch chips are indistinguishable. The coordinator knows what
each worker is for at spawn time; the user has to guess.

## Decision

`spawn_worker` takes a **required** `name` — a short kebab-case name
for the deliverable — and the worker's fresh branch is `moon/<slug>`.
Because the worktree directory is already derived from the branch
(`.worktrees/<branch-slug>`) and the sessions-list chip already renders
the branch, one argument names all three surfaces with no frontend
change.

The same name is also the worker's **session title**, set at creation
instead of at first message. So the sessions-list row reads
`fix-login-redirect` the moment the worker spawns, matching its branch
chip, rather than a truncated copy of the task prompt that the
[auto-rename](../coder.md#auto-rename) cheap-model call replaces one
turn later. A pre-set title suppresses that call — the coordinator
already named the work, and a second name for the same thing is worse
than one.

- **Required, not optional.** Optional means the model omits it and
  the fleet is anonymous again. A name that slugs to nothing (pure
  punctuation) is an `invalid_args` error rather than a silent
  fallback to `moon/agent-<id>`.
- **Slugged to `[a-z0-9-]`** with alphanumeric ends. That single rule
  subsumes every `git check-ref-format` trap (`..`, trailing `.`,
  `.lock`, spaces, leading `-`) without enumerating them. A leading
  `moon-` is dropped so the namespace doesn't double up, and the slug
  is capped at 40 chars on a word boundary to keep the chip readable.
- **Collisions get a `-2`, `-3`, … suffix**, checked against the
  parent's local branches and leftover worktree directories. Two
  workers named the same thing (a retry, the same fix attempted twice)
  must not fail the spawn.
- **`base_branch` wins.** A worker continuing an existing branch keeps
  that branch's name; `name` is ignored there.
- **The UI path is unchanged**: `coder_new_worktree_session` passes no
  name and keeps `moon/agent-<id>`.

## Alternatives considered

- **Let the coordinator pass a full branch name.** Rejected: it hands
  the model the `moon/` namespace and ref-syntax rules for no gain.
  Slugging a human-readable name is the same UX with none of the
  failure modes.
- **A separate "worker label" field on the session header.** Rejected:
  it adds a persisted field and frontend work to render it, while the
  branch chip and the session title — both already onscreen — carry the
  name for free.
- **Let the auto-rename overwrite the seeded title.** Rejected: the
  row would silently stop matching the branch chip one turn in, and the
  coordinator's chosen name is the one the user was told about.
- **Rename the branch after the first turn** from a cheap-model summary
  (phase 6's W.3.1 / W.4.1 idea, still open for the UI path).
  Rejected here: the coordinator already knows the task before the
  worker's first turn, so waiting buys nothing and a live-branch rename
  is strictly more machinery.

## Related

- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the coordinator / worker model this refines.
- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md)
  — the `moon/agent-<id>` default and the branch-as-deliverable stance.
- [ADR 0037 — cross-project workers](0037-cross-project-workers.md) —
  `spawn_worker`'s other argument (`folder`); naming applies the same
  way in the target project.
