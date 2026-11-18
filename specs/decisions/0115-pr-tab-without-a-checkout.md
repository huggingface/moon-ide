# ADR 0115: A pull request opens in a tab, not a worktree

Amends [ADR 0112](0112-github-inbox.md) (what Enter does in the inbox)
and [ADR 0028](0028-coder-worktree-sessions.md) (the confirm on
discarding a worktree); extends
[ADR 0113](0113-pr-context-in-review-tab.md).

## Context

Opening a PR from the inbox created a worktree for its branch, made
that folder active and opened Review changes there. For reading a PR
that is far too much: a checkout on disk per PR glanced at, a new
folder bar, and a folder switch that swapped the sidebar and the coder
session out from under whatever was going on in the same project.

## Decision

A PR opens as a `pr://<number>` tab of the folder the user is in:

- The PR head is fetched into a private ref (`refs/moon/pr/<n>`); no
  branch, checkout or worktree is created, and fork PRs work the same
  way. The tab shows the PR panel of ADR 0113 and a read-only diff
  from the merge base to the head, with review threads and moonbot's
  notes under their lines.
- **No folder switch** when the active folder is the PR's project or
  one of its worktrees. A PR of another bound project makes that
  project active first, since a tab belongs to a folder.
- **Background open** (Ctrl-click, middle click, `Ctrl+Enter`) adds
  the tab without showing it, in the PR's project when that isn't the
  active one, so several PRs can be queued from the inbox.
- **Review** posts a verdict (comment / approve / request changes)
  with a summary, by PR number.
- A worktree is made only on request: **Check out in a worktree**
  (the previous behaviour, with the editable Review changes tab) and
  **Start an agent on it** (an isolated coder session on the PR's
  branch). Neither is offered for a fork PR, whose branch isn't on
  `origin`.
- The tab is not restored at launch.
- **It loads in stages**: the description from one `gh` call, then
  the review threads and the diff side by side. The diff waits on a
  fetch from the remote, which is the slow part and no reason to hold
  back the text.

Discarding a worktree no longer asks first: the branch is kept, so
nothing is lost. It still asks before forcing the removal of one with
uncommitted or untracked changes
([ADR 0028](0028-coder-worktree-sessions.md)).

## Rejected alternatives

- **Keep the worktree, hide the cost** (create lazily, prune on tab
  close). Still a checkout per PR, and still a folder to switch to.
- **Read the diff from the GitHub API.** A second diff renderer and no
  way to open the full file; the git objects give both for one fetch.
- **A session per PR tab.** Reading a PR is not a conversation; the
  agent is one button away when it is wanted.

## Not covered

Inline draft comments in the PR tab: drafts are stored per folder and
anchored to a working tree (ADR 0027), neither of which a PR tab has.
Line comments go through the worktree review until that is reworked.
