# Test plan 0058: cross-folder routing + drop fast/large model tier

- **Date**: 2026-05-07
- **Phase**: 6.x (coder polish). Supersedes the access-control half of [`0057-cross-folder-subagent-nudge.md`](0057-cross-folder-subagent-nudge.md): the synthetic `/workspace/<name>` paths still exist, but they now **route** to the named folder instead of erroring.

## What shipped

- `ToolRegistry::resolve_workspace_path` returns a `(target_folder, relative_path)` pair. `read_file`, `list_dir`, `write_file`, and `edit_file` dispatch against the returned folder's `WorkspaceHost`. Cross-folder access from the parent is now first-class.
- The previous cross-folder error message (with the `spawn_subagent` nudge) is gone. The only error case left is `/workspace/<name>/...` where `<name>` doesn't match any currently-bound folder; that error lists the bound folders so the model can self-correct.
- The bare-basename relative form (`<sibling-name>/foo.rs`) routes the same way as the synthetic absolute form. `./<sibling-name>/foo.rs` opts out and resolves against the active folder, same as before.
- `spawn_subagent` lost its `model: "fast" | "large"` argument. Sub-agents inherit `DEFAULT_LARGE_MODEL` — the same everyday-driver model the parent uses. `DEFAULT_FAST_MODEL` is retained for the auto-rename title generator only.
- `Subagent` struct lost its `model_tier` field; `ModelTier` enum is removed entirely from `crates/moon-coder/src/lib.rs`.
- System prompt rewritten:
  - `## Workspace folders` now says all path-taking tools route via `/workspace/<name>/...` to any bound folder.
  - `## When to use sub-agents` reframed: sub-agents are for **delegation** (context preservation, parallelism, scoped delegation), not access. Cross-folder access is the parent's own tools.
- Sub-agent system prompts (`RESEARCH_SYSTEM_PROMPT`, `AGENT_SYSTEM_PROMPT`) softened from "you cannot reach the parent's other bound folders" to "you are scoped to a single folder" — `grep` and `bash` still run against the assigned folder; relative paths resolve there.
- `spawn_subagent` tool description rewritten to lead with **context preservation** (large input, small output), then parallelism, then scoped delegation. Cross-folder access is mentioned as a useful side-effect, not the primary reason.
- Project-bar git status refresh (`bindCoderRefresh` in `state.svelte.ts`) is now surgical when possible: each parent `tool_call` event has its `args.path` parsed and the resolved folder added to a per-window pending set. The 200 ms debounced flush refreshes only those folders. Sub-agent activity, `bash`, `grep`, and parse failures still trigger the all-folders fan-out via a sticky bit. Worst case is the old behavior.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, signed in, **two** folders bound (e.g. `moon-ide` and `huggingface_hub`). The active folder is the first one.

### Inspect the parent's system prompt

1. Send a fresh prompt to the agent. Open the trace (test plan 0055).
2. Confirm:
   - `## Workspace folders` paragraph says the path-taking tools accept `/workspace/<name>/...` for any bound folder.
   - `## When to use sub-agents` lists context preservation first, parallelism second, scoped delegation third. Cross-folder access is no longer in the list of reasons.
   - `## Bound folders` lists every bound folder under its `/workspace/<name>` path.

### Cross-folder read via synthetic path

3. Ask: _"What does `/workspace/huggingface_hub/README.md` say? Just summarise the first paragraph."_
4. Expected: the agent calls `read_file({path: "/workspace/huggingface_hub/README.md"})` directly. The tool result is the file contents (not an error). The summary in the agent's reply matches the actual README.

### Cross-folder read via bare basename

5. Ask: _"List `huggingface_hub/src` for me."_ (or whatever directory exists in the sibling)
6. Expected: `list_dir({path: "huggingface_hub/src"})` succeeds and returns the directory listing.

### Same-named subdirectory inside active folder

