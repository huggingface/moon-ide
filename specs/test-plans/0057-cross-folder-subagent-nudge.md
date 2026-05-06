# Test plan 0057: `/workspace/<name>` synthetic paths + cross-folder sub-agent nudge

- **Date**: 2026-05-07
- **Phase**: 6.x (coder polish) — small surface but it changes the system prompt, the path-handling in four tools, and the `spawn_subagent` description, so it gets its own plan.

## What shipped

- Every bound folder is now advertised in the system prompt under a synthetic `/workspace/<name>/...` path, regardless of where the folder actually lives on disk and regardless of whether moon-ide is in host or container mode. The active folder is also exposed under that surface, so the model can use either form (`src/foo.rs` or `/workspace/<active-name>/src/foo.rs`) interchangeably.
- New `ToolRegistry::resolve_workspace_path` runs first thing in `read_file`, `list_dir`, `write_file`, and `edit_file`. It strips the `/workspace/<active-name>/` prefix when present, and **rejects** paths that target another bound folder (either `/workspace/<other>/...` or a relative path whose first segment matches a sibling's basename). The error names the folder and tells the model exactly what to do: `spawn_subagent` with `folder: "<other-name>"`. Disambiguation opt-out: prefixing with `./` (so `./<other-name>/foo.rs`) skips the check and lets a same-named subdirectory inside the active folder pass through.
- `PHASE_6_0_SYSTEM_PROMPT` rewritten with explicit "Workspace folders" + "When to use sub-agents" sections covering the three reasons to reach for `spawn_subagent`: cross-folder access, parallelism (4-concurrent), and context preservation (don't pollute the parent transcript with 30 file reads when one paragraph is the answer).
- `compose_system_prompt` renders each bound folder as `- /workspace/<name> **(active — your tools operate here)** · <description>` for the active folder and `- /workspace/<name> — sibling, reach via spawn_subagent · <description>` for the rest. Stale "(summary still generating)" placeholder still applies.
- Sub-agent system prompts (`RESEARCH_SYSTEM_PROMPT`, `CODER_SYSTEM_PROMPT`) now spell out: tools operate against the assigned folder only, no cross-folder reach, no nested `spawn_subagent`. Sub-agents go through the same path gate as parents.
- `spawn_subagent` tool description rewritten to lead with the _three reasons_ (cross-folder, parallelism, context-preservation) rather than the previous "spawn against a folder" framing.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, signed in, two folders bound to the workspace (e.g. `moon-ide` and a sibling repo like `huggingface_hub`). The active folder is the first one; the sibling is bound but inactive.

### Inspect the parent's system prompt

1. With both folders bound, send a fresh prompt to the agent.
2. Open the session trace via the `</>` button (test plan 0055) and find the leading `system` record. Confirm:
   - There is a `## Workspace folders` paragraph that mentions the synthetic `/workspace/<active-name>/` form.
   - There is a `## When to use sub-agents` section that lists the three reasons (cross-folder, parallelism, context preservation).
   - The `## Bound folders` section lists each folder as `- /workspace/<name>` with `(active — your tools operate here)` on the active one and `sibling, reach via spawn_subagent` on the others.

### Cross-folder rejection — absolute synthetic path

3. Manually craft a prompt that nudges the agent to read from the sibling: e.g. _"What does `/workspace/huggingface_hub/README.md` say?"_
4. The agent will likely call `read_file({path: "/workspace/huggingface_hub/README.md"})`. Expected: the tool result is `is_error: true` with a message that mentions `huggingface_hub`, `spawn_subagent`, and the `folder: "huggingface_hub"` argument. The agent should then either issue a `spawn_subagent` call or explain to the user that it needs to delegate.

### Cross-folder rejection — relative basename

5. Same setup, ask: _"Look in `huggingface_hub/src/lib.rs` and tell me what's there."_
6. Expected: `read_file({path: "huggingface_hub/src/lib.rs"})` (or similar) errors with the same sub-agent suggestion. This is the failure mode the user originally reported.

### Cross-folder rejection — bare basename

7. Ask: _"List the contents of huggingface_hub."_
8. Expected: `list_dir({path: "huggingface_hub"})` errors with the sub-agent suggestion.

### Active-folder synthetic path resolves

9. Ask: _"Read `/workspace/<active-name>/AGENTS.md`."_
10. Expected: the file opens just like `read_file({path: "AGENTS.md"})` would. No error. The agent shouldn't even notice the synthetic form was rewritten.

### `./` opt-out for legitimate same-named subdir

11. Setup: a directory inside the active folder happens to share a sibling's basename (e.g. the active folder has a `foo/` subdir, and a sibling folder is also named `foo`).
12. Ask the agent to read a file in that subdir, gently suggesting the `./` form: _"What's in `./foo/README.md` (the local subdir, not the bound folder)?"_
13. Expected: `read_file({path: "./foo/README.md"})` succeeds. No cross-folder error. Without the `./`, it would have errored.

### Sub-agent honours the same gate

14. Spawn a sub-agent against a folder. Ask the parent to suggest the sub-agent reach a third folder: _"Spawn a research sub-agent against repo-a and ask it to also peek at repo-b."_
15. Expected: the sub-agent's `read_file({path: "/workspace/repo-b/..."})` call also errors with the cross-folder message. Sub-agents don't have `spawn_subagent` themselves, so they have to report back to the parent.

### Auto-recovery test

16. Same prompt as step 5 (relative basename of a sibling). Watch the panel.
17. Expected: after the first turn errors, the model issues `spawn_subagent({folder: "huggingface_hub", mode: "research", task: "..."})` and reports the result back to the user. The cross-folder error message should be enough to push the model into the right call without further user steering.

## What must keep working

- Same-folder tool calls — relative paths inside the active folder (`src/foo.rs`, `crates/moon-coder/src/lib.rs`, `./local/...`) all resolve as before. The path gate is a pre-check; the host's existing canonicalization + workspace-root check still runs after.
- The host's existing "outside workspace root" rejection — paths like `/etc/passwd` or `../../etc/passwd` still error from `LocalHost::resolve`. The cross-folder gate intentionally only fires for paths it can recognise as a bound-folder reference; everything else passes through to the host's safety net.
- `grep` and `bash` — neither got a path gate. `grep` always searches the active folder root, and `bash` is by-design free to do anything (the model could `cd` anywhere; that's a separate problem).
- `spawn_subagent` happy path — same args, same return shape, same parallelism cap (4), same fast-vs-large model selection. Only the description changed.
- Empty workspace / single-folder workspace — when there are no siblings, the path gate has nothing to reject. The "Bound folders" section still renders the active folder so the synthetic path is consistently advertised.

## Known limitations

- The bare-basename detection (`<sibling>/foo.rs`) is name-based, not stat-based. If the active folder really has a subdirectory whose name matches a sibling's basename, the model has to use `./<sibling>/foo.rs` to reach it. This is documented in the error message itself, so the model gets one round-trip of feedback. The alternative — stat the active folder root before deciding — adds an `await` to every path-taking tool and only helps in a name-clash case we expect to be rare in practice.
- `bash` is unguarded. A model that runs `cd ../sibling-repo && cat README.md` bypasses the gate entirely. We accept that: bash is meant to be the escape hatch for "tools don't cover this", and trying to police shell paths gets exponentially complicated for ~zero added safety. The system prompt nudges sub-agent usage; `bash` is the pressure-relief valve.
- The synthetic `/workspace/<name>` is purely a presentational + lexical convention. There's no actual mount at `/workspace`. If the model dumps a literal absolute path in source code (a `bash` heredoc that writes a Dockerfile, say), the `/workspace/` prefix won't survive into the final artefact unless it goes through the active-folder rewrite first. This is a non-issue for the tools but worth noting.
- Folder basenames are derived from the path's tail at bind-time. Two bound folders with the same basename (`foo` and `bar/foo`) is an edge case the gate treats as ambiguous — `find` returns the first match. Sub-agent target resolution (`subagent.rs::find_bound_folder`) has the same wart, so we're at least consistent. Add disambiguation when this comes up in real life.

## Related

- Specs: [coder.md § Synthetic `/workspace/<name>` paths and cross-folder rejection](../coder.md), [coder.md § Tool surface](../coder.md), [coder.md § Sub-agents](../coder.md), `crates/moon-coder/src/tools.rs::resolve_workspace_path`, `crates/moon-coder/src/defaults.rs::PHASE_6_0_SYSTEM_PROMPT`.
- Prior test plans: [0044-coder-polish.md](0044-coder-polish.md) (initial sub-agent surface), [0054-token-usage-and-auto-compaction.md](0054-token-usage-and-auto-compaction.md) (the most recent agent-loop test plan).
