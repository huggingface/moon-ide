# Test plan 0079: coder host-mode absolute paths, `task` rename, running pip, sub-agent persistence

- **Date**: 2026-05-17
- **Phase**: Phase 6.x (coder polish)

## What shipped

Four small, related coder changes that the human asked for in one batch (see [agent-transcripts](../../) entry "no legacy handling needed (eg handling old /workspace for host) - anyway do all 4 please"). Lumped into one test plan because they all touch the same layer (parent loop / panel) and want to be exercised together once you reload a session.

### A. `/workspace/<name>` in **host mode** is gone

- `compose_system_prompt` now branches on whether the workspace shell container is `Running` (probed via [`ToolRegistry::bash_target_is_container`](../../crates/moon-coder/src/tools.rs), the same plumbing the `bash` tool routes through). Host mode advertises every bound folder by its real absolute host path (`/home/eliheros/code/moon-ide`); container mode keeps `/workspace/<name>` because that's the actual mount inside the container.
- `resolve_workspace_path` accepts a new case: an absolute path that lands under any bound folder's root → that folder + the inside-folder relative path. The synthetic `/workspace/<name>` branch still resolves (no removal, no legacy gate at the resolver), but the prompt only steers the model to that form in container mode.
- `PHASE_6_0_SYSTEM_PROMPT`'s "Workspace folders" prose was rewritten to be mode-agnostic: it points the model at the dynamic "Bound folders" section for the right shape per turn.
- All four path-routing tool definitions (`read_file` / `list_dir` / `write_file` / `edit_file`) had their `path` description rewritten to mention either active-folder relative or the absolute path the prompt's "Bound folders" section advertises.
- Per AGENTS.md "no premature migrations" + the user's explicit "no legacy handling needed", we did **not** add a backward-compat alias for the old path shape. A model that emits the host-mode shape works; a model that emits the synthetic `/workspace/<name>` shape also works because that branch is still in the resolver. The change is purely about what we _advertise_ to the model.

### B. Sub-agent spawn / finish persisted on the parent's JSONL