7. Create a directory with the same basename as the sibling _inside_ the active folder, e.g. `mkdir -p crates/huggingface_hub` in `moon-ide`.
8. Ask: _"List `./huggingface_hub` — the one inside this folder."_
9. Expected: `list_dir({path: "./huggingface_hub"})` resolves against the active folder (returns `crates/huggingface_hub`'s listing — wait, actually `./huggingface_hub` would resolve relative to the active folder root). Let's say the test directory is `./huggingface_hub` directly under the active folder root: confirm the listing is the empty/local one, **not** the sibling's. Clean up the test directory afterwards.

### Cross-folder write

10. Ask: _"Add a comment line saying `// hello from the parent` at the top of `/workspace/huggingface_hub/some-existing-file.py`."_
11. Expected: the agent calls `edit_file` (or `write_file`) with `/workspace/huggingface_hub/...`. The actual file in the sibling folder gets modified. Confirm via the IDE's file tree on the sibling, and via `git status` in the sibling.

### Unbound folder error

12. Ask: _"Read `/workspace/no-such-folder/foo.txt`."_
13. Expected: tool result is `is_error: true`, message names `no-such-folder` and lists the actually-bound folders (the active and sibling basenames). Agent should explain to the user that the folder isn't bound.

### Sub-agent dispatch — model defaults

14. Ask: _"Spawn a research sub-agent against `huggingface_hub` to summarise the top-level structure."_
15. Open the parent's trace. Find the `tool_call` for `spawn_subagent`. Confirm:
    - The `args` JSON has `task`, optionally `folder` and `mode`. **No `model` key**, even if the model tries to add one.
    - If the model does include `model: "fast"` or `model: "large"` in the call, the dispatch should succeed (extra unknown fields are ignored by the deserializer) — the sub-agent runs on the large model regardless. Confirm by opening the sub-agent's session JSONL: the header's `model` field is `Qwen/Qwen3.5-397B-A17B:scaleway` (or whatever `DEFAULT_LARGE_MODEL` resolves to).

### Sub-agent dispatch — model is the everyday driver

16. From the project bar, double-click the sub-agent badge to open its pop-out.
17. Confirm the token-usage ring shows the same model name as the parent's. The "fast" model should appear nowhere in the sub-agent's transcript or trace.

### Auto-rename still uses the fast model

18. Send a fresh prompt that produces a coherent first turn (e.g. _"Hi, can you list the files in `src/lib`?"_). Wait for the turn to complete.
19. The session title in the sidebar should change from the truncated prompt to a 2-5 word generated title within ~2-3 seconds.
20. Open the session JSONL. The session-rename event's `model` field (or whichever metadata records it) should be `Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway` — the fast model. Auto-rename is the only remaining consumer of `DEFAULT_FAST_MODEL`.

### Project-bar git status: surgical refresh

21. Open the IDE with the active folder clean (`git status` shows nothing). The sibling is also clean; its project-bar badge shows nothing.
22. Ask: _"Add a TODO comment to `/workspace/huggingface_hub/some-existing-file.py`."_
23. Within ~half a second of the turn completing, the **sibling's** project-bar badge should show `~1`. The active folder's badge should remain empty (the parent didn't touch it). This confirms the surgical-refresh path correctly identified the sibling as the only touched folder.
24. Have the agent edit a file in the active folder afterwards. The active folder's badge should update; the sibling's should still show its earlier `~1`. Both folders being touched in one turn should result in both badges updating without one of them flickering through "0" first.

### Project-bar git status: sub-agent fan-out fallback

25. Spawn a sub-agent against the sibling that does a bunch of `edit_file`s.
26. The sub-agent's edits don't carry the bound folder in the wrapper event, so the listener flips the fan-out bit. On `subagent_finished` the flush refreshes both folders. Confirm by checking that the sibling's badge updates after the sub-agent completes (you should see badge update timing match the sub-agent's completion, not its individual tool calls).

### Tests + lints

```
cargo test -p moon-coder
cargo clippy --all-targets -- -D warnings
bun run check
bun run lint
```

All green. The `tools::tests::cross_folder` module now has tests that confirm cross-folder paths route to the sibling instead of erroring (`synthetic_sibling_path_routes_to_other_folder`, `relative_sibling_basename_routes_to_other_folder`, `relative_sibling_basename_alone_routes_to_root`), plus the unbound-name error case (`synthetic_unbound_name_errors_with_bound_list`).

## What's deliberately not tested

- We do **not** test that the model spontaneously prefers `spawn_subagent` over direct cross-folder access for context-preservation cases. That's a behavioural property of the prompt; we'll observe it in real workloads over the next week of dogfooding and iterate on the prompt if the model is too eager to do everything itself. The roadmap entry on this is "watch and adjust", not "verify with a unit test".
- We do not test `bash` or `grep` cross-folder behavior — those are documented as active-folder-only and that hasn't changed.
- The "two folders being touched in one turn" surgical-refresh case (step 24) depends on test ergonomics; if the model batches both edits into the same `tool_call` window the badges flip together, and that's the right answer either way.
