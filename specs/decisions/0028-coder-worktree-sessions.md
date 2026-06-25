# ADR 0028 — Worktree-backed coder sessions

Date: 2026-06-03
Status: accepted

## Context

[ADR 0016](0016-coder-concurrent-sessions.md) gave each bound folder
many concurrently-running coder sessions. But every session in a
folder still drives **one working tree**: they read and write the same
files, share one git index, and any `bash` build step they run lands
in the same checkout. Two agents working the same folder at once stomp
each other — interleaved edits, a half-built tree, racy `git add -A`
during a commit.

The user-visible need that motivated this ADR: run several agents on
the same project at the same time and end up with **separate,
clean deliverables** — "changes from multiple agents shouldn't end up
in the same branch if committed, or the same unstaged pile if not, so
I can cleanly make several PRs or commits." A workaround a user's
agent hand-rolled (create a `git worktree` via `bash`, work there,
then promote into the main checkout and delete the worktree) proved
the shape is right but exposed two gaps: an unbound worktree
directory isn't reachable by the coder's file tools, and there's no
IDE machinery to create, review, or clean up the isolated checkout.

The point is **isolation, not merging**. The deliverable is a branch.
Forcing the work back into the main checkout (the workaround's last
step) is exactly what defeats the goal of N independent PRs.

## Decision

A coder session can **opt into running in its own git worktree** — a
separate working directory checked out on a fresh branch, sharing the
repository's object store. Each isolated agent accumulates its work
(staged, unstaged, or committed) on its own branch, fully isolated
from the main checkout and from every other agent's worktree. The
user turns each branch into its own commit / PR through the normal
SCM flow. There is **no forced merge-back**; the branch is the unit
of delivery.

Opt-in is **per session** (the granularity the user picked): a "new
isolated session" affordance sits alongside the ordinary `+`. An
ordinary session keeps today's behaviour — it drives the folder's
main working tree.

### A worktree is a first-class bound folder

