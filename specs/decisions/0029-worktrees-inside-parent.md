# ADR 0029 — Worktrees inside the parent repo, relative links

Date: 2026-06-04
Status: accepted

Supersedes the container-handling parts of
[ADR 0028](0028-coder-worktree-sessions.md): the on-disk location, the
shared worktrees mount, the `git worktree repair` dance, and the
host-first/container-fallback git reads it forced. The rest of 0028
(isolation model, branch-is-the-deliverable, lifecycle, per-project
session list, move-into-worktree) stands unchanged.

## Context

ADR 0028 (W.4/W.4.1) placed each worktree **outside** any repo, under
`<state_dir>/worktrees/<parent-slug>/<branch-slug>`, and made it usable
in the dev container by bind-mounting the whole worktrees tree once at
`/workspace/.worktrees` and then running `git worktree repair` **inside
the container** to rewrite the worktree's git links to the in-container
paths.

That worked but bought real complexity, all rooted in one fact: a
worktree created out-of-repo gets **absolute** git links, and an
absolute path is valid in only one namespace. So the metadata had to be
"repaired" to whichever namespace was active, which meant:

- host-side git could **not** read a container-repaired worktree, so
  the IDE's git _reads_ needed a host-first/container-fallback hack and
  several reads (diff, blame, branch label) were just blank on a
  worktree folder in container mode;
- while the container was **down**, the worktree's git was unavailable;
- the worktree needed a prune-lock because the _other_ namespace's
  parent repo flagged its (non-existent-there) gitdir prunable;
- a shared mount + repair-on-create/start/resume had to exist at all.

## Decision

Place the worktree **inside the parent repo** at
`<parent>/.worktrees/<branch-slug>`, created with `git worktree add
--relative-paths` (git >= 2.48), and add `/.worktrees/` to the parent's
`.git/info/exclude`.

The worktree now rides the parent repo's existing `/workspace/<name>`
bind mount, so the worktree-to-parent **relative** offset is identical
on the host and inside the container. `--relative-paths` writes the two
git link files (`<worktree>/.git` and
`<parent>/.git/worktrees/<id>/gitdir`) as relative paths, so they
resolve in **both** namespaces at once. No separate mount, no repair,
no per-namespace metadata, no host-vs-container read split.

The worktree's container path is derived, not mounted: the parent's
`/workspace/<name>` plus the relative tail (`moon_core::
worktree::worktree_container_path`).

### Git version requirement

`--relative-paths` needs git >= 2.48 (Jan 2025). The host git is
version-gated: worktree creation errors with an actionable "update git"
message on anything older, rather than failing downstream. moon-base
builds git from source (>= 2.48) for the same reason — see below.

## Consequences

- **Host and container git both work on the worktree at all times** —
  container up or down. The host-first/container-fallback read hack
  from 0028 is no longer needed for correctness (it stays only as
  harmless robustness for genuinely degraded states); diff/blame/branch
  reads work on worktree folders in container mode.
- **The shared `/workspace/.worktrees` mount and all
  `git worktree repair` machinery are deleted** (the
  `WORKTREE_CONTAINER_ROOT` const, the compose mount, the
  create/start/resume repair hooks). Less code, fewer failure modes.
- **`git worktree add --relative-paths` sets `extensions.
relativeWorktrees` (+ `core.repositoryformatversion = 1`) in the
  repo's `.git/config`**, and git < 2.48 **refuses** such a repo
  (`fatal: unknown repository extension found`). Implications, all
  accepted:
  - The extension lives in **local** `.git/config`, which is not
    cloned/pushed — so teammates and CI cloning fresh are unaffected.
    The blast radius is the one machine that created the worktree.
  - On that machine, **both** host git and the container's git must be
    > = 2.48. The host is version-gated; moon-base builds git from
    > source. Any _other_ tool that touches the same local repo with an
    > older git would also need updating.
  - The extension **persists after `git worktree remove`** — trying one
    worktree session tags the local repo for git >= 2.48 going forward.
  - This was a deliberate choice over manually rewriting the link files
    (which avoids the extension and works on any git): native
    `--relative-paths` keeps git itself maintaining the relative links
    across future git operations, and the team controls both its host
    and container git versions.
- **`.worktrees/` is excluded from the parent's `git status`** via
  `.git/info/exclude` (written idempotently at create time, resolved
  through `rev-parse --git-common-dir` so it's correct even when `.git`
  is a file). The directory is a registered worktree, but the exclude
  keeps it from showing as untracked.
- **The prune-lock stays as defense-in-depth.** The W.4 hazard it was
  built for (the other namespace flagging the gitdir prunable) is gone
  now that the worktree resolves in both namespaces, but locking still
  marks a worktree IDE-managed and guards it if its checkout is ever
  transiently absent. The discard path still unlocks before removing.
- **Terminal / `bash` cwd** for a worktree folder map to the parent's
  `/workspace/<name>` mount plus the relative tail (frontend
  `containerCwdFor`, backend `worktree_container_path`), not a mount of
  their own.
- No migration: an existing out-of-repo worktree from the 0028 layout
  is simply stale and gets recreated, per
  [AGENTS.md § No premature migrations](../../AGENTS.md).

## Alternatives considered

- **Keep the 0028 out-of-repo placement + repair.** The status quo
  being superseded. Rejected: the absolute-path-per-namespace model is
  the source of every complication above.
- **Manually rewrite the link files to relative instead of
  `--relative-paths`.** Works on _any_ git (no extension, no version
  gate, the current moon-base 2.39 would be fine) and was validated
  end-to-end. Rejected in favour of native `--relative-paths` so git
  maintains the relative links itself; the team controls its git
  versions, making the >= 2.48 requirement acceptable. (If that
  requirement ever becomes a problem, the manual rewrite is the
  drop-in fallback.)
- **Nest inside the parent's working tree, the 0028 rejected
  alternative** (`<repo>/.moon-worktrees/<branch>`). 0028 rejected this
  over `git status` pollution and a blind `.gitignore` entry. Both
  objections dissolve here: `.git/info/exclude` (not a tracked
  `.gitignore`) keeps the status clean, and the in-repo placement is
  exactly what makes the relative links resolve across namespaces. The
  objection that _actually_ mattered — cross-namespace metadata — turned
  out to be worse for the out-of-repo design, not this one.

## Related

- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md) —
  the feature this refines; everything outside container path-handling
  still applies.
- [ADR 0007 — compose & moon-base](0007-compose-and-moon-base.md) — the
  dev image that now builds git from source.
