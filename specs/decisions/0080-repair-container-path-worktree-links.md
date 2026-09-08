# ADR 0080 — Repair container-path worktree links on adoption

Date: 2026-09-07
Status: accepted; implemented. Extends
[ADR 0079](0079-worktree-adoption-from-disk.md).

## Context

Worktree creation is always host-side (`run_git_worktree_add` with
`--relative-paths`, ADR 0029) precisely so the git links resolve
identically on the host and inside the dev container. But that only
covers the IDE's own creation path. An agent running in the container
can reach for raw `git worktree add` through the `bash` tool — the
prompt tells it not to, and the tool can't intercept the call — and
git then writes **absolute `/workspace/…` paths** into both link
files:

- `<worktree>/.git` → `gitdir: /workspace/<base>/.git/worktrees/<name>`
- `<parent>/.git/worktrees/<name>/gitdir` → `/workspace/<base>/.worktrees/<name>/.git`

On the host neither resolves: `git worktree list` flags the entry
`prunable` and reports the container path, and the ADR 0079 adoption
sweep skipped it (host prefix mismatch + `<path>/.git` doesn't exist
at the reported container path). The checkout was invisible in the
project bar on every reload, and its branch stayed pinned.

## Decision

The adoption sweep recognizes and repairs the shape, because the
dev-container bind-mount layout is deterministic and 1:1
(`/workspace/<parent-basename>/…` ↔ `<host-parent>/…`):

- A reported worktree path under `/workspace/<parent-basename>/` is
  translated to its host equivalent before the location and liveness
  checks (`container_path_to_host`).
- `repair_absolute_worktree_links` rewrites each link file that is
  absolute **and** points inside the parent's container mount to the
  relative form `--relative-paths` would have written:
  `../../.git/worktrees/<name>` and `../../../.worktrees/<name>/.git`.
- A link naming anything outside the parent's mount (a real host path,
  a foreign mount) is left untouched — it may be valid, and rewriting
  a link we can't place would corrupt it.

The repair is host-side file I/O only — no git invocation, no
container requirement — and runs before the liveness check so a
repaired checkout binds in the same pass.

## Consequences

- Container-created worktrees (agent mistake or otherwise) show up in
  the project bar on the next startup / folder-add instead of
  lingering as prunable registrations.
- Idempotent: healthy relative links are a no-op.
- The underlying agent-guidance gap (don't run `git worktree` through
  `bash`) stays a prompt-level concern; this just makes the failure
  recoverable.

## Alternatives considered

- **Run `git worktree repair` on the host.** It only fixes links whose
  moved path it can guess from the current location; it does not know
  our `/workspace/<basename>` mapping, and it needs a git ≥ the
  version that wrote `extensions.relativeWorktrees` — the host's may
  be older. Two tiny text rewrites are exact and dependency-free.
- **Intercept `git worktree` in the agent's container bash.** Fragile
  string-matching on arbitrary shell; the sweep repair is a single,
  testable choke point that also covers future out-of-band creators.
- **Skip-and-log instead of repairing.** Leaves the row invisible and
  the branch pinned — the exact silent failure ADR 0079 set out to
  kill.

## Related

- [ADR 0029 — worktrees inside the parent repo](0029-worktrees-inside-parent.md)
- [ADR 0079 — worktree rows adopt from disk at startup](0079-worktree-adoption-from-disk.md)
