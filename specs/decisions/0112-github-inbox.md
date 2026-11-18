# ADR 0112: GitHub inbox tab

## Context

Reviewing pull requests — a growing share of them opened by agents —
started outside the IDE: github.com/notifications filtered to unread,
or one repository's PR list filtered to an author. moon-ide listed PRs
only in the branch switcher: one repository, 30 rows, no CI or review
state, and opening one moved the project's checkout.

## Decision

An `inbox://github` pseudo-tab lists what wants the user in the
workspace's repositories — the GitHub remotes of its bound folders —
as one list, newest first, each item once. Toggle chips above it
filter by why an item is there: **Unread** (notification threads),
**Review requested**, **Watched**, **Mentioned**, **Mine**. None
selected shows everything; several show their union. Each row carries
its reasons as tags.

- **Sources.** Searches for open PRs (`review-requested:@me`,
  `mentions:@me`, `author:@me`, and one per watched author), and the
  unread notification threads, all limited to the workspace's
  repositories. Pull requests behind threads that no search
  returned are fetched by number; issues and other subjects are listed
  from the notification alone.
- **Watched authors** are a per-workspace list, edited in the tab. A
  bot is written `name[bot]`.
- **Transport** is the host's `gh` (`gh api graphql`, `gh api`), like
  the rest of the PR surface: no token handling of our own.
- **One request per source.** A snapshot names the sources that failed;
  the tab keeps their previous rows and says so.
- **Polling**, no webhooks: every 75 s while an inbox tab is on screen
  in a focused window, every 5 min otherwise so the counts on the
  sidebar's Inbox button stay current.
- **Opening a pull request** (Enter) opens it in a tab, without a
  checkout ([ADR 0115](0115-pr-tab-without-a-checkout.md)), and marks
  its thread read. Issues and other subjects open on GitHub.
- **The tab is pinned.** Once open it is the first tab of every
  folder until closed, and switching folders while looking at it keeps
  it in view: it is the one tab that is about the workspace, not a
  folder. Its open state is saved with the rest of the inbox's state.
- **Mark read** (`e`) marks the notification thread read on GitHub.
- **Snooze** (`s` / `w`) hides an item until 08:00 the next day or the
  next Monday; a **Snoozed** chip shows what is hidden. GitHub has no
  equivalent, so this is local state. Time alone ends a snooze; new
  activity does not.
- **Local state** (watched authors, snoozes) is per workspace, in
  `WorkspaceSession.inbox`: the inbox is scoped to a workspace, and
  two workspaces' windows saving one machine-wide list would drop each
  other's changes.

Design tokens for radius, spacing and type size (`--m-radius-*`,
`--m-space-*`, `--m-text-*`) and a shared `Pill` land with this tab,
which is their first user; existing components move over as they are
touched.

## Rejected alternatives

- **One batched GraphQL query for every search** (moon-dash's shape).
  With account-wide searches and the check rollup it regularly ran past
  GitHub's 10 s limit and returned a 502 with no data at all.
- **Per-check CI status** to tell a cancelled run from a failed one.
  Fetching the contexts roughly doubled a search's time; the rollup is
  shown as GitHub reports it.
- **Account-wide lists**, like the notifications page this replaces
  (the first cut did this). A workspace is a unit of attention: a
  window on one set of repositories should not count another team's
  review requests, and every row can then be opened in the IDE.
- **A section per reason** (the first cut). The same PR showed up to
  four times and the lists had to be scanned one by one.
- **Local pin and done state** (moon-dash has both). Marking read
  covers "done" for threads; pinning waits for a need.
- **Waking a snoozed item on new activity.** Predictable beats clever:
  the point of snoozing a noisy PR is not hearing about it until then.
- **Show github.com in a browser tab.** The point is to review in the
  IDE's own diff, next to the agent that can act on it.

## Not covered

Forgejo. What a PR shows once open is
[ADR 0113](0113-pr-context-in-review-tab.md) and
[ADR 0115](0115-pr-tab-without-a-checkout.md).
