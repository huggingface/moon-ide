# ADR 0113: The review tab shows the pull request, and can approve it

Amends [ADR 0027](0027-review-comments.md) (non-goals: other people's
comments, APPROVE / REQUEST_CHANGES).

## Context

With the inbox ([ADR 0112](0112-github-inbox.md)) a review starts in
the IDE, but the Review changes tab showed only the local diff. What
the PR is for, whether its checks pass, and what reviewers — human or
bot — already said still meant a trip to GitHub. moonbot also keeps
nit-level notes in the PR description, in a marker-delimited block,
because GitHub offers no better place; there they are easy to miss and
detached from the lines they are about.

## Decision

When the folder's branch has an open GitHub PR, the review tab shows it:

- **A panel above the file sections**: title, state, review decision,
  check counts, description, moonbot's notes, open review threads, and
  the checks list (failing first). Notes and threads are a jump list
  into the diff.
- **The conversation in the panel**, after the description: top-level
  comments and review summaries, oldest first, long ones cut short
  until asked for. They are where a bot's whole review often lives,
  and nothing else in the tab showed them. Comments GitHub hides are
  left out, as are reviews that are only a carrier for inline threads.
- **Under their lines in the diff**: review threads, and moonbot notes
  that name a line. Resolved threads are folded to one row.
- **Read-only.** Replying to or resolving a thread still happens on
  GitHub; each thread links there.
- **Placement is by the line number GitHub reports at the PR head**
  (for notes: at the commit moonbot reviewed, which the panel names and
  flags when the branch has moved). A checkout that isn't at that
  commit shows them a few lines off. The user's own drafts keep their
  content-based anchoring.
- **GitHub's markdown dialect** for descriptions, notes and thread
  comments: raw HTML is rendered (sanitised, with no `style`, ids,
  author classes or form controls), comments are dropped, bare URLs
  link, a newline breaks the line, and `> [!NOTE]`-style alerts are
  callouts. File previews keep raw HTML escaped.
- **Notes are parsed here**, from the last
  `<!-- moonbot:minor-notes:start -->` … `end` block; the block is
  removed from the description shown.

Publishing a review takes a verdict — comment, approve or request
changes — and no longer needs a draft comment: an approval can be
empty, a request for changes needs a summary, and a comment-only
review needs a draft or a summary. Drafts that all fail to place are
never turned into a bare verdict.

The PR is re-read when the tab opens and after a publish. No polling.

## Rejected alternatives

- **Re-anchor threads and notes by content**, like local drafts. They
  arrive with a line number and no fingerprint; guessing a new position
  for someone else's comment is worse than showing it slightly off.
- **Reply and resolve in the IDE.** A second write path and thread
  state to keep in sync, for something the GitHub link covers.
- **A moonbot-agnostic "notes in the description" format.** One bot
  writes these for one team; the markers are its contract.
- **Poll the PR while the tab is open.** Checks change on a scale of
  minutes; the refresh button is enough until it proves not to be.

## Not covered

Forgejo PRs (the panel and annotations are GitHub-only),
issue-style conversation comments, and deployments.