- `SessionRecord` gained `SubagentSpawned { tool_call_id, subagent_id, target_folder, mode }` and `SubagentFinished { subagent_id, tokens_used_estimate, was_error, result_preview }` variants.
- [`handle_task`](../../crates/moon-coder/src/runner.rs) appends the spawn record to the parent's session JSONL **before** `run_subagent` runs, and the finish record after. Best-effort persistence: a write failure logs at warn but doesn't fail the tool call.
- `Coder::open_session` now replays both records: the spawn re-emits the live `SubagentSpawned` event (rebuilds the parent's collapsed card + seeds an empty transcript bucket) and the finish re-emits `SubagentFinished` (flips the card's status pip). For each spawn, the open path also reads the sub-agent's own JSONL under `<sessions_dir>/<parent_session_id>/<subagent_id>.jsonl` and re-emits each of _its_ records as `SubagentEvent`s, so the popped-out transcript view shows the original conversation, not just a synthetic preview.
- Missing sub-agent JSONL → `tracing::warn!` and the card-only restoration path. Sessions written before this landed have no spawn / finish records and reload exactly the way they did before (cards aren't there).

### C. `spawn_subagent` → `task` (wire / model-facing only)

- The tool's name advertised to the LLM and its arg parser is now `task`, not `spawn_subagent`. The new name matches what every other agent product the team has used calls this primitive, and a model that picks the wrong one in dogfooding consistently picks `task`.
- Internally, the Rust types (`Subagent`, `SubagentReport`), the module (`subagent.rs`), the on-disk record (`SubagentSpawned`, `SubagentFinished`), and the live event (`CoderEvent::SubagentSpawned`, `SubagentFinished`) all keep their `subagent` naming — the rename is one-layer-deep on purpose, since internally that's still what gets spawned and the disambiguation against `Session` matters.
- Old test plans (`0050-sub-agents.md`, `0057-cross-folder-subagent-nudge.md`, `0058-cross-folder-routing-and-drop-tiers.md`, `0059-rename-coder-mode-to-agent.md`, `0061-unify-subagent-iteration-cap.md`, `0050`, `0054`, `0077`) deliberately keep the `spawn_subagent` mention as a historical record of what shipped at the time — they're not the live contract.
- Runner-side: `dispatch_tool_calls` now matches `name == "task"`. The homogeneous-batch parallelism path triggers when **all** calls in an assistant message are `task`. The dispatch helper was renamed `handle_spawn_subagent` → `handle_task`.

### D. "Running" pip in the session list

- The session list (`coder.view === 'list'`) now paints a small pulsing accent dot left of the title and a `running…` label in the meta row for any session row whose turn is currently running. **Superseded by [ADR 0016](../decisions/0016-coder-concurrent-sessions.md):** when this plan shipped, `busy` was folder-scoped and only the visible session could ever be running, so the pip only ever appeared on one row at a time. Post-ADR, `busy` is per-session and multiple rows in the same folder can show pips simultaneously — see [test plan 0085](0085-coder-concurrent-sessions.md).
- `prefers-reduced-motion: reduce` disables the pulse keyframes; the dot stays solid in that case.

## How to test

Prerequisites: `cargo test -p moon-coder`, `cargo clippy --workspace --all-targets`, `bun run check`, `bun run lint` clean.

### A — host-mode paths

1. **Bind two folders, host mode.** Open the IDE on `~/code/moon-ide`. Add `~/code/moon-landing` as a sibling project (folder bar `+`). Confirm there's no workspace shell container running for this workspace (no green pip in the project bar; `docker ps` shows no `moon-shell-*` for the workspace id).
2. **System prompt advertises real abs paths.** Send any prompt to the agent (`hello`). In the parent session's JSONL (open the trace via `</>`), find the latest `SessionLoaded` / first user message; just above it, the system prompt's "Bound folders" section should list both folders by their absolute host paths (`/home/eliheros/code/moon-ide`, `/home/eliheros/code/moon-landing`), not `/workspace/<name>`. The "Workspace folders" prose should not mention `/workspace`.
3. **Tool calls accept abs paths inside the active folder.** Ask the agent to `read /home/eliheros/code/moon-ide/AGENTS.md`. Expected: tool result is the file's contents — no "escapes workspace root" error. Same for `read AGENTS.md` (relative); both should resolve identically.
4. **Tool calls accept abs paths inside a sibling.** Ask the agent to `read /home/eliheros/code/moon-landing/README.md` (or some other file you know exists in the sibling). Expected: tool result is the file's contents. The tool call's `args.path` echoes the absolute path; the routing happens inside `resolve_workspace_path` and you don't see anything about it in the UI, only that the read succeeded.
5. **Synthetic form is still tolerated.** Ask the agent to `read /workspace/moon-ide/AGENTS.md` (manually pasted, since the prompt won't suggest it in host mode). Expected: works — the resolver still recognises the `/workspace/<name>` branch even when the prompt doesn't advertise it. (Per AGENTS.md "no premature migrations": we don't gate this off; we just don't advertise it.)
6. **Container mode still uses /workspace.** If the team has a workspace shell container handy, start it (`Start container` in the project bar) and re-prompt. Expected: the prompt's "Bound folders" section now lists folders as `/workspace/<name>`. The same `read /home/...` abs-path call still works because the resolver is mode-agnostic, but the model would normally pick the form the prompt just showed it.
7. **Unrelated abs path is still rejected.** Ask the agent to `read /etc/passwd`. Expected: tool result is `{"error":"…escapes workspace root…"}` (or a not-found error if the path canonicalisation fails differently). Same as before this change.

### B — sub-agent persistence across reload

8. **Spawn one, observe the card.** In a fresh session, ask the agent to `task: research what AGENTS.md says about test plans, return one paragraph`. The model should call `task({ task: "...", folder: "<active-name>", mode: "research" })`. Expected: a collapsed sub-agent card lands inline under the parent's tool row, with a `running…` pip; finishes within a turn or two with a `done` pip and a 1–2-line preview.
9. **Click the card.** Card pops out into the sub-agent's full transcript: tool calls, intermediate assistant rows, final summary. The back-arrow returns to the parent.
10. **Reload the editor.** Hit `Ctrl+R` (or kill + relaunch the IDE). When the session re-mounts, the parent's transcript should re-render with the sub-agent card present (under the right tool row), in `done` state, with the preview text intact. The pop-out click still navigates into the sub-agent transcript — and the transcript shows the same rows as in step 9, not just the synthetic preview row.
11. **Disk shape.** Open the session JSONL in the editor (via `</>`). Search for `"kind":"subagent_spawned"` and `"kind":"subagent_finished"`. Each spawn record carries `tool_call_id`, `subagent_id`, `target_folder`, `mode`. Each finish record carries `subagent_id`, `tokens_used_estimate`, `was_error`, `result_preview` (omitted when null/empty). The sub-agent's own JSONL still lives under `<sessions_dir>/<parent_session_id>/<subagent_id>.jsonl` — open it; rows match the popped-out transcript.
12. **Missing sub-agent JSONL gracefully degrades.** `mv ~/.local/share/moon-ide/coder-sessions/<slug>/<parent_session_id>/<subagent_id>.jsonl /tmp/x` and reload the parent. Expected: the card still appears (rebuilt from the parent's `SubagentSpawned` record), but clicking it lands in an empty pop-out. The IDE log shows a `tracing::warn!` for the failed sub-agent load. Move the file back to clear.
13. **Pre-existing sessions** (before this landed). Open a parent session whose JSONL has no `subagent_spawned` records but did spawn sub-agents during its lifetime. Expected: no card on reload (per AGENTS.md "no premature migrations" — we don't backfill from sub-agent JSONLs). The user can still open the sub-agent's JSONL via `</>` if they know the id; that's the explicit fallback.
14. **Two sub-agents in one assistant message.** Ask the agent something that warrants two parallel `task` calls (same prompt, two scopes — e.g. "summarise both folders"). Both cards should appear under the same assistant row. After reload, both cards re-appear in the same order.

### C — `task` wire rename

15. **Tool list.** Open the parent's trace, find an assistant message that emits a `task` call: `tool_calls[].function.name === "task"`. The `parameters.properties` shape unchanged: `task` (string), `folder` (string), `mode` (`"research"` / `"agent"`), `system_prompt` (string).
16. **Inline tool hint.** The collapsed tool row in the parent transcript shows a chip like `<folder> · <mode> — <task first line>` (e.g. `moon-ide · research — research what AGENTS.md says…`). Same shape as the existing `bash` / `read_file` chips.
17. **Spec / prompt cross-references** all read `task` instead of `spawn_subagent` (system prompt's "When to use sub-agents" section, the `task` tool description's prose, the inline mention in the "Bound folders" section).
18. **No old-name fallback.** Ask the agent (or manually craft a tool call via the dev console) to invoke `spawn_subagent({task: "…"})`. Expected: `CoderError::ToolFailed` from the dispatcher's "no such tool" fallback (or the registry's equivalent error path). The model should self-correct on the next turn after seeing the error.

### D — running pip

19. **Single session running.** From the session list view, click `+` for a new session, send a prompt that takes a couple of seconds (any non-trivial task). Click "← Sessions" before it finishes. Expected: the new session appears in the list with a pulsing accent dot left of its title and a `running…` label in the meta row before the relative timestamp.
20. **Other sessions are quiet.** Other rows in the list (older sessions) show no dot, no label.
21. **Pip clears on finish.** Wait for the turn to finish (the `done` checkmark would appear if you'd been in the session view). Re-render the session list (the bucket's `busy` flips on `turn_complete`); the dot disappears.
22. **Reduced motion.** With `prefers-reduced-motion: reduce` set in the OS / browser, the dot is solid (no pulse animation) but otherwise present.
23. **Per-folder isolation.** Open a second project and start a turn there too. Switch back to the first project's session list. Expected: only the first project's running session shows a pip; the second project's running turn doesn't bleed into the first project's list. (One running turn per project; the bucket's `busy` is folder-scoped.)

## Out of scope / known limitations

- **Live container-mode probe overhead**: every `refresh_system_prompt` call now probes `bash_target_is_container`, which talks to docker via `moon-container::Workspace::status()`. Same call site `bash` already makes per-tool-call. If this becomes a hot path we'd cache the probe per-turn; for now it's one extra call per turn and well below the round-trip noise.
- **Sub-agent transcript pagination**: the open path replays _every_ record from a sub-agent's JSONL into the parent's bucket. Sub-agent JSONLs are typically tiny (one or two assistant turns + tool calls), so this is fine in practice. A pathological sub-agent with hundreds of iterations would slow reload measurably; we'll lazy-load on click if that becomes a real issue.
- **No `task` alias for `spawn_subagent`** by design — `tool_failed` is the surface a model should learn from, not a silent rename.
- **Path resolver doesn't reject `/workspace/<name>` in host mode**: it still works, just isn't advertised. A model that's seen the synthetic form in past sessions doesn't fight us when it carries that habit forward.
