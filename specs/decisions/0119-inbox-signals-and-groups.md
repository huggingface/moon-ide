# ADR 0119: The inbox shows what stands out, and can sort itself by topic

Extends [ADR 0112](0112-github-inbox.md).

## Context

The inbox is one list, newest first. With a few authors opening many
pull requests each, that is forty rows that look alike: nothing says
which have comments waiting or notes from the review bot, and nothing
says which six are the same kind of change and can be gone through in
one sitting.

## Decision

**Signals.** Each open pull request's row shows its unresolved review
threads and moonbot's minor notes. They come from a second request
(`inbox_digests`), made only for rows that are new or whose
`updatedAt` moved, so a poll costs what it did before.

**Groups.** A "Group by topic" toggle shows the open pull requests
under named groups made by one model call (title, author, branch,
size, changed paths, start of the description; the coder's standard
model). Everything else is listed last, ungrouped.

- Grouping is by what a change is about, never by author.
- A pull request is placed once and stays put: a push doesn't move
  it, and a new arrival is offered the existing groups first. Only
  "Regroup" starts over. Stability matters more than the best
  possible grouping at every moment.
- Groups and assignments are stored per workspace, with the rest of
  the inbox's state.
- The model is called only while the toggle is on, for pull requests
  it hasn't been asked about, and never again by itself after a
  failure.
- The model sorts. It does not review, rank or post anything.

## Rejected alternatives

- **Rules instead of a model** (by path prefix, label, author). Cheap
  and predictable, but the useful groups ("billing emails", "token
  revocation") cut across all three.
- **Regroup at every poll.** Rows would jump between groups while
  being read, and every poll would cost a model call.
- **Fetch threads and bodies with the listing.** Fifty descriptions
  and thread lists on every search is what made the batched query
  time out ([ADR 0112](0112-github-inbox.md)).
- **Group everything, issues and notifications included.** They are
  few and read differently; a group of one issue is noise.

## Not covered

Reviewing a group in one tab, and an agent that reads a group's pull
requests and briefs the reviewer, are the next steps and not built.
Whatever they add, a verdict or a comment is only ever posted by the
user, in their own words.
