# Test plan 0050: Multi-project & Sub-agents

- **Date**: 2026-05-06
- **Phase**: 6 (coder) — folds the previously-planned `spawn_subagent` shape into shipped behaviour, adds per-bound-folder context, and wires a parallel-foreground sub-agent UI.

## What shipped

Six-phase delivery, every phase its own commit:

- **Phase A — `ToolContext` refactor.** Extracted `pub struct ToolContext { folder, mode }` + `pub enum CoderMode { Research, Coder }` from [`crates/moon-coder/src/tools.rs`](../../crates/moon-coder/src/tools.rs). `ToolRegistry::dispatch` now takes `&cx`, every tool reads its folder from there. `write_file` / `edit_file` short-circuit with `Err(CoderError::ReadOnlyMode)` when `cx.mode == Research`. `bash` is **not** mode-gated (the read-only constraint lives in the sub-agent's system prompt instead). No user-visible change in this phase.
- **Phase B — Bound-folder summaries.** New [`crates/moon-coder/src/folder_summary.rs`](../../crates/moon-coder/src/folder_summary.rs): reads each bound folder's manifest bundle in canonical order (`AGENTS.md`, `README.md`, `Cargo.toml`, `package.json`, `pyproject.toml`), resolves filenames case-insensitively against the folder's top-level entries, calls the `fast` model, caches at `<XDG_DATA_HOME>/moon-ide/folder-summaries/<slug>.json`, invalidates when any input's bytes change (64-bit FNV-1a). Runner refreshes the parent's system prompt on every turn — adds a "Bound folders" section listing every bound folder with a 2–3 sentence description. Generation kicks off as a detached `tokio::spawn`; the runner never blocks waiting on one. New `folder_summary_ready` event + `coder_folder_summary` Tauri command for the project-bar tooltip.
- **Phase C — Sub-agent runner + parallel dispatch.** New [`crates/moon-coder/src/subagent.rs`](../../crates/moon-coder/src/subagent.rs) with `Subagent`, `ModelTier`, `SubagentReport`, `run_subagent`. `spawn_subagent` tool definition lives here too, deliberately outside `ToolRegistry::definitions()` — it gets appended only to the **parent's** tool list, so sub-agents never see it (depth=1 cap enforced via tool-list shape rather than a runtime check). `run_turn` detects homogeneous `spawn_subagent` batches and dispatches via `tokio::spawn` + `join_all` bounded by `Semaphore::new(4)`; mixed and single-call batches stay sequential. Cancellation cascades via `cancel.child_token()`.
- **Phase D — Sub-agent JSONL persistence.** Sub-agent transcripts at `<XDG_DATA_HOME>/moon-ide/coder-sessions/<parent-folder-slug>/<sub-id>.jsonl` (same slug as the **parent** session's project, not the sub-agent tool target). Header optional fields (`parent_session_id`, `parent_tool_call_id`, `subagent_mode`, `subagent_target_folder` when the target differs) cross-reference the parent and spell out cross-folder ops. Existing on-disk parent sessions stay byte-compatible — the new fields use `serde(default, skip_serializing_if = "Option::is_none")`.
- **Phase E — Frontend.** Mirror types `SubagentMode`, `SubagentSummary`, `SubagentTranscript` in [`src/lib/protocol.ts`](../../src/lib/protocol.ts) and [`src/lib/coder.svelte.ts`](../../src/lib/coder.svelte.ts). Inline collapsed sub-agent card under each `spawn_subagent` tool row in [`src/lib/components/CoderPanel.svelte`](../../src/lib/components/CoderPanel.svelte), with mode badge + folder basename + status pip + result preview + "Open transcript →" button. New `CoderView::'subagent'` branch renders the pop-out via a shared `rowMarkup` snippet so the parent transcript and the sub-agent transcript stay in lockstep.
- **Phase F — Spec sync + this test plan.** Folded `Sub-agents (planned)` into the shipped section of [`specs/coder.md`](../coder.md), added the new tool to the tool table, documented per-folder sessions + `CoderEventEnvelope`, parent-slug sub-agent JSONL placement, AGENTS-first manifest order, replaced the "no implementation" caveat with the real non-goals.
- **Multi-session backend + frontend (post-F):** `CoderState.sessions_by_folder`, `FolderSession`, `run_turn` closes over the session's bound folder; `coder:event` uses `{ folder, event }`; `AppState.coder.last_session_by_folder`; per-folder composer draft/attachments in `coder.svelte.ts`; `abort` stops the **active** folder's turn only.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, signed-in to Hugging Face (per test plan 0039), at least two workspace folders bound (the multi-project surface needs at least two folders to exercise the sub-agent's `folder` argument).

### Sanity: parent session unchanged in the simple case

1. Open a single-folder workspace. Send a plain prompt: "list the top-level files in this repo".
2. Expected: the parent uses `list_dir` against the active folder, returns the listing. Behaviour identical to pre-multi-project. `cargo test -p moon-coder --lib` is the regression backstop.

### Multi-session per folder (background turns + envelope routing)

With two folders **A** and **B** bound:

3. Start a deliberately slow prompt in folder A (e.g. "read every entry in Cargo.toml dependency section and summarise" on a chunky repo).
4. While A is still streaming, switch the active workspace folder to B in the project bar.
5. Expected: the Coder UI shows folder B's session list / transcript / composer state — nothing from folder A overlays it. Optionally open devtools logs: inbound `coder:event` payloads remain tagged with `{ folder }` matching the emitting project.
6. Switch back to A before the turn completes. Expected: A's transcript shows the uninterrupted stream finishing; tools ran against folder A paths the whole time.
7. **Composer drafts:** type `draft-A` into the coder prompt on folder A without sending; switch to B, type `draft-B`; ping-pong folders a few times. Expected: returning to each folder restores exactly that folder's composer text and attachments; no cross-talk.
8. **Abort scope (active folder only):** With a long turn running in folder A again, switch to folder B immediately. Press Stop while B has **no** turn in flight. Expected: returning to folder A afterward, that turn either completed or **still ran** — it must **not** have been cancelled purely because Stop was clicked from B without a matching turn. (**Then** optionally switch back to B, kick off work there so B has an active turn, and confirm Stop terminates B's turn only.)

### Bound folders system prompt

9. Bind a second folder via the project bar / "+ Add folder" if you haven't already from the preceding steps.
10. Send a prompt: "what other folders do you see in this workspace?".
11. Expected: on the **second** turn after binding (the first turn fires summary generation), the parent's response references both folders by name with descriptions derived from each folder's manifest bundle (`AGENTS.md` first when present, then `README.md`, then the other manifests). While the first turn is still running you may see `(summary still generating)` if you happen to peek at the generated prompt — that's expected.
12. In one folder, add or edit `AGENTS.md` with distinctive text (e.g. "This service handles auth only") and keep a generic `README.md`. Send another prompt asking what that folder is for. Expected: the summary leans on the AGENTS content, not only the README blurbs.
13. Rename a manifest to odd casing on disk (e.g. `readme.MD` or `agents.md` at repo root). Send a prompt after the watcher settles. Expected: it still participates in the bundle (case-insensitive top-level resolve); description updates after signature change on the **next** turn.
14. Edit one folder's `README.md` heavily, save, and send another prompt. Expected: that folder's description regenerates on the next turn (cache invalidated by the input-signature change).

### Sub-agent: research mode against the active folder

15. Send a prompt that nudges the model toward research delegation: "spawn a research sub-agent to find every place we call `tracing::warn!` and summarise the categories".
16. Expected: a `spawn_subagent` tool call appears with `mode: "research"`. A collapsed card renders inline under the tool row with a `research` badge (quiet neutral fill), the active folder's basename, and a "running…" status pip. The card streams updates as the sub-agent's events arrive.
17. Click the card ("Open transcript"). Expected: panel swaps into the sub-agent view with a `← Back` header and the full sub-agent transcript (assistant deltas + tool calls). Click `← Back`: returns to the parent's session.
18. Sub-agent finishes. Expected: collapsed card status flips to `done`, result preview shows the first 2 lines of the answer, token estimate visible.

### Sub-agent: coder mode against another bound folder

19. Stay on folder X as active. Send a prompt: "spawn a coder sub-agent in folder Y to add a `// TODO` comment at the top of `src/main.rs`" (substitute paths that exist in Y).
20. Expected: `spawn_subagent` tool call with `folder` set to Y and `mode: "coder"` (or omitted, defaulting to coder). Collapsed card has the accent-tinted `coder` badge and shows folder Y's basename.
21. After the sub-agent finishes, run `git status` in folder Y from a terminal. Expected: the file in folder Y is modified — the sub-agent's `edit_file` actually wrote there, even though the parent stayed bound to folder X.
22. Verify the sub-agent's JSONL on disk: `ls <XDG_DATA_HOME>/moon-ide/coder-sessions/<X-slug>/sub-*.jsonl` (parent **X** slug, not Y). The header should include `parent_session_id`, `parent_tool_call_id`, and `subagent_mode: "coder"`, plus `subagent_target_folder` / equivalent metadata tying the transcript to folder Y because the tool target differed from the parent path.

### Sub-agent: read-only enforcement

23. Send a prompt: "spawn a research sub-agent that tries to call `write_file` on README.md". (Or modify the system prompt to lean on it.)
24. Expected: the sub-agent's `write_file` attempt returns a `ReadOnlyMode` error in its tool result. The model recovers by returning a text answer instead of mutating the file.

### Parallel sub-agents

25. Send a prompt: "in parallel, spawn three research sub-agents — one to summarise folder A, one for folder B, one for folder C". (Single bound folder, so spawn three sub-agents pointing at the same folder with different scoped tasks if you only have one.)
26. Expected: three collapsed cards appear in quick succession (all `running…`). Their inner transcripts update concurrently — open one, see deltas land in real time; switch to another, also live.
27. With more than 4 concurrent spawns: extras queue against the `Semaphore::new(4)`. Verify by spawning 6 in one batch — at most 4 are `running` simultaneously, the others stay queued until a permit frees.

### Cancellation cascade

28. Spawn a long-running sub-agent. While it's still streaming, hit the parent's stop button **without changing away from this folder**.
29. Expected: parent + every live sub-agent stop within ~1s. Each sub-agent's collapsed card flips to its terminal status (`error` for those that hit `Aborted`, or `done` if they happened to finish before the cascade landed). No orphan tasks: `top` shows no `tokio` worker stuck on the LLM HTTP call.

### Depth=1 cap

30. Get a sub-agent to try to spawn its own sub-sub-agent (e.g. include "spawn another sub-agent" in the task). Expected: the model never sees `spawn_subagent` in the sub-agent's tool list, so it can't even describe the call. If it tries by hallucinating the schema, the `dispatch()` returns `UnknownTool` and the sub-agent moves on.

### Persistence + IDE restart

31. Run a sub-agent to completion. Note the `sub_session_id` from the parent's tool result.
32. Quit moon-ide, restart.
33. Reopen the parent session. Expected: the parent's tool row + collapsed card render correctly (the parent JSONL replay rebuilds them).
34. Click "Open transcript" on the sub-agent card. **Known limitation**: the pop-out shows "Sub-agent transcript not available — re-open the parent session to refresh" because in-memory transcripts are lost on quit and we don't yet load sub-agent JSONLs from disk on demand. The on-disk JSONL still exists at `<XDG>/moon-ide/coder-sessions/<parent-folder-slug>/<sub-id>.jsonl`.

### Folder summary cache lifecycle

35. Bind a fresh folder containing only `Cargo.toml` (no README). Send a prompt and observe the parent's system prompt (or pull `coder_folder_summary` via devtools invoke). Expected: a description derived from `Cargo.toml`'s package name + dependencies, persisted at `<XDG>/moon-ide/folder-summaries/<slug>.json`.
36. Add a `README.md` with new content. Send another prompt. Expected: cache invalidates (input signature changes), regeneration fires, the description updates within a turn.

## What must keep working

- All Phase 6.x parent behaviours: streaming, auto-rename, session list, attachments, abort, sign-in/out.
- LSP-driven completion (sub-agents don't go through the LSP path; the parent's Ctrl-Space completion is unaffected).
- Existing on-disk parent sessions deserialise correctly even though `SessionHeader` gained four optional sub-agent fields (`parent_session_id`, `parent_tool_call_id`, `subagent_mode`, `subagent_target_folder`). The defaults serialize as omitted, so a re-write produces byte-identical JSONL for parent sessions that predate sub-agents.

## Known limitations

- **In-memory sub-agent transcripts only**: pop-out works for sub-agents observed in the current IDE session; older ones surface as "transcript not available". Loading from disk lands when the Tauri layer exposes a `coder_load_subagent` command + replay path.
- **Token estimate is approximated**: `tokens_used_estimate` is `bytes / 4`. Precise tracking arrives when streaming `usage` chunks are plumbed through the inference parser.
- **No per-sub-agent abort UI**: parent abort cascades to all sub-agents; individual cancel buttons are deferred.
- **Sub-agents don't see the parent's transcript**: by design — the `task` argument is meant to be self-contained. The model gets a system-prompt nudge that says exactly this.
- **`bash` in research mode is behavioural-only**: a confused / adversarial sub-agent could bypass via `bash -c 'echo foo > file'`. We accept the trade-off in exchange for `git log` / `cargo check` / `pytest --collect-only` working out of the box.

## Related

- Specs: [coder.md](../coder.md) (sub-agent + bound-folder sections rewritten), [protocol.md](../protocol.md)
- ADR: [0010 — coder rewrite](../decisions/0010-coder-rewrite-not-acp.md)
- Roadmap: [phase-06-coder.md](../roadmaps/phase-06-coder.md)
- Follow-up plan: per-folder git-status indicators in the project bar + container icon for compose services (tracked separately as "multi-project visibility").
