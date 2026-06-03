# ADR 0024 — Route `git commit` (and its hooks) through the active shell target

Date: 2026-06-21
Status: accepted

## Context

`run_git_commit` / `run_git_commit_on_new_branch` always spawned
`git` directly on the host (`git -C <root> …`), even when the
workspace shell container was `Running`. That includes the
`git add -A`, the `git commit` that fires the project's
**pre-commit hook**, the commit-safety snapshot dance
(ADR 0015), and the rev-parse/log reads afterwards.

The hook is the problem. A repo's `.husky/pre-commit` runs e.g.
`bunx lint-staged`; `bunx` / `node` / the pinned formatters live
in the **container's** userland (`node_modules/.bin/`, fnm's
default Node, system bins from `moon-base`), not necessarily on
the host. Running the hook on the host means it inherits whatever
`PATH` the desktop process was launched with — which, for a GUI
launch that never sourced the user's shell rc, omits `~/.bun/bin`
and fnm entirely. Symptom: `git commit` fails with
`.husky/pre-commit: bunx: not found` (code 127) even though the
same `bunx` works fine in the integrated terminal (terminals
attach via `docker exec`/login shell and get the full PATH).

This also broke the cross-cutting invariant that _anything doing
I/O on the workspace goes through the active `WorkspaceHost` so it
behaves identically on host and in a container_ — and it was
inconsistent with format-on-save, which already routes through
`ShellTarget::Container` (ADR 0013 § Container routing).

## Decision

Thread the active `ShellTarget` into the commit path and run
**every** git invocation in `run_git_commit` /
`run_git_commit_on_new_branch` (and the safety-snapshot helpers)
through it:

- `ShellTarget::Host` → `git -C <root> …`, unchanged.
- `ShellTarget::Container` →
  `docker exec -w <server_root> <container> git -C <server_root> …`,
  where `server_root` is `root` translated through the bind mount.

The new private helper `git_command(target, root)` in
`crates/moon-core/src/host.rs` builds the right `Command`; all
call sites in the commit path go through it. The
`LocalHost::git_commit` / `git_commit_on_new_branch` methods
resolve `shell_target()` once (before the `spawn_blocking`) and
pass it down, exactly like format-on-save.

Routing **all** of the commit's git calls (not just the
hook-firing `git commit`) keeps the index / object view coherent
within one logical operation. That's safe because `.git` is the
same bytes on both sides through the bind mount — host-side and
in-container git see an identical repository. What changes is
purely the hook's process environment.

### Fallback

When `root` is outside the container bind mount (path translation
returns `None`), `git_command` falls back to host execution
rather than spawning git against a path the in-container process
can't see — same posture format-on-save takes.

## Consequences

- The pre-commit hook now runs with the container's toolchain
  when a container is active, so `bunx lint-staged` and friends
  resolve their binaries. This is the behaviour the team expects
  and matches what the integrated terminal already does.
- The per-folder git mutex (ADR 0015) and the commit-safety
  snapshot are unchanged in _what_ they do; they just run their
  git subprocesses in the container too, so the snapshot's
  `read-tree` / `checkout-index` restore lands on the same
  repository the hook mangled.
- Host-only workspaces (no resolver, or container not running)
  are completely unaffected — `ShellTarget::Host` is the default
  and reproduces the previous code path verbatim.
- The other git operations (`git_push`, `git_pull`,
  `git_status_entries`, blame, diffs, fetch, …) still run on the
  host. They don't fire user hooks and don't need the container's
  PATH, so leaving them host-side avoids a `docker exec` per
  background poll. If a future need surfaces (e.g. a credential
  helper that only exists in the container), the same
  `git_command` seam extends to them one at a time.

## Supersedes / relates to

- Extends [ADR 0015](0015-git-serialisation.md) — the commit path
  it serialises now optionally runs in the container.
- Mirrors [ADR 0013 § Container routing](0013-format-on-save-file-based.md)
  — same `ShellTarget` seam, same bind-mount path translation,
  same outside-mount fallback.
