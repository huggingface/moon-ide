# ADR 0018 — Auto-stage on save during merge resolution

Date: 2025-11-21
Status: accepted; first ADR to spell out the "no index in the UI" stance that
[Phase 5 §5.3 / §5.6](../roadmaps/phase-05-git.md) and
[ADR 0015](0015-git-serialisation.md) imply.

## Context

The SCM panel deliberately doesn't model git's index. Every gesture
the panel exposes today operates on whole files:

- **Commit** runs `git add -A` immediately before `git commit -m …`
  so the index is a transient implementation detail (see
  [ADR 0015](0015-git-serialisation.md) for the safety-snapshot
  dance around that pair).
- **Discard / Restore** runs `git restore --source=HEAD --staged
--worktree -- <paths>` — one command, no per-stage choice.
- **Revert all** is the same gesture, applied to the whole tree.

There is no "Stage hunk", "Stage file", "Unstage", or "View
staged diff" affordance. The team's mental model is "edit the
files, then commit them" — and the existing surface is enough to
keep them productive.

Phase 5 §5.6 wires in-app merge-conflict resolution. The
mechanics force the index back into the picture: `git merge`
populates the index with three-stage entries for every conflicted
file, and a file's `UU` status only clears when something runs
`git add <path>` against it. Two options for closing that loop:

1. **Explicit "Mark resolved" gesture.** A per-row action (or a
   "Stage" button somewhere in the panel) that runs `git add` on
   the file. Mirrors the VS Code / GitLens UI. The user
   explicitly says "this is done"; the SCM panel keeps the
   index visible in this one corner of the world.
2. **Auto-stage on save.** When the user saves a file that was
   reported `conflicted`, run `git add` for them iff the saved
   bytes no longer carry column-0 conflict markers. The user's
   mental model becomes "edit the file until it looks right,
   save it, the conflict marker on the row disappears" — no new
   gesture, no new concept.

## Decision

**Take option (2): auto-stage on save during merges.**

The hook lives in `WorkspaceState.saveActive`:

1. Capture `wasConflicted` before the write — was the file in
   `gitStatusEntries` with `status === 'conflicted'`?
2. Run the regular save (format-on-save pipeline, post-save
   re-read, fingerprint refresh, LSP didChange).
3. If `wasConflicted && !hasConflictMarkerLines(freshText)`,
   call `git_add_paths([file.path])`. Best-effort: failure is
   silent; the next status refresh re-evaluates the row.
4. Kick `refreshGitMergeState` so the SCM panel's
   `unmergedPaths` list updates and the "N unresolved" hint
   recounts.

`hasConflictMarkerLines` checks for column-0 `<<<<<<<` /
`=======` / `>>>>>>>` (seven literal characters each). Indented
or inline occurrences (this very ADR, test fixtures, JSON-like
templates) don't trip it.

The full merge-commit flow still runs `git add -A` server-side
inside `git_commit`, so any file the auto-stage hook missed (e.g.
the user resolved it from a terminal without re-saving through
moon-ide) still gets staged when they click **Commit merge**.

## Why not option 1

- **One concept, not two.** The rest of the SCM panel never
  surfaces the index. Adding a "Mark resolved" button only for
  the merge case forces the user to learn a gesture that
  contradicts everything else they use the panel for.
- **Save already means "I'm happy with this".** Editing a
  conflicted file is itself a "I'm doing something with this
  row" gesture; the conflict block is right in front of the
  user. Forcing a second click after the save would feel like
  busywork.
- **Soft-warn covers the "saved-without-resolving" footgun.**
  Committing a file that still has marker text on disk pops a
  confirm. So the worst case from auto-staging too eagerly is
  the user gets a dialog when they click commit, not a
  silently-broken commit.
- **Consistency with `git commit`'s own behaviour.** Plain `git
commit` with `MERGE_HEAD` present runs `git add -A` for the
  user under the hood. Auto-staging on save just moves that
  same gesture earlier so the row's badge clears immediately
  rather than only at commit time.

## What this rules out (for now)

- **Index visibility.** No "staged" vs. "worktree" split in the
  file tree. The SCM panel still shows one row per path.
- **Partial-stage hunks.** Phase 5's "Still outstanding" already
  lists per-hunk stage / discard as deferred; this ADR doesn't
  change that calculus — if and when we wire it, the diff view's
  per-hunk UI will be the surface, not a separate "staging area"
  view.
- **A "Mark resolved" command** in the command palette.
  Symmetrical reasoning: the gesture already exists (save the
  file).

## Consequences

- The trait grows `git_add_paths(paths: &[String]) -> MoonResult<()>`
  (and its Tauri/IPC siblings). Same lexical containment as
  `git_restore_paths`. The merge-resolution flow is the only
  caller for now; future per-hunk staging would route through a
  different method.
- The fs-watcher's `.git/MERGE_HEAD` / `MERGE_MSG` whitelist (and
  the matching frontend listener) does the heavy lifting for
  panel reshape: the auto-stage hook is purely the "clear the
  badge immediately" optimisation. If the IPC fails or the user
  resolves out-of-band, the next status refresh still produces
  the right surface.
- If the team starts asking for explicit "Mark resolved" in
  practice (e.g. a workflow where they want to commit some
  resolved files before others, or stage a binary conflict the
  marker scan can't detect), we revisit — likely by adding a
  per-row context-menu entry, not by reworking the whole index
  story. Until then, this stance flows from
  [AGENTS.md §Scope discipline](../../AGENTS.md): hardcode the
  first concrete need, defer the second.
