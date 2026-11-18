# ADR 0121: Pull request and issue tabs belong to the workspace

Supersedes the folder rules of
[ADR 0115](0115-pr-tab-without-a-checkout.md) and
[ADR 0117](0117-issue-tab.md).

## Context

A pull request tab belonged to a folder's tab strip and read "the
active folder's" repository. Opening a pull request of another
project therefore made that project active first: the sidebar, the
session and the editor all changed for what was meant as a look. And
a queue of pull requests opened from the inbox was scattered over the
folders they came from, out of sight after any folder switch.

## Decision

Pull request and issue tabs are the workspace's, like the inbox:

- A tab opens where the user is, whichever bound project it is about,
  and is in every folder's strip from then on. Nothing switches
  folder to open one.
- Switching folders while looking at one keeps it in view. Closing it
  closes it everywhere. Each folder keeps its own order and split
  placement of them.
- The tab's path carries its project folder
  (`pr://<folder>#<number>`), and every command it calls names that
  folder instead of relying on the active one. A review, in
  particular, is posted through the pull request's project: "the
  active folder" could be another repository with a pull request of
  the same number.
- What needs the project does switch to it, on a click: checking the
  branch out, starting an agent on it, opening one of its files. The
  tab comes along.
- They are still not restored at launch.

## Rejected alternatives

- **Keep per-folder tabs, drop only the switch** (a tab of project B
  living in folder A's strip). The switch goes away, but the queue is
  still lost on the next folder change.
- **A separate strip or panel for pull requests.** A second place to
  look, for what is a handful of tabs.
- **Address the repository by `owner/name`.** The diff is read from a
  local object store, which needs a folder anyway; and the mapping
  from repository to folder is only known once the inbox has loaded.

## Not covered

Commit tabs and Review changes stay with their folder: they are
about its checkout.
