# ADR 0030 — Orchestrator sessions: an agent as a client of the coder surface

Date: 2026-07-10
Status: proposed; design only, no code yet. Not on a phase roadmap. This
ADR records the shape we converged on so the next conversation starts
from it. It anticipates, and is gated on, two things that are also not
built yet: the [companion app](0023-mobile-companion-bridge.md) turning
the coder surface into a clean client surface, and
[`coder_new_worktree_session`](../coder.md#worktree-sessions) being
promoted from a UI-only Tauri command to a client-callable method.

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

An **orchestrator** is an ordinary `agent`-mode top-level coder session
that can **spawn, observe, and interact with peer top-level sessions**
("workers"), which are themselves ordinary coder sessions in worktrees.
The sub-agent layer (`task`, depth-1 cap, synchronous-block, returns-one-
string, hidden from the session list) is **unchanged** — workers may
still spawn sub-agents under today's rules.

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

## Two design forks left open

These shape the implementation but don't change the shape above; both
want a deliberate call before the first cut.

1. **What tools does the orchestrator have?** Two clean ends:
   - **(A) Regular agent + coordinator tools.** The orchestrator is an
     `agent`-mode top-level session with `spawn_worker` /
     `observe_worker` / `steer_worker` / `respond_to_worker_prompt`
     appended to its tool list, the way `task` and `ask_user` are
     appended to a parent's list today. It keeps `read_file` / `bash`
     / `edit_file` etc., so it can fetch issues with `bash` (`gh`) or
     `web_fetch` itself and even step in and edit directly. Simplest;
     one session kind; the model _can_ do work itself. Risk: the model
     does the work itself instead of delegating, and the tool list is
     large.
   - **(B) Pure coordinator.** A distinct session kind with only the
     worker-management tools (+ `bash` / `web_fetch` for issue
     discovery, maybe `read_file`). It _cannot_ edit files directly —
     it must delegate. Forces the delegation posture the use case
     wants, at the cost of a new "kind" concept the spec currently
     doesn't have (today: "top-level sessions are always `agent`;
     mode is a sub-agent concept").

   The existing design's spirit leans (A) — the `task` precedent is
   "append to the parent's tool list," not "a new session kind." But
   the _behaviour_ the use case wants (it delegates rather than doing)
   leans (B).

2. **"Take over" and multi-client concurrency on one session.** Once a
   worker is a real top-level session, two clients (you + the
   orchestrator, and potentially the phone _too_) can all send to /
   abort / answer the same session. The companion app raises the
   identical question (phone + desktop, same session). So this is not
   a new problem this ADR invents — it's the same multi-client-same-
   session question, and whatever resolution the companion lands on
   (first-writer-wins? "foreground client" claim? last-writer-wins
   with event echo?) will bind the orchestrator too. Resolve it once,
   in the companion's terms, not twice.

   The one asymmetry worth noting: the orchestrator is an
   _autonomous_ client, so "it steered a worker I was looking at" is
   more surprising than "my phone steered a session my desktop had
   open." That may justify a small affordance — a toast when an agent
   steers the session you're viewing — but the underlying concurrency
   rule is shared with the companion.

## What this deliberately does not do

- **Does not lift the `task` depth cap.** Sub-agents stay depth-1,
  synchronous, returning one string, hidden from the session list.
  Workers spawn sub-agents under today's rules unchanged.
- **Does not make workers a special session kind.** A worker is an
  ordinary top-level session in a worktree. The only thing that makes
  it a "worker" is that the orchestrator spawned it and holds a handle.
  Open it yourself and it's just another session.
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

## Prerequisites (gating, not blocking)

This ADR is **design-only** and is gated on work that is also not
built yet:

1. **The companion app** turning the coder surface into a clean client
   surface. The orchestrator is the first _in-process_ client of that
   surface; building it before the surface is client-clean would
   re-couple the loop to the webview, which is exactly what the
   companion spec warns against.
2. **`coder_new_worktree_session` as a client-callable method**, not a
   UI-only Tauri command. (Trivial mechanically; the point is the
   _promotion_, not the plumbing.)
3. **`coder_respond_to_prompt` as a client-callable method** on every
   session's parked prompts, not just the desktop's — and the
   companion's v1 scope updated to list it, since "interact with coder
   sessions like moon-ide" already implies it.

None of these are blockers in the sense of "decide first"; they're
gates in the sense of "land before, or alongside, the first
orchestrator cut." The companion is the forcing function for (1) and
most of (3); (2) is a small mechanical promotion.

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
