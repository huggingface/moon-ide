# ADR 0118: A pull request's files are gone through one at a time

Extends [ADR 0115](0115-pr-tab-without-a-checkout.md).

## Context

The PR tab stacked every changed file's diff in one scroll under the
description. With a few files that reads well. With thirty it is a
page nobody can hold in their head: no sense of how much is left,
nothing to tell the lockfile from the change that matters, and a
conversation of any length pushes the first diff out of reach.

## Decision

- **A list of the files beside the diffs**, with status, lines added
  and removed, the number of unresolved threads and notes, and a tick
  once viewed. It is the way around the tab: "Overview" goes back to
  the description, a file goes to its diff, and the file being read is
  highlighted. In a pane too narrow for the list and two diff columns
  it opens over the page from the banner instead.
- **Viewed** on a file folds it and opens the next unviewed one, so a
  review is a walk through the list with the progress in sight.
- **A large pull request (more than 8 files) starts with every file
  collapsed**, and going to a file closes the others: one diff on the
  page at a time. Deleted, binary, generated and very large files
  start collapsed in any pull request.
- `j` / `k` step through the files and `v` marks the current one.
- Viewed marks are kept in memory, per pull request and head commit. A
  push clears them all.

## Rejected alternatives

- **Collapse everything and leave it at that.** Shorter, but still a
  scroll with no map and no progress.
- **A separate page per file** (list on the left, one file on the
  right, no scroll between files). Cleaner for large pull requests and
  worse for small ones, where scrolling through three files is the
  fastest way to read them; it would need both modes and a switch.
- **Split the tab into Conversation and Files**, like GitHub. The list
  already gets to either in one click, and the description stays where
  a reviewer starts.
- **Viewed marks that survive a push**, per file content like
  GitHub's. Needs the blob id of every file in the diff answer and a
  persisted store; worth it only if the marks prove useful.

## Not covered

The commit tab and Review changes keep their plain stack. Marks do not
survive a restart and are not sent to GitHub.
