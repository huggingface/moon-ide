# ADR 0030 — Orchestrator sessions: an agent as a client of the coder surface

Date: 2026-07-10
Status: accepted; first implementation cut landed. The coordinator
mode, the `mode` header field, the `spawn_worker` / `observe_worker` /
`steer_worker` / `abort_worker` / `respond_to_worker_prompt` tools, the
by-id client surface (`send_to` / `abort_session` / `observe_session`),
and the worktree-creation promotion (`create_worktree_session`) are
implemented and tested. Not yet on a phase roadmap — this is the first
concrete realization of the design. The [companion app](0023-mobile-
companion-bridge.md) is still the forcing function for the broader
"coder surface as a client surface" cleanup; the orchestrator reuses
the `CoderHandle` method set directly (in-process, not over WSS).

The events-as-messages feeder is also landed: a background task
subscribes to the coder event broadcast, filters for the orchestrator's
workers, builds a dispatch packet on `TurnComplete`, and feeds it into
the orchestrator's session via `send_to` — waking the orchestrator's
LLM loop. The existing steer queue was already multi-entry FIFO with
batched drain (the ADR's premise that it was one-at-a-time was stale),
so only the feeder task itself was new. See [§ Orchestrator-specific
mechanics](#orchestrator-specific-mechanics-the-genuinely-new-bits).

## Context

The coder panel today is strictly hierarchical and one-shot at the
delegation level. From
[`coder.md` § Sub-agents](../coder.md#sub-agents):

- The `task` tool dispatches a **sub-agent** that runs to completion (or
  budget / cancel) and **returns one string**. The parent's tool call
  _blocks on it_. Multiple `task` calls in one assistant message run in
  parallel (4-permit semaphore) but each still blocks its caller.
- Sub-agents are **not top-level sessions**: their JSONLs live under
  `<sessions-dir>/<parent-folder-slug>/<parent-session-id>/<sub-id>.jsonl`,
  they are **excluded from the session list** (`list_sessions` reads the
  slug dir flat, so only `sess-*` shows), and they have no composer /
  abort / steer / revert affordances of their own.
- Depth is capped at **1** (`task` is omitted from the sub-agent's own
  tool list). "Detached / background sub-agents" and "per-sub-agent
  abort UI" are explicitly
  [out of scope](../coder.md#out-of-scope-explicitly).
- The `ask_user` tool is **parent-only**; sub-agents have no panel to
  answer through, so they can't even ask.

Worktree sessions
([ADR 0028](0028-coder-worktree-sessions.md),
[0029](0029-worktrees-inside-parent.md)) already let a _user_ opt a
session into its own git worktree so several agents work one project at
once and each lands an independent branch / PR. Worktree creation is a
Tauri command (`coder_new_worktree_session`) driven by a UI button;
**it is not exposed to the agent as a tool.** ADR 0028's own "future
direction" note anticipates "lean on worktrees automatically once
multiple agents run concurrently."

The [companion app](../companion.md) (planned, not built) is the
forcing function that turns the coder's session/run/steer/abort/observe
surface into a **clean JSON-RPC client surface, not something coupled to
the webview**. Its v1 scope — "subscribe to `coder:event`, render the
transcript, `coder_send` (send / steer), `coder_abort`; session list /
open / new reuse the existing `coder_*` commands" — is a near-exact
requirements list for what an in-process agent would need to drive a
peer session. The companion spec's
["Cloud / always-on future"](../companion.md#cloud--always-on-future)
already names the boundary this work shares: "the loop must stay owned
by `moon-core`, not by a UI lifetime," and "detached / overnight runs
need the loop re-attachable across client connect/disconnect; the
constraint for now is simply **don't deepen the loop ↔ process
coupling**."

### The use case

> I tell an agent to look at the GitHub issues & open PRs for a repo,
> it opens new agents in worktrees, interacts with them until each PR
> is opened. Those sessions look like ones I opened — I can take over
> / follow up from the agent.

Two traits make this a different shape from today's `task`:

1. **Spawn returns a handle, not a result.** The orchestrator does not
   block on a worker; the worker keeps running. The orchestrator
   decides when (or whether) to attend.
2. **Workers are top-level sessions, not sub-agent transcripts.** They
   live in the per-project session list, have their own composer /
   abort / steer, and — crucially — the user can open them mid-run and
   take over.

## Decision

An **orchestrator** is a top-level coder session in a new `coordinator`
mode (see [§ Fork 1](#fork-1--orchestrator-tool-surface-pure-coordinator--read-only-inspection))
that can **spawn, observe, and interact with peer top-level sessions**
("workers"). Workers are themselves ordinary `agent`-mode coder sessions
in worktrees. The sub-agent layer (`task`, depth-1 cap, synchronous-
block, returns-one-string, hidden from the session list) is
**unchanged** — workers may still spawn sub-agents under today's rules.

### The orchestrator is an in-process client of the coder surface

The reusable work is _not_ a parallel protocol. It is the same
client surface the companion app is already forcing into existence:

| Orchestrator needs            | Companion needs (now or later)                                                                    | Today's command              |
| ----------------------------- | ------------------------------------------------------------------------------------------------- | ---------------------------- |
| Create a worker in a worktree | (later — not v1, but same need)                                                                   | `coder_new_worktree_session` |
| Seed the worker with a prompt | "send a prompt to the active session" (v1)                                                        | `coder_send`                 |
| Observe worker state mid-run  | "subscribe to `coder:event`, render transcript" (v1)                                              | `coder:event` stream         |
| Steer a worker                | `coder_send` (steer) (v1)                                                                         | `coder_send`                 |
| Abort a worker                | `coder_abort` (v1)                                                                                | `coder_abort`                |
| Answer a worker's `ask_user`  | answer a parked prompt (not in companion v1 scope today, but implied by "interact like moon-ide") | `coder_respond_to_prompt`    |

The two genuinely orchestrator-specific mechanics — the ones the
companion does _not_ justify because it renders events in a UI rather
than feeding them to an LLM — are isolated below.

### Workers are ordinary top-level sessions

A worker is a top-level coder session in a worktree, created by
`coder_new_worktree_session`. Concretely this means:

- It is **already detached**: per
  [ADR 0016](0016-coder-concurrent-sessions.md) a turn is a spawned
  task closing over an `Arc<SessionRuntime>`, so it runs whether or
  not its session is the visible one, and concurrent turns per folder
  already work. The "worker runs while the orchestrator isn't blocking
  on it" property is already true. **Nothing new.**
- It **lives in the per-project session list** beside sessions the user
  started by hand. The user opens it mid-run and takes over through the
  normal composer / abort / steer / revert. "Take over" is not a
  special claim gesture; it is just _opening the session_.
- It **inherits the worktree machinery unchanged**: worktree-as-bound-
  folder, branch-as-deliverable, per-project session list, the
  move-into-worktree button — all already built.
- It **can spawn sub-agents with `task`** under exactly today's rules
  (depth-1, synchronous, returns a string, hidden from the list). The
  orchestrator does not change the sub-agent layer.

### The coder surface becomes a client surface

Three promotions, each of which the companion also benefits from:

1. **`coder_new_worktree_session`** moves from a UI-only Tauri command
   to a client-callable method. The orchestrator needs it now; the
   companion needs it later (its v1 reviews/steers existing sessions
   but will want to mint isolated ones eventually). Same work, two
   consumers.
2. **`coder_respond_to_prompt`** is a client-callable method on every
   session's parked prompts, not just the desktop UI's. A worker's
   `ask_user` must be answerable by **whichever client is attending
   the session** — the desktop, the phone, or the orchestrator. The
   companion's "interact with coder sessions like moon-ide" posture
   already implies this; today's companion v1 scope doesn't list
   `coder_respond_to_prompt` and should.
3. The **`coder:event` stream** is already envelope-wrapped
   `{ folder, session_id, event }` and already consumed by a
   non-webview client (the companion renders it over WSS). The
   orchestrator is a third consumer in-process.

### Orchestrator-specific mechanics (the genuinely new bits)

**(a) Events-as-messages — workers wake the orchestrator's LLM loop.**
The companion renders `coder:event` in a UI; the orchestrator feeds
selected worker events into its _own_ LLM loop as dispatch packets.
The orchestrator is an **idle session fed by a dispatch queue** whose
inputs are (i) the composer (you) and (ii) worker events
(`ask_user` raised, `turn_complete`, PR opened, stuck). Each wake
runs one orchestrator turn to respond, then goes idle again. "Passive
until needed by one of the workers" is not a mode to build — it is the
default idle-between-turns state of any session, with a second feeder
added to its input queue.

Each wake delivers a **self-contained dispatch packet** about one
worker — task, branch, turns-so-far, the question or state change, a
recent transcript tail. The orchestrator answers from the packet +
its plan, **not from the workers' transcripts**. Its own context holds
only the **meta-plan** (the issue list, the strategy, which worker
addresses which issue) and a **rolling dispatch log** (recent wakes,
so it remembers "I already told #3 to skip the e2e tests"). This is
the `task` tool's "describe the task self-containedly, the sub-agent
doesn't see the parent's transcript" discipline, applied in the other
direction. Auto-compaction handles the plan + log growing; per-worker
detail is ephemeral per-wake and does not accumulate. A single
orchestrator can juggle a handful of workers this way _because it
isn't holding their contexts_.

**(b) A multi-entry, batchable dispatch queue.** Today's steer queue
is [one-at-a-time](../coder.md#loop-shape): a second queued steer
replaces the first with a toast. That assumes one human typing. For
an orchestrator fed by N workers, if workers #2 and #4 fire events
while the orchestrator is mid-turn, **both** queue (rather than #4
clobbering #2), and the orchestrator drains them — ideally **batched
into one wake** ("#2 asked X, #4 finished Y") rather than two serial
turns, since handling them together is where any cross-worker
reasoning ("#4's PR obsoletes #2's, tell #2 to stop") would happen.

This queue change is **orchestrator-scoped**, not a general change to
every session's steer queue. Ordinary sessions keep the one-at-a-time
model; the orchestrator opts into multi-entry batching. (Whether the
two are one queue with a mode flag or two distinct queue kinds is an
implementation detail for the first cut.)

### The depth cap is untouched

This is worth stating explicitly because it's the easiest thing to
misread. The `task` depth-1 cap guards the **delegation** axis
(parent → sub-agent → returns-a-string → done). Sub-orchestrators do
_not_ live on that axis. A sub-orchestrator is a **peer** (a
top-level session) spawned by the orchestrator the same way the
orchestrator spawns workers — it's just a worker whose job happens to
be "coordinate these other workers." So:

- The depth-1 `task` cap **stays exactly as is.** Nothing about this
  ADR lifts it, and nothing should.
- **Sub-orchestrators are permitted by the design** (a worker can be a
  coordinator — it inherits `spawn_worker` like any worker) but are a
  **scale escape valve, not a v1 requirement.** For the use case that
  motivated this (read issues, open PRs — 3-10 workers, intermittent
  attention each), one orchestrator with self-contained dispatches is
  sufficient. Sub-orchestrators earn their keep when N gets large or
  when workers cluster into groups needing sustained coordinator
  attention (e.g. "these 4 issues are all about one subsystem, one
  coordinator should hold that context"). The architecture permits
  them without requiring them at v1.

## Resolved v1 forks

### Fork 1 — orchestrator tool surface: pure coordinator + read-only inspection

The orchestrator is a **pure coordinator** in the spirit of option (B)
above — no `write_file` / `edit_file`, it must delegate all mutation —
but it keeps **read-only inspection tools** so it can reason about what
its workers are doing without polluting its context with their
transcripts:

- `read_file`, `list_dir`, `grep` — for codebase context when planning
  or answering a worker's question.
- `bash` (read-only intent, `research`-mode semantics) and `web_fetch` /
  `web_search` — for issue discovery (`gh issue list`), looking at PRs,
  reading docs.
- `spawn_worker` / `observe_worker` / `steer_worker` / `abort_worker` /
  `respond_to_worker_prompt` — the coordinator tools.

It is a distinct session kind, not "an `agent`-mode session with
coordinator tools appended." Today the spec says "top-level sessions are
always `agent`; mode is a sub-agent concept" — this ADR adds one top-
level mode (`coordinator`) alongside `agent`. The tool-list shape
enforces the posture (the model _can't_ edit, so it _must_ delegate),
which is the behaviour the use case wants. A regular `agent` session
that happens to be doing coordination (option (A)) is still allowed —
nothing prevents a user from running an `agent` session and using
`spawn_worker` if that surfaces as a need — but the orchestrator the
use case points at is `coordinator`.

This adds a `coordinator` mode the spec doesn't have today. We accept
the new concept rather than overload `agent` because the whole point is
to _prevent_ the model from doing the work itself, and tool-list shape
is the reliable enforcement (prompt-nudges aren't).

### Supporting direction: per-turn diffs (not a v1 commitment)

A pure coordinator that can't read worker transcripts still needs to
_review what a worker changed_ — a diff, compact and current, not a
stream of `tool_call` rows. This motivates a separate, smaller idea:
**store per-turn diffs** — the working-tree diff attributable to one
agent turn, persisted alongside the JSONL or derived from it.

Two consumers, same storage:

- The **orchestrator** gets "what did worker #2's last turn change?"
  as a compact artifact in a dispatch packet, instead of either (a)
  reading the worker's full transcript or (b) holding the worktree's
  state in its own context.
- The **IDE** gets a per-turn diff review surface ("show me what the
  agent changed this turn"), which is independently useful for any
  session — you review an agent run turn-by-turn the way you'd review
  a colleague's commits.

This is a supporting direction, **not a v1 commitment.** It's captured
here because it's what makes the pure-coordinator posture viable for
review, and because the "same storage, two consumers" pattern is
consistent with the rest of this ADR. Shape, persistence, and whether
it lands before or with the first orchestrator cut stay open.

### Fork 2 — "take over": the orchestrator manages what it spawned; edge cases deferred

v1 scoping, not a concurrency rule:

- The orchestrator **only coordinates the sessions it spawned.** It
  does not react to sessions it didn't create (other top-level
  sessions, sessions the user opened by hand, sessions another
  orchestrator spawned). Its dispatch queue is fed by its own workers
  only.
- The human (or the companion) can still **open a worker as a normal
  active session** and stop / steer / answer its `ask_user` through the
  existing surfaces — **we don't handle the multi-client edge cases
  correctly for v1.** If both the orchestrator and the user steer the
  same worker, last-writer-wins-ish, no claim protocol, no
  "foreground client" arbitration. The orchestrator doesn't detect
  or react to the human's intervention.
- This is explicitly **deferred, not solved.** The full multi-client-
  same-session concurrency question (orchestrator + desktop + phone
  all attending one worker) is the companion app's question and gets
  resolved once, in the companion's terms, later. The one asymmetry —
  the orchestrator is an _autonomous_ client, so "it steered a worker
  I was looking at" is more surprising than "my phone steered a
  session my desktop had open" — may eventually justify a small
  affordance (a toast when an agent steers the session you're
  viewing), but not for v1.

The practical effect: for v1, a worker is the orchestrator's to manage
end-to-end, and "take over" is just _you opening the session_ — it
works because the session is ordinary and the existing surfaces still
apply, but the orchestrator carries on oblivious. That's good enough
for the motivating use case (orchestrator drives workers to PRs, you
can look at any of them) and keeps the scope of this ADR small.

## What this deliberately does not do

- **Does not lift the `task` depth cap.** Sub-agents stay depth-1,
  synchronous, returning one string, hidden from the session list.
  Workers spawn sub-agents under today's rules unchanged.
- **Does not make workers a special session kind.** A worker is an
  ordinary top-level session in a worktree. The only thing that makes
  it a "worker" is that the orchestrator spawned it and holds a handle.
  Open it yourself and it's just another session. (The orchestrator
  _is_ a new top-level mode — `coordinator` — per Fork 1 above. That's
  a mode on the _spawning_ side, not a kind on the _spawned_ side:
  workers are plain `agent` sessions in worktrees.)
- **Does not solve multi-client concurrency on a worker for v1.** If
  the orchestrator and the human (or the phone) both steer the same
  worker, behaviour is last-writer-wins-ish with no arbitration; the
  orchestrator doesn't detect or react to the human's intervention.
  See [§ Fork 2](#fork-2--take-over-the-orchestrator-manages-what-it-spawned-edge-cases-deferred).
  Resolved later, in the companion's terms, once.
- **Does not solve cross-restart survival.** "Run until each PR is
  opened" may outlive a single IDE session. The companion spec already
  names this boundary: detached / overnight runs need an always-on
  headless `moon-core` clients attach to. This ADR inherits that
  constraint — the orchestrator's workers die on process restart the
  same way any top-level session's in-flight turn does today — and
  does not attempt to solve it. The guardrail ("don't deepen the
  loop ↔ process coupling") binds here too.
- **Does not require sub-orchestrators at v1.** Permitted by the
  design (a worker can be a coordinator), not required.

## Prerequisites (now landed)

The first implementation cut landed these, so the "gating" framing
is now historical:

1. **The companion app** turning the coder surface into a clean client
   surface — still not built. The orchestrator cut sidesteps this by
   calling `CoderHandle` methods directly (in-process), not over WSS.
   The companion remains the forcing function for the broader surface
   cleanup (the `BridgeRpc` handler is the existing precedent for a
   non-Tauri in-process consumer).
2. **`coder_new_worktree_session` as a client-callable method** —
   **landed** as `CoderHandle::create_worktree_session(base_branch,
mode)`. The Tauri command is now a thin wrapper over it.
3. **`coder_respond_to_prompt` as a client-callable method** —
   **already was** client-callable (`CoderHandle::respond_to_prompt`
   scans all folders by `call_id`, not just the visible session). The
   orchestrator's `respond_to_worker_prompt` tool composes it with
   the new `observe_session` + `pending_call_id` to discover the call
   id by worker id.

## Alternatives considered

- **Lift the `task` depth cap and let sub-agents recurse.** This is
  the obvious "make `task` return a handle instead of blocking" path.
  Rejected: it reopens the recursive-explosion problem the depth cap
  exists to prevent, and it conflates the _delegation_ axis (parent →
  junior) with the _coordinator_ axis (orchestrator → peer). The two
  axes have different invariants — a peer is a real session the user
  can take over; a sub-agent is a hidden blocking call — and should
  stay separate.
- **A bespoke "orchestrator ↔ worker" protocol.** Rejected: the
  companion is already producing the abstraction (coder surface as
  client surface), and building a second one would double the surface
  area and re-couple the loop to a new protocol instead of to the
  webview. The orchestrator is _one more client_ of the surface the
  companion forces into existence.
- **Workers as sub-agents, just detached and in the session list.**
  Rejected: sub-agents carry depth-1, no-`ask_user`, parent-abort-
  cascades, returns-a-string semantics that are all wrong for a peer
  the user can take over. Making workers ordinary top-level sessions
  reuses the worktree + ADR 0016 machinery wholesale instead of
  bending the sub-agent layer into a shape it wasn't designed for.

## Related

- [ADR 0016 — concurrent coder sessions](0016-coder-concurrent-sessions.md)
  — the per-folder multi-session model that already makes workers
  detached. The "worker runs while the orchestrator isn't blocking on
  it" property comes from here.
- [ADR 0028 — worktree-backed coder sessions](0028-coder-worktree-sessions.md)
  / [ADR 0029 — worktrees inside the parent](0029-worktrees-inside-parent.md)
  — the worktree-as-bound-folder, branch-as-deliverable, per-project
  session list machinery workers reuse unchanged. ADR 0028's "future
  direction" note ("lean on worktrees automatically once multiple
  agents run concurrently") anticipates this ADR.
- [ADR 0023 — mobile companion via `moon-bridge`](0023-mobile-companion-bridge.md)
  / [`specs/companion.md`](../companion.md) — the forcing function that
  turns the coder surface into a client surface. The orchestrator is
  the first in-process client of that surface; the cloud / always-on
  future and the cross-restart constraint are inherited from here.
- [`specs/coder.md` § Sub-agents](../coder.md#sub-agents) — the layer
  this ADR deliberately leaves unchanged.
- [`specs/coder.md` § Worktree sessions](../coder.md#worktree-sessions)
  — the machinery workers reuse.
- [`specs/coder.md` § Out of scope](../coder.md#out-of-scope-explicitly)
  — "background detached sub-agents," "per-sub-agent abort UI," and
  "detached / cross-restart agent runs" are the three explicitly
  deferred items this ADR revisits; the first two are reframed (workers
  aren't sub-agents), the third is inherited as a gate.
