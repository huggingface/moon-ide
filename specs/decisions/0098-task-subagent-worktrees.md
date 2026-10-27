# ADR 0098: `task` can start a sub-agent in a worktree

## Context

ADR 0096 kept `enter_worktree` away from sub-agents on the grounds that
they inherit their parent's routing. That only holds when the sub-agent
targets the parent's own project. A sub-agent scoped to a sibling folder
(`task`'s `folder` argument) has no worktree to inherit: the parent
can't `enter_worktree` into another project. Asked to make a PR there,
the sub-agent fell back to a hand-rolled `git worktree add` outside
`.worktrees/`, which the IDE neither binds nor cleans up.

## Decision

- The top-level `task` tool takes an optional `worktree` argument with
  the same `{ name, base } | { branch }` shape and semantics as
  `enter_worktree` (fresh `moon/<slug>` branch, or attach to or check out
  an existing one). It's valid only with `mode: "agent"`.
- The parent's dispatcher resolves the worktree in the **target**
  folder's project before spawning, and scopes the sub-agent to that
  worktree folder. The sub-agent loop is unchanged: it already handles
  a worktree as its folder.
- The tool result carries `worktree: { branch, worktree_path, created }`.
- The sub-agent's prompt names the branch when its folder is a worktree,
  and tells it not to create worktrees itself.
- This supersedes ADR 0096's "expose it to sub-agents" rejected
  alternative, but only in this parent-chosen form.

## Rejected alternatives

- **Give sub-agents `enter_worktree`.** The sub-agent loop has no access
  to the coder state that worktree creation needs, and a sub-agent
  re-routing itself mid-run is harder to follow than a parent deciding
  up front where delegated work lands.
- **A `folder` argument on `enter_worktree`.** It moves the _parent_
  session into another project, which isn't what delegation wants.
