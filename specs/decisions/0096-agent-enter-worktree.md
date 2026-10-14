# ADR 0096: Agents move themselves into a worktree with `enter_worktree`

## Context

Ordinary agent sessions that need a branch ran `git worktree add` or
`git checkout -b origin/main` themselves through `bash`. That checkout
didn't belong to their session: the session's tools kept running
against the project's main tree, and the folder bar only picked the
worktree up at turn end (ADR 0079). A branch created with
`git checkout -b x origin/main` also tracks `main`, so with
`push.default=upstream` a plain `git push` went straight to `main`.
Only coordinators had an IDE path to a worktree (`spawn_worker`), and
only the panel button could move a session into one.

## Decision

- Top-level `Agent` sessions in a project get an `enter_worktree` tool.
  `name` creates `moon/<slug>` (same slugging + de-dup as worker
  names, ADR 0042) off the default branch, or off `base`, created
  `--no-track` (ADR 0093). `branch` checks out an existing local or
  remote branch; if an IDE-managed worktree under
  `<project>/.worktrees/` already has it, the session attaches to that
  one instead of failing on git's "already checked out" refusal.
- The tool stamps `worktree_root` / `worktree_branch` on the session
  header, the same routing the panel's move-into-worktree button sets
  (ADR 0028), and re-points the **current turn** at the worktree: later
  tool calls in the same batch and every following round-trip run in
  it, and the system prompt's active folder is refreshed.
- Switching from one worktree to another is allowed; the old checkout
  and its branch stay in place (the branch is the deliverable).
- The frontend learns of the move through `session_worktree_changed`
  (renamed from `session_worktree_cleared`, now carrying the new
  fields) plus `workspace_folders_changed`.
- The base prompt tells agents to use the tool instead of creating
  branches by hand, and to push with `git push -u origin HEAD`.

## Rejected alternatives

- **Detect a `git worktree add` in `bash` and auto-switch.** Guessing
  intent from shell commands; a worktree made for inspection would
  hijack the session.
- **Apply the routing from the next turn only** (like the panel
  button). The agent calls the tool mid-task and keeps going; tools
  that still hit the main tree until the turn ends are exactly the bug.
- **Expose it to sub-agents or coordinators.** Sub-agents inherit
  their parent's routing; coordinators isolate work with
  `spawn_worker`.