The worktree directory is registered as a **bound workspace folder**.
This is the load-bearing decision: every existing surface — file
tree, SCM panel, per-folder git-change badges, diff view, review
comments, terminal, LSP — is keyed by folder path and already gives
each bound folder its own [`WorkspaceHost`](../architecture.md#workspacehost-phase-2)
with its own git serialisation mutex ([ADR 0015](0015-git-serialisation.md)).
A worktree is just another repo root, so the whole stack lights up for
free and the user reviews / commits / pushes the branch with the same
SCM panel they already use. No parallel "review this agent's changes"
UI is built.

The worktree folder is **marked** (a folder `origin` discriminator,
see below) so it differs from a user-picked folder in three ways:

- It renders **nested under its parent** in the folder bar with a
  branch glyph, not as a peer top-level project.
- Its lifecycle is **tied to the session** that owns it (created on
  opt-in, offered for cleanup on session delete / explicit discard).
- It is **excluded from the manual "+ Add folder" restore semantics**:
  it is re-bound at startup only because its session still references
  it, and it is pruned (not just unbound) when discarded.

### Session ownership stays with the parent; only routing diverges

The session remains **owned by its parent folder**: its JSONL lives
under the parent folder's slug, it appears in the parent's session
list, and the panel keeps it where the user started it. What changes
is **tool routing** — the session's `read_file` / `write_file` /
`edit_file` / `bash` / `grep` / git tools run against the **worktree's**
`WorkspaceHost`.

Mechanically this mirrors the existing `subagent_target_folder`
precedent (a session whose tools run against a folder other than the
one it's filed under). Two optional fields go on the session JSONL
header:

```rust
worktree_root:   Option<String>,  // absolute path of the worktree checkout
worktree_branch: Option<String>,  // branch name the worktree is on
```

At turn time the runner resolves `cx.folder` to
`folder_for_path(worktree_root)` when the field is set, falling back to
the captured parent `folder_path` otherwise. The header round-trips on
reopen, so an isolated session keeps routing to its worktree across
restarts. The fields elide when `None`, so ordinary sessions keep a
byte-identical header; the schema version still bumps to `4` per the
project's convention.

A worktree session's **sub-agents inherit the routing**: they default
to the parent session's active folder, which is now the worktree, so
all of one agent's parallel sub-work lands on the one branch. That is
the desired behaviour — one agent, one branch, no matter how many
sub-agents it fans out.

### On-disk location and naming

Worktrees live **outside any repo**, under the per-workspace state
dir:

```
<workspaces_dir>/<workspace_id>/worktrees/<parent-folder-slug>/<branch-slug>/
```

Outside the repo keeps the parent's `git status` clean (no nested
working tree, no `.gitignore` hack) and makes per-workspace cleanup a
single directory walk. The bound-folder display name is
`<parent-name> ⑂ <branch>`; the directory basename embeds the parent
slug so two worktrees from different parents can't collide on the
`/workspace/<name>` container mount.

A session can also start from an **existing** branch instead of a
fresh one — a local branch, or a remote branch DWIM-created locally
the way `git switch <name>` does. This is how you point an agent at a
colleague's branch: it's checked out only in the worktree, so the
parent's working tree (and every other agent) is undisturbed — which
is the whole reason not to just `git switch` in the parent. A branch
can be checked out in only one worktree, so this fails if the branch
is already checked out somewhere (git's own constraint, surfaced
verbatim).

When starting fresh, the branch name defaults to a stable
`moon/agent-<short-id>` at creation (there's no diff to summarise yet)
and is renameable; an AI branch-name suggestion can replace it after
the first turn, the same "any outcome of the first turn" trigger
auto-rename already uses.

### Git primitives

Three new `WorkspaceHost` methods, routed through the active host the
same way every other git command is (`git_command(target, root)`,
host or `docker exec`, serialised behind the per-folder git mutex per
ADR 0015):

- `git_worktree_add(path, branch)` where `branch` is `New(name)` →
  `git worktree add -b <name> <path>` off `HEAD` (validated with
  `check-ref-format`), or `Existing(name)` → `git worktree add <path>
<name>`, letting git DWIM a local tracking branch from a remote when
  no local exists.
- `git_worktree_list()` — parses `git worktree list --porcelain`.
- `git_worktree_remove(path)` — `git worktree remove [--force] <path>`.

### Lifecycle

- **Create**: opting a session in runs `git_worktree_add`, registers
  the worktree as a bound folder, and stamps the session header.
- **Restore**: the worktree directory persists on disk, so startup
  re-binds it as long as a session still references it. A worktree
  whose directory vanished out-of-band is warn-and-skipped like any
  missing folder.
- **Discard**: removing the worktree folder (its `×` in the folder
  bar) prunes the git worktree, run against the **parent** repo's
  host. Guarded: `git worktree remove` refuses a dirty worktree, so a
  first failure re-confirms before forcing — an agent's uncommitted
  work isn't silently dropped. The **branch is never deleted by the
  IDE** — it's the deliverable; pruning a worktree leaves the branch
  in place for a later PR. Deleting the owning **session** does _not_
  prune the worktree: the branch is the deliverable, so clearing a
  noisy transcript shouldn't throw away the checkout you may still
  want to PR. (Unpushed commits ride the branch, which survives a
  prune, so the guard only needs to cover the dirty working tree.)

## Consequences

- The user gets N agents → N branches → N PRs with zero manual
  worktree bookkeeping, reviewed through the SCM panel they already
  know.
- The folder bar grows nested worktree rows. A folder with three
  isolated agents shows three child rows under it; the parent's own
  main-tree session is unaffected.
- A worktree is a real checkout: it costs disk (a second working
  tree) but shares the object store, so the marginal cost is the
  working-tree files, not the history.
- **Isolated sessions run their tooling _in the container_, like
  every other folder.** The cost the team cares about is the build
  toolchain: an isolated agent must `cargo build` / `pnpm install`
  with the container's tools, not the host's. The obstacle is that a
  worktree lives under the per-workspace state dir (outside the
  `/workspace/<name>` bind mounts) and git bakes absolute paths into
  its metadata, which only resolve where they were written. The
  resolution (validated against a live `moon-base` container):
  - The dev compose bind-mounts the **whole worktrees tree once** at
    [`WORKTREE_CONTAINER_ROOT`](../../crates/moon-protocol/src/container.rs)
    (`/workspace/.worktrees`). Mounting the root — not each worktree —
    means creating an isolated session never recreates the running
    container (docker can't hot-add a mount).
  - A worktree is created host-side, then `git worktree repair`'d
    **inside the container** so its metadata holds the
    `/workspace/.worktrees/…` paths. After that, container git +
    `bash` work in the worktree.
  - A worktree folder routes like any folder: the shell resolver, the
    coder's `bash` cwd, and `git_worktree_remove` all map it under the
    shared mount and run container-side when the workspace runs in a
    container (host-side otherwise). Repair re-runs on container
    start/resume, so the metadata self-heals across restarts.
  - Worktrees stay **out of** the per-folder bind-mount set (they're
    under the shared root mount), and fs tools remain host-direct.
  - Degradation is bounded: while the container is **down** the
    worktree's git is unavailable (its metadata holds container paths)
    — but so is the rest of the workspace, and starting the container
    repairs it. A pure-host workspace (no container) keeps everything
    host-side.
- **Worktrees are locked against prune.** A worktree's gitdir path is
  valid in only one context, so the _other_ context's parent repo
  flags it `prunable` — a stray `git gc` / `git worktree prune` (host-
  or container-side) would sever it. `git_worktree_add` therefore
  `git worktree lock`s every IDE-managed worktree, which makes prune
  skip it; `git_worktree_remove` `git worktree unlock`s first (the
  discard path), and `git worktree repair` preserves the lock.
  Validated end-to-end against a live `moon-base` container: a locked
  worktree survives a hostile `git gc --prune=now` +
  `git worktree prune --expire=now` from inside the container, and the
  branch + commits persist after removal.
- No migration: the header schema bump and the new folder
  discriminator are additive and pre-stable per
  [AGENTS.md § No premature migrations](../../AGENTS.md).

## Alternatives considered

- **Serialise / lock writes within one working tree** instead of
  isolating. Prevents corruption but not the goal: everyone's changes
  still pile into one branch / one unstaged set, so you can't make
  separate PRs. Rejected.
- **Auto-merge each worktree back into the main checkout** when the
  turn ends. This is the workaround's last step and directly
  contradicts the user's stated need to keep deliverables separate.
  Rejected.
- **Worktree as a session-scoped overlay** (a per-session working dir
  threaded into `ToolContext`, with a bespoke "review this session's
  changes" surface) rather than a bound folder. Avoids folder-bar
  clutter but rebuilds the entire SCM / file-tree / diff / review
  stack worktree-aware. Far more code for a worse review experience.
  Rejected in favour of reusing the folder machinery.
- **Nest the worktree inside the parent's working tree**
  (`<repo>/.moon-worktrees/<branch>`). Keeps everything inside one
  bind mount (nice for containers) but pollutes the parent's
  `git status` and forces a blind `.gitignore` entry. Rejected;
  out-of-repo placement is cleaner.
- **One worktree per agent run, auto-pruned at turn end.** Too
  ephemeral — the user reviews and PRs _after_ the agent finishes, so
  the worktree has to outlive the turn. Lifecycle is tied to the
  session, not the turn.

## Related

- [ADR 0016 — concurrent coder sessions](0016-coder-concurrent-sessions.md) —
  the per-folder multi-session model this builds filesystem isolation
  on top of.
- [ADR 0015 — git serialisation](0015-git-serialisation.md) — the
  per-folder git mutex the new worktree primitives acquire.
- [ADR 0024 — git commit in container](0024-git-commit-in-container.md) —
  the `git_command(target, root)` routing the worktree primitives reuse.
- [ADR 0025 — coder arbitrary-path fs](0025-coder-arbitrary-path-fs.md) —
  why an unbound directory was reachable by `bash` but not the file
  tools; a worktree becomes a _bound_ folder rather than relying on
  the arbitrary-path path.
- [specs/coder.md § Worktree sessions](../coder.md#worktree-sessions) —
  the spec section this ADR backs.
- [test plan 0098 — worktree sessions](../test-plans/0098-coder-worktree-sessions.md).
