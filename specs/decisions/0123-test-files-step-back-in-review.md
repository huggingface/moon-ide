# ADR 0123: Test files step back in pull request review

Extends [ADR 0118](0118-pr-files-one-at-a-time.md) and
[ADR 0119](0119-inbox-signals-and-groups.md).

## Context

Agent-written pull requests come with long tests. A `+93 −27` in the
inbox turned out to be `+33 −27` of code and a 60-line test, and the
test's diff sat open in the page next to the change that mattered.
The size shown should be the size of what has to be read closely.

## Decision

- A file is a test by its path alone: under a `test`, `tests` or
  `__tests__` directory, or named `*.test.*`, `*.spec.*`, `*_test.*`
  or `test_*.py`. The core decides, once, for both surfaces below.
- In a pull request tab, test files start collapsed, and the file
  list's total leaves them out. Their own count stays, greyed.
- In the inbox, a row's `+/−` leaves them out as soon as the row's
  digest is in; before that, and for a pull request of more than 100
  files (the digest reads no further), it is GitHub's total.
- The tooltip on either total says how much the tests add.

## Rejected alternatives

- **A `spec` / `specs` directory as tests.** This repository keeps its
  specifications there.
- **Hiding test files, or their counts, altogether.** Whether a change
  has tests, and roughly how much, is part of the review.
- **Leaving lockfiles and other generated files out of the total too.**
  Not asked for; they already start collapsed.
- **A per-user pattern setting.** One team, one convention so far.

## Not covered

Tests written inside a source file (Rust's `mod tests`) count as code.
