# ADR 0124: A pull request's agent answers on the diff

## Context

Reading an agent-written pull request raises questions a diff can't
answer: why this lock, who else calls that. The user wanted one click
on a block of the diff to get a short explanation, and to ask back.
The explanation must come from an agent that can read the whole
repository and use every tool, not from a model shown one hunk.

## Decision

- **One agent per pull request.** An ordinary coder session, in a
  checkout of the pull request's branch (the worktree "Check out in a
  worktree" makes, reused when it exists). It is visible in the
  sessions list and can be talked to like any other.
- **`review_note(path, line, text, side?)`**, a tool every session
  has: it puts a card under a line of the diff being reviewed. It is
  a live event tagged with the agent's checkout; the IDE shows it on
  the pull request whose branch that checkout holds. Notes live in
  memory, and a push to the pull request drops them.
- **Explain** on a collapsed-lines bar (and on a file's header) sends
  that session a request naming the file and lines, to be answered
  with `review_note`. A reply typed in a card is sent to the same
  session, so a thread keeps the agent's context.
- **Nothing is posted.** The tool has no path to GitHub; what the user
  wants on the pull request, they write themselves.

## Rejected alternatives

- **A one-shot model call per block**, fed the hunk and the
  description. Fast and parallel, but it can only paraphrase the diff.
- **A hidden session.** The user should see what the agent reads and
  be able to steer it.
- **A session per block.** Each would relearn the pull request.
- **Running the agent in the project's own checkout.** It would read
  the default branch, not the code under review.

## Not covered

- A fork's pull request: its branch can't be checked out here.
- Requests queue behind one another in the one session.
- The checkout is not updated when the pull request is pushed to.
- Notes in the Review changes tab, and keeping them across restarts.
