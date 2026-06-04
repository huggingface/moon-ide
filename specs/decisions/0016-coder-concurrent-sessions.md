# ADR 0016 — Concurrent coder sessions per folder

Date: 2026-05-25
Status: accepted; supersedes the "one running turn per folder" language
in [coder.md § Multi-session per project](../coder.md#multi-session-per-project)
and the matching observation in
[test plan 0079](../test-plans/0079-coder-host-paths-and-task-rename.md).

## Context

Today every bound workspace folder has exactly one
[`FolderSession { session: Mutex<Session>, turn: Mutex<TurnState> }`](../../crates/moon-coder/src/runner.rs).
The runner contract has been "one running turn per folder": when the
user clicks `+` for a new session or picks another session from the
sessions list, [`new_session`](../../crates/moon-coder/src/runner.rs)
and [`open_session`](../../crates/moon-coder/src/runner.rs) **cancel**
the active folder's in-flight turn before swapping the in-memory
`Session` out. Without the cancel, the still-running `run_turn` would
keep writing assistant deltas and tool results into the brand-new
blank `Session` (and into the wrong JSONL on disk), corrupting both
sessions.

The user-visible failure mode that motivated this ADR: starting a
long turn in folder X, then clicking `+` to start a quick second
question — the first turn gets silently aborted. The user expects to
juggle several concurrent agent turns in the same project the way
they already juggle one per folder.

Cross-folder concurrency is already supported (the per-folder bucket
is exactly that). The change here is **within** a folder.

## Decision

Per-folder runtime grows from "one session + one turn" to "many
sessions, each with its own turn". `FolderSession` becomes a holder
of `SessionRuntime` entries keyed by session id:

```rust
struct FolderSession {
    runtimes: RwLock<HashMap<String, Arc<SessionRuntime>>>,
    visible: RwLock<Option<String>>, // session id the panel is mounted on
}

struct SessionRuntime {
    session: Mutex<Session>,
    turn: Mutex<TurnState>,
}
```

Every spawned task (`run_turn`, `run_subagent`, `spawn_auto_rename`,
`maybe_autosync_to_hub`) closes over an `Arc<SessionRuntime>` instead
of `Arc<FolderSession>`. `new_session` / `open_session` allocate /
look up a runtime, set it as visible, and **never touch other
runtimes' cancel tokens**. `delete_session` removes the runtime
entry; if it was running, the cancel token fires first.

`abort` operates on the **visible session's** turn only — same
semantics as today, just one extra hop. Stopping a background turn
requires switching to it first (clicking the row in the sessions
list). `coder_sign_out` remains the global escape hatch and cancels
every runtime in every folder.

`send` still targets the visible session. A `send` while the visible
session's turn is running is a steer, exactly as today. A `send` on
a freshly-created session (made visible by `+`) starts a new turn
even while another session in the same folder is mid-flight.

### Wire shape

`CoderEventEnvelope` grows a `session_id: String` field:

```rust
pub struct CoderEventEnvelope {
    pub folder: String,
    pub session_id: String,
    pub event: CoderEvent,
}
```

The frontend routes events by `(folder, session_id)` instead of just
`folder`. Events that genuinely don't belong to a session
(`folder_summary_ready`, `hub_sync_started` / `hub_sync_finished`)
use an empty `session_id` and the dispatcher falls through to a
folder-scoped handler. Empty-string sentinel rather than
`Option<String>` keeps the TS mirror simple and the serde shape
non-optional on the hot path; the small handful of folder-scoped
event variants are documented inline.

This is a breaking wire-format change; `moon_protocol::PROTOCOL_VERSION`
bumps. Per [AGENTS.md § No premature migrations](../../AGENTS.md), no
compatibility shims for old in-flight clients — they'd just be talking
to themselves anyway since both ends update together.

### Frontend state

`FolderViewState` splits along a session-scoped / folder-scoped seam:

- **Session-scoped** (move under a new `SessionRuntimeState` keyed by
  session id): `rows`, `busy`, `subagentSummaries`,
  `subagentTranscripts`, `viewSubagentId`, `tokenUsage`, `compaction`,
  `todos`, `activeSession`, `draft`, `attachments`. Each running
  session keeps its own transcript, busy pip, context ring, todo
  list, and composer state — switching sessions in the same folder
  doesn't wipe what the user typed into the other one's composer.
- **Folder-scoped** (stay on `FolderViewState`): `sessions` (list of
  persisted sessions on disk under the folder slug), `view`
  (`'list' | 'session' | 'subagent'` — the panel-level view selector),
  `attentionPending` (rolled up across all sessions in the folder so
  the folder-bar sparkle still works as a "something here finished"
  signal).

The proxy getters on `Coder` (`coder.rows`, `coder.busy`, …) extend
from `coder.current.X` to `coder.current.visibleRuntime.X` for
session-scoped fields. Components that read them stay untouched.

`bucketFor(folder)` gains a sibling `runtimeFor(folder, sessionId)`
the event dispatcher uses to route incoming envelopes. Lazy-create
on first event for an unknown session id keeps the cold path simple
and matches how `bucketFor` already works.

### UI consequences

- The sessions list grows a per-row "running" pip — pulsing accent
  dot + `running…` label — for every session id present in the
  folder's running-runtime set, not just the visible one. Test plan
  0079 already pictured this for the cross-folder case (its
  observation that "only the active folder's session list shows pips"
  becomes "every running session in any folder shows a pip").
- The folder-bar pip / `attentionPending` sparkle stays as a
  folder-level rollup. A user juggling three running agents in
  folder X still sees one folder-bar pip; the granularity drops to
  the sessions list inside that folder.

> **Addendum (per-session "finished" marker).** The folder-scoped
> `attentionPending` rollup above answers "did anything here
> finish?" but can't say _which_ session. `SessionViewState` later
> gained its own per-session `attentionPending` flag for exactly
> the same triage need the running pip serves: a session whose turn
> ends while the user is following a _different_ session paints a
> static amber `finished` marker on its row (mirrors the folder-bar
> `.done` hue, no pulse). Set on `turn_complete` / `aborted` /
> `error` when the session isn't the one being followed; cleared on
> `openSession` and on `setActiveFolder` when the folder's visible
> session is the finished one. The folder-level rollup is unchanged
> — this is the per-row counterpart, same as the running pip is the
> per-row counterpart of `busyForFolder`.

- The composer draft / attachments become per-session. The Phase 6
  multi-session work made them per-folder; per-session is the
  natural next step now that the user can hold multiple in-flight
  conversations in the same folder.

### What `last_session_by_folder` means after this change

`AppState.coder.last_session_by_folder` already records "the most
recently opened/sent session per folder" — exactly the visible-session
pointer we need at restore time. No schema change. On launch the
panel restores each folder to its last-visible session; other
sessions are not re-hydrated as runtimes until the user opens them,
because there's no in-flight turn to preserve across a process
restart.

## Consequences

- Each `SessionRuntime` is cheap (a `Mutex<Session>` + a `Mutex<TurnState>`).
  Memory cost is in the rebuilt `messages: Vec<ChatMessage>` per
  opened session, which is the same per-session cost we had before;
  we just no longer drop it on session switch. Practical cap is
  whatever the user opens by hand — no concurrency limit imposed at
  this layer. Provider rate limits throttle the network side.
- `abort`'s "visible session only" semantics mean stopping a
  background turn is a two-click operation (open it, hit stop). Fine
  for the volume of concurrent turns we expect; a per-row stop
  button is a future polish, not a launch requirement.
- The per-session JSONL files already exist on disk — every session
  is its own file, and the runner already binds the runtime to its
  own `session_dir`. No on-disk shape changes.
- The breaking protocol bump matches the
  [no-premature-migrations](../../AGENTS.md) clause: pre-stable
  schema, dev-only installed base, both sides update together.

## Alternatives considered

- **Refuse `new_session` / `open_session` while a turn is running**
  (the "Option A" we considered before settling on this ADR). One-
  line backend change plus a toast. Doesn't deliver the capability
  the user actually asked for, and trains the user to wait when the
  whole point of background turns is "don't have to wait".
- **Per-session abort buttons in the row.** Possible later. Not
  worth the extra UI surface until the volume of concurrent agents
  justifies it; the open-then-stop gesture is two clicks and
  matches the existing per-folder model.
- **Keep composer draft per-folder.** Tempting for the same reason
  cross-folder drafts are per-folder (folder hop, come back to your
  half-typed prompt). But once two sessions in the same folder can
  both have half-typed prompts, conflating them collapses real
  user intent. Per-session draft is the only consistent answer.

## Related

- [specs/coder.md § Multi-session per project](../coder.md#multi-session-per-project) —
  rewritten in the same commit to match this ADR.
- [test plan 0085 — concurrent sessions per folder](../test-plans/0085-coder-concurrent-sessions.md) —
  ships alongside.
- [test plan 0079 — coder host paths + task rename](../test-plans/0079-coder-host-paths-and-task-rename.md) —
  the "one running turn per project" observation in its dev notes is
  superseded by this ADR.
- [ADR 0010 — coder rewrite not ACP](0010-coder-rewrite-not-acp.md) —
  the "we own the loop" stance that makes a refactor at this layer
  cheap.
