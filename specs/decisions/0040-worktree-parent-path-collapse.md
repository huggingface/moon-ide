# ADR 0040 — Worktree sessions: parent-addressed paths collapse into the worktree

Date: 2026-07-22
Status: accepted

## Context

A worktree-backed session (ADR 0028/0029) pins its tool routing to
the worktree checkout — `cx.folder` resolves from the header's
`worktree_root`, and that part works. But the parent project stays a
bound folder, fully addressable by name and by absolute path, and the
model is actively steered toward it:

- In container mode the system prompt advertised every folder as
  `/workspace/<name>`. A worktree folder's name is its directory
  basename (`moon-agent-<id>`), so the session's _own_ advertised
  path didn't exist in the container (the checkout rides the parent's
  mount at `/workspace/<parent>/.worktrees/<slug>`), while the
  parent's `/workspace/<parent>` sat next to it, valid and inviting.
- `task` accepts a folder by basename; the parent's name is the name
  of the project the user's request talks about.

Observed failure (moon-landing, `sess-1784727842302-bf10dbb6`): the
model's first call was `task(folder: "moon-landing")` and every
subsequent `write_file`/`edit_file` used `/workspace/moon-landing/…`
paths. All of it routed to the parent checkout — the entire task
landed on `main`'s working tree and the worktree stayed pristine.
The isolation the worktree exists for was silently defeated.

## Decision

For a session whose `cx.folder` is a worktree, the parent project
means "my project". Three layers enforce it:

1. **Path collapse** (`ToolRegistry::resolve_target`): any path
   argument that resolves to the worktree's **parent** folder —
   host-absolute, synthetic `/workspace/<name>`, or bare-basename —
   is re-targeted to the worktree with the same relative path. A
   path spelling out this worktree's own `.worktrees/<slug>/…` tail
   strips to worktree-relative; a path into a _different_ worktree
   under the same parent is left alone (another agent's checkout,
   physically correct as addressed).
2. **`task` folder collapse** (`build_subagent_spec`): an explicit
   `folder: "<parent>"` from a worktree session resolves to the
   worktree, so delegated work stays on the session's branch.
   Cross-project targets are unaffected.
3. **Honest prompt** (`compose_system_prompt`): container mode now
   advertises a worktree folder at its real path
   (`/workspace/<parent>/.worktrees/<slug>` via
   `worktree_container_path`), the active marker names the branch,
   and the parent's row says paths there resolve into the worktree
   instead of the generic "sibling, reach via `task`". A worktree
   with no cached folder summary reuses its parent's (same codebase);
   the background summary generator skips worktree folders entirely.

`coder_rerun_tool_call` routes through the same worktree resolution
instead of the live active folder, so a reapplied edit lands where
the original did.

`bash` can still `cd` anywhere — a shell is a shell. The collapse
covers the structured surfaces where the leak actually happened and
the prompt no longer bait-and-switches the model.

## Alternatives considered

- **Prompt-only fix** (advertise the right paths, warn about the
  parent). Necessary but not sufficient — models drift back to the
  project's canonical name across long sessions and sub-agent
  reports. Rejected as the sole measure.
- **Hard-error on parent-addressed writes.** Safer-looking, but the
  model's intent is unambiguous (there is exactly one right target),
  and a mid-turn error burns iterations on a self-correction loop we
  can do for free. Reads would have to error too or the model edits
  what it didn't read. Rejected in favour of DWIM collapse.
- **Hide the parent from a worktree session's folder list.** The
  parent must stay listed — it's a real bound folder other sessions
  use, and its description is the best summary of the codebase.
  Rejected.

## Related

- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md)
- [ADR 0029 — worktrees inside the parent repo](0029-worktrees-inside-parent.md)
- [specs/coder.md § Worktree sessions](../coder.md#worktree-sessions),
  § Path resolution and cross-folder routing.
