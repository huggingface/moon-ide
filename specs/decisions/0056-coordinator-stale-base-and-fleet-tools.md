# ADR 0056 — Coordinator stale-base & fleet tools

Date: 2026-08-04
Status: accepted; implemented.

## Context

A coordinator driving several workers to PRs hit the failure
[ADR 0028](0028-coder-worktree-sessions.md) predicted but didn't
instrument: **stale-base drift**. A worker's branch is created off
`origin/main` at spawn time and only drifts further behind as its
siblings' PRs merge. Observed in a real session, two workers (#142 and
#89) produced diffs that _silently deleted_ merged work — one ~2900
lines — because their worktrees sat on an old `origin/main`. Both were
caught only by human intervention, and the coordinator caught one by
hand-running `gh pr diff` in its read-only `bash`, not via any designed
tool.

Three distinct harness gaps fell out of that session:

1. **No rebase-before-PR / rebase-before-merge gate.** `workspace_scm_status`
   reports ahead/behind _upstream_, not behind the default branch, and
   nothing flags "this diff reverts files the worker didn't write".
2. **No fleet inventory.** The coordinator couldn't see its own workers
   without polling `observe_worker` per id, so it hand-maintained a
   `todo_write` list that could drift from the real registry — and its
   wake messages didn't say how many workers were still going.
3. **Opaque worker ids.** Dispatch packets and tool handles read
   `sess-<digits>-<hex>`, unreadable to both the coordinator and the
   human watching it.
4. **A disconnected worker was a black box.** The disconnect notice said
   "the user owns it now" but nothing about whether its branch was
   committed / pushed / stale, so the coordinator had to remember to
   audit it.

## Decision

### `check_worker_base` — the stale-base tripwire

A new coordinator tool. Resolves the worker's worktree, runs a new
`WorkspaceHost::git_base_check` (`git fetch`, then `behind`-count vs the
default branch + `git diff <ref>...HEAD --numstat`), and
cross-references the diff's deleted files against the files the worker
touched on its last turn (`last_turn_diff`). A path with `deletions > 0`
the worker never edited is the **revert suspect** — merging / PR-ing the
branch would re-delete work that landed on the default after its base.

The verdict is a **heuristic, not proof**: `last_turn_diff` only covers
the latest turn, so a worker that legitimately edited a file several
turns back reads as "didn't touch it". The tool says so in its
`verdict_basis` field rather than overstating certainty. The three-dot
diff is the right lens regardless: it shows what the _branch_ did, so a
deletion on an untouched file is genuinely suspicious.

`git_base_check` fetches first because the whole point is catching drift
against the _live_ default, not a stale local ref; a failed fetch
(offline, local-only repo) downgrades to the local ref or to `None`
(local-only repos have no default to drift from). New protocol types
`GitBaseCheck` / `GitBaseCheckFile` (ts-rs exported).

### `list_workers` — fleet ground truth

A new coordinator tool returning one `WorkerSnapshot` per registered
worker (title, branch, running / idle / needs-input, attached state,
`behind_default`) plus attached / disconnected counts. It reads the same
in-memory `CoordinatorRegistry` the dispatch feeder uses, so it can't
drift the way a hand-maintained todo list can. This is what makes the
"re-triage every in-flight worker after a merge" loop from the field
report a single call.

### Fleet-count wake messages

The dispatch feeder's `TurnComplete` wake now leads with the worker's
name and appends the live attached count ("(2 workers still on your
fleet — `list_workers` for the full picture)"), so the coordinator knows
when the fleet has drained without polling.

### Named worker session ids

A worker spawned with a `name` (ADR 0042) now gets a session id that
embeds it: `sess-<slug>-<ts>-<rand>` instead of `sess-<ts>-<rand>`. The
id stays unique (timestamp + random suffix) and remains the **stable
tool / registry key** — tools still key off it, not the mutable title.
The embedded name is purely for human and LLM readability of dispatch
packets, `list_workers` rows, and a `ls` of the sessions directory. The
slug is re-sanitised to `[a-z0-9-]` (it's already the git branch slug,
so it's safe) and capped; a slug that sanitises to nothing falls back to
the plain id. Nothing parses the id structurally — sessions are listed
by `*.jsonl` glob and sorted by the header's `updated_at_ms`, and the
only prefix consumer is `sub-` id minting, which is agnostic to the
inner shape. Per AGENTS.md "no premature migrations", this is additive;
old timestamp ids keep working.

### Disconnect notice carries a branch snapshot

Both disconnect paths (idle and after-turn) now append a one-line
snapshot to the notice: branch, ahead / behind upstream, uncommitted
file count, and drift behind the default. The handover is no longer a
black box the coordinator has to remember to audit. Wake and disconnect
messages also lead with the worker's **name** (`worker_label`) rather
than the opaque id.

## Consequences

- The stale-base failure mode gets a designed gate instead of a lucky
  catch: run `check_worker_base` before `merge_worker_changes` / before
  opening or updating a PR, and re-check the fleet after each merge.
- The coordinator's awareness is grounded in the registry, not
  self-reported todos.
- Coordinator-facing payloads are human-readable (names first, ids as
  keys).
- None of this removes the human's judgement: `check_worker_base` flags
  and recommends a rebase; it does not auto-rebase (a conflicted
  auto-rebase in a worker's worktree is its own mess, and the
  coordinator is deliberately not given a rebase tool yet).

## Alternatives considered

- **Auto-rebase a stale worker.** Rejected: a conflicted rebase strands
  the worker's worktree mid-rebase with no coordinator affordance to
  resolve it. Flag-and-steer is safer; a designed `rebase_worker_branch`
  tool is a follow-up once real usage shows the steer-to-rebase loop is
  too clunky.
- **A "default branch advanced" dispatch event** (push-based re-triage
  instead of `list_workers`-pull). Rejected for now as the bigger lift —
  it needs the IDE to detect ref moves and feed a coordinator wake. The
  `behind_default` field on `list_workers` rows gets the same
  information with one cheap pull.
- **Rename the id scheme globally** (drop the timestamp). Rejected: the
  timestamp is what guarantees collision-free ids across concurrent
  folders / processes without coordination, and the id is the
  transcript filename. Embedding the name gets the readability without
  touching uniqueness.
- **Key tools off the worker's name instead of the id.** Rejected: names
  are mutable (auto-rename, user edits); the registry / feeder / event
  routing need a stable key. Name is a label, id is the key.

## Related

- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md)
  — branch-as-deliverable; the isolation this instruments.
- [ADR 0030 — orchestrator sessions](0030-orchestrator-sessions.md) —
  the coordinator / worker / registry model these tools extend.
- [ADR 0042 — `spawn_worker` names the worker's branch](0042-named-worker-branches.md)
  — the `name` the session id now also embeds.
- [ADR 0052 — disconnect a worker](0052-disconnect-worker-from-coordinator.md)
  — the handover the branch snapshot enriches.
- [ADR 0055 — merge aborts on conflict](0055-merge-aborts-on-conflict.md)
  — the sibling fix for when a stale merge is attempted anyway.
