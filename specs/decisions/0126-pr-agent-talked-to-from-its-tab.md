# ADR 0126: A pull request's agent is talked to from its tab

## Context

[ADR 0124](0124-review-agent-and-notes.md) gave each pull request an
agent that answers on the diff. Starting it made its project the active
one and its session the coder panel's, which took the user away from
what they had open; and anything not about a block of the diff
(a comment, the pull request as a whole) had to be asked from the
coder panel, away from what it was about.

## Decision

- **Asked from the tab.** Under the conversation, a box asks the agent
  about the pull request. "Ask" on a comment or a review thread puts
  that comment or thread with the question (quoted to the agent,
  named on the user's card). The agent's reply to that turn is shown
  as a comment under the question; only its last message, not what
  it said between tool calls.
- **Out of the way.** Starting the agent changes neither the active
  project nor the coder panel's session. The session is in the
  sessions list and opens like any other.
- **Same session** as Explain and the diff's notes, so it keeps what
  it has learned. Its replies on the diff stay `review_note` cards.
- Like the notes, the questions and answers live in memory, and
  nothing is posted.

## Rejected

- **A panel of its own in the tab**, a second chat UI to keep in step
  with the coder panel's. The answers are short and read best next to
  the comments they are about.
- **A session per question.** Each would relearn the pull request.
- **Showing every message of the turn.** The narration between tool
  calls is noise here; the coder panel has it.

## Not covered

- Answers when the agent was busy with something else: a question
  queues behind the running turn, whose reply is then taken as the
  answer.
- Keeping the exchange, or the tab's link to its session, across a
  restart.
