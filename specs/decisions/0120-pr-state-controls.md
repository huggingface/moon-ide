# ADR 0120: A pull request's state is changed from its panel

Extends [ADR 0113](0113-pr-context-in-review-tab.md) and
[ADR 0115](0115-pr-tab-without-a-checkout.md).

## Context

A pull request could be read and approved in the IDE, but marking it
ready or merging it meant opening GitHub for one click. With many
small pull requests a day that is most of the trips to the browser.

## Decision

The pull request panel carries the state controls, in the PR tab and
in Review changes alike:

- **Ready for review / Convert to draft**, **Close / Reopen**: one
  click each. They are reversible.
- **Merge**: with the method the repository allows (a picker when
  there are several, the viewer's default first), and one more click
  to confirm, since it can't be taken back. The merge names the head
  commit the user was shown; GitHub refuses it when the branch has
  moved, so what is merged is what was reviewed.
- When GitHub reports the merge would be refused, the button is
  disabled and says why. An unknown status (GitHub works it out
  lazily) still lets the click through: GitHub has the last word.
- Each control is one `gh` call addressed by the pull request's URL,
  so no checkout is consulted or touched. The head branch is never
  deleted from here; the repository's own setting decides that.
- Nothing is ever done without a click: no agent, poll or shortcut
  reaches these.

## Rejected alternatives

- **A confirmation on everything.** Draft and close undo with the
  button that replaces them.
- **Bypassing branch rules** (`--admin`) for those who may. The rare
  case is worth the trip to GitHub, where it is visible what is being
  overridden.
- **Deleting the branch after the merge.** `gh` would also delete a
  local branch and move the checkout it is on.

## Not covered

Auto-merge ("merge when checks pass"), merge queues, updating a
branch that is behind, and editing the merge commit's message.
