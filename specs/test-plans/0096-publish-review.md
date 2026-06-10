# Test plan 0096: publish review comments to GitHub (Phase 5.7.2)

Date: 2026-06-13

## What shipped

- The Review tab's **"Publish review →"** CTA (enabled when there are draft comments) opens a dialog: an optional review-summary textarea and Publish / Cancel. Publishing posts every draft comment to the current branch's GitHub PR as **one** review (`event: COMMENT`).
- `WorkspaceHost::publish_pr_review` shells out to `gh` (never a raw token): `gh pr view --json number,headRefOid,state` resolves the PR + head SHA, then `gh api --method POST /repos/{owner}/{repo}/pulls/{n}/reviews --input -` posts the assembled review JSON.
- **Commit-drift reconciliation.** Each comment is re-anchored against the **PR head** version of its file (`git show <headRefOid>:<path>`) by content fingerprint. Clean / drifted comments post at their head-diff line (`line`+`side`, never the legacy `position`); comments whose line isn't present at the head are reported **lost** and kept as local drafts.
- On a successful post the comments that landed are **deleted locally** (drafts live only until they're on GitHub); lost ones remain. No PR for the branch → the dialog says so and links the user toward creating one; nothing posts.
- Rust fingerprint (`review_fingerprint`) is locked to the frontend's by a parity unit test, so a fingerprint written at comment-creation re-resolves at publish.

## How to test

Prerequisites: `bun install`; `gh` installed and authenticated (`gh auth status` OK); a GitHub-remote repo bound as the active folder, on a branch that **has an open PR**. Run `bun dev`.

1. Add 2-3 review comments across a couple of files (test plan 0095). The banner's "Publish review →" button enables and shows the comment count.
2. Click **Publish review →**. Expected: a dialog opens with the comment count, an optional summary field, and Publish / Cancel.
3. Type a summary, click **Publish**. Expected: the button shows "Publishing…", then the dialog shows e.g. `Posted 3 comments as one review.` Open the PR on github.com → confirm a **single** review appears (one event / one notification) with your summary as the review body and each inline comment on the right line/side.
4. Back in the IDE: expected the posted comments are **gone** from the Review tab (banner count drops to 0, cards removed). Reload the IDE — they stay gone (cleared from the session).

### Commit drift

5. Add a comment on a line, then **push a new commit to the PR** (from a terminal or another machine) that changes a _different_ part of the same file, leaving the commented line intact. Publish. Expected: the comment still lands on the correct line at the new head — it re-anchored by content against `headRefOid`, not your local HEAD.
6. Add a comment on a line, then push a commit that **rewrites that exact line**. Publish. Expected: the dialog reports e.g. `Posted 1 comment … 1 couldn't be placed and stayed as a draft.` That comment remains in the Review tab; the others posted.

### No PR

7. Check out a branch with **no open PR** (or detach HEAD). Add a comment, Publish. Expected: the dialog says `No open PR for <branch>. Push the branch and open a PR, then publish.` Nothing posts; the comment stays a local draft.

### Auth / gh failures

8. Temporarily break auth (`gh auth logout`), Publish. Expected: a `Publish failed: …` outcome in the dialog (gh's error surfaced); comments stay as drafts. Re-auth and retry succeeds.

## What must keep working

- Everything in test plan 0095: composing (keyboard + gutter "+"), editing, deleting, markdown rendering, re-anchoring, stale state, Viewed checkboxes, banner counts.
- 5.7.0 persistence: drafts and Viewed marks round-trip through `session.json`; clearing posted comments persists.
- Existing `gh`-backed features (branch switcher PR list, `gh pr checkout`) are unaffected — publish adds new `gh pr view` / `gh api` calls, it doesn't change the others.
- The dialog never blocks the app: Cancel and the overlay click close it (except mid-publish, when the buttons disable until the round-trip returns).

## Known limitations

- **GitHub only** (matches `remote_web_url`). A non-GitHub remote has no PR to resolve; publish reports no PR.
- **`event: COMMENT` only.** No Approve / Request changes from the IDE yet — the `event` field is the seam to add them.
- **No `gh pr create`.** If there's no PR we stop and tell the user; we don't open one for them.
- **Lost comments aren't auto-relocated to the PR head.** They stay as local drafts for the user to re-place or delete.
- A non-BMP (astral-plane) character inside an anchored line would make the Rust/JS fingerprints disagree; such a comment goes "lost" (safe failure — stays a draft). Not worth UTF-16 surrogate handling for source-code review.

## Related

- Spec: [`specs/review-comments.md`](../review-comments.md)
- Decision: [ADR 0027 — local-first review comments](../decisions/0027-review-comments.md)
- Roadmap: [`specs/roadmaps/phase-05.7-review-comments.md`](../roadmaps/phase-05.7-review-comments.md)
- Builds on: test plans 0095 (review comments UI), 0074 (Review changes tab)
