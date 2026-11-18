# ADR 0117: Issues open in a tab

Extends [ADR 0112](0112-github-inbox.md) and
[ADR 0115](0115-pr-tab-without-a-checkout.md).

## Context

The inbox lists issues next to pull requests, but only a pull request
opened in the IDE; an issue went to the browser. Most of them are read
once and acted on from the coder panel, which the browser is not next
to.

## Decision

An issue of a bound repository opens as an `issue://<number>` tab of
the folder the user is in, the same way a pull request does (no folder
switch within a project, background open, not restored at launch). It
shows title, state, labels, author, assignees, the description and the
comments, in GitHub's markdown, read through `gh issue view`.

Read-only: commenting, closing and labelling stay on GitHub, one click
away.

The comment list is the one a pull request's conversation uses
([ADR 0113](0113-pr-context-in-review-tab.md)).

## Rejected alternatives

- **An embedded GitHub page.** GitHub refuses to be framed, and a
  webview of it would be a second browser to keep signed in.
- **Reply from the tab.** A write path for something done a few times
  a week; wait until it is asked for.

## Not covered

Discussions, releases and the other notification subjects still open
on GitHub. Why a closed issue was closed is not shown: `gh issue view`
doesn't report it on every `gh` version in use.
