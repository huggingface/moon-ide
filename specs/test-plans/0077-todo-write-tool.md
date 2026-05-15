# Test plan 0077: todo_write tool

- **Date**: 2026-05-15
- **Phase**: Phase 6 — coder (post-6.3 follow-up)

## What shipped

- A new `todo_write` tool the agent can call to maintain a small in-context plan for the current task. Wire shape matches Cursor / pi-mono so prompts carry over verbatim: `{ todos: [{ id, content, status }], merge: bool }` → `{ todos: [...] }`.
- Per-session storage on `Session.todos`; one `SessionRecord::TodosUpdate` snapshot appended per call; replay-last-wins on session reopen. List survives compaction.
- Runner short-circuits the dispatch (alongside `spawn_subagent`) since the list lives on per-session state. Sub-agents (both `agent` and `research`) get the same tool with their own scratchpads written into their own JSONL.
- Frontend: a compact pill next to the panel header's context ring (dominant glyph + `done / total`, hidden when empty, click to expand a popover with the full list). Per-call body via `ToolBodyTodoWrite.svelte`. Hint chip on the collapsed tool row prefers `→ <in-progress content>`, falling back to `M / N done`.
- System prompt gets a `## Todo list` section telling the model when to use it (3+ steps, multi-region work) and the conventions (one item `in_progress` at a time, full fields per item even with `merge: true`, `merge: false` + `[]` to clear).

## How to test

Prerequisites: `bun install`, signed-in to a coder provider (HF or OpenRouter), at least one workspace folder open.

1. **Cold start, empty list.** Open a fresh coder session in a project. The header to the left of the context ring should show **no pill** (list is empty). Open the model settings and confirm `todo_write` is in the advertised tool list (model picker → tools section, if exposed; otherwise inspect the network call).
2. **First plan.** Prompt: _"Plan a small refactor: rename `getCwd` to `getCurrentWorkingDirectory` across this repo. Use a todo list."_ Expected: the agent emits a `todo_write` call with `merge: false` and 3-5 items, statuses mostly `pending` with one `in_progress`. Header pill appears immediately, glyph `▶`, accent-coloured. Pill text reads `0/N`. The collapsed tool-row chip shows `→ <first in-progress item>`. Expanding the row shows the same list with status glyphs and accent on the in-progress item.
3. **Click the pill.** A popover opens under the pill, listing every item with its glyph and content. The in-progress item is accent-coloured; pending items are neutral; completed / cancelled (none yet) would be struck through. Click outside the pill → popover closes. Re-open, press `Escape` → also closes.
4. **Status flip via merge.** Continue the conversation: _"go"_ (or whatever). Expected: as the agent finishes each step, it emits `merge: true` calls flipping `status: in_progress` → `completed` and the next pending item → `in_progress`. Pill count ticks up (`1/5`, `2/5`, …), glyph stays `▶` while any item is in progress. Each call appears as its own row in the transcript with the post-merge list.
5. **All-done state.** When the agent flips the last item to `completed`, the pill glyph switches to `✓` and the count reads `N/N`. The popover shows every item struck through with the success-coloured `✓` glyph.
6. **Cancellation.** Trigger a scenario where the agent cancels an item (e.g. _"actually skip the docstring update"_). Expected: it flips that item to `cancelled` (struck through, neutral colour, `−` glyph) and continues. The cancelled item counts toward the `done` numerator the same way `completed` does.
7. **Wholesale replace.** Ask the agent to switch tasks mid-session: _"forget that, instead let's audit the build script"_. Expected: a `todo_write` call with `merge: false` and a fresh 3-item list. The pill resets to `0/3`; the popover shows only the new items.
8. **Clear the list.** _"drop the todo list"_. Expected: a `merge: false` call with `todos: []`. Pill disappears (popover, if open, auto-closes). Per-call body shows the "list cleared" placeholder.
9. **Persistence + replay.** Quit and relaunch the IDE (or open a different session and come back). Expected: when the session re-opens, the pill state matches what it was before the close (reflecting the **last** `TodosUpdate` record on disk). The transcript shows the historical `todo_write` rows with their per-call lists.
10. **Compaction passthrough.** Drive the session into auto-compaction (long context). Expected: compaction collapses old messages, the pill still shows the same list before and after, and a follow-up `todo_write` call still merges against the right set of items (the agent didn't lose track of its plan).
11. **Sub-agent isolation.** Ask the agent to spawn a sub-agent that itself uses a todo list. Expected: the sub-agent's collapsed card shows its own per-call `ToolBodyTodoWrite` rows. The **parent's** header pill does **not** include sub-agent items — they're separate scopes.
12. **Backend unit tests.** `cargo test -p moon-coder --lib todo::` — all `merge_todos` cases pass (replace, clear, in-place update, append unknowns, leave-untouched, no-op, snake_case serde).

## What must keep working

- Sessions written before this commit (no `TodosUpdate` records) still load cleanly with an empty pill — the replay loop sees zero `TodosUpdate` records and `last_todos` stays `Vec::new()`.
- `compaction_complete` doesn't touch `Session.todos`; the pill survives the fold.
- `dispatch_tool_calls` still routes `spawn_subagent` and the parent-loop tools through their original paths — the `todo_write` short-circuit is additive.
- The hint chip on collapsed tool rows continues to work for every other tool (path for file ops, command for `bash`, pattern / query / URL for the rest); we only added a new branch for `todo_write`.
- Sub-agent runs without `todo_write` calls do not regress: the new local `todos: Vec<TodoItem>` defaults to empty and the dispatch path falls through to `tools.dispatch` for every other tool.
- `bun run fmt && bun run lint && bun run check` stays clean — no new warnings under oxlint, tsgo, svelte-check, clippy, or `cargo check`.

## Known limitations

- The user can't tick items by hand. The list is the agent's scratchpad; "edit by user" isn't a flow we model. If the user wants to change the plan, they ask the agent.
- No hard cap on list size. The system prompt asks the agent not to fragment work into micro-steps; we'll add a cap if it earns its keep.
- Sub-agent todos don't surface in the parent's header pill. Fine for now (separate scopes), revisit if dogfooding shows users want a "subagent's plan" indicator.
- The popover doesn't reposition itself on scroll / panel-resize. It's anchored under a button in the panel header which sits at the top of a stable layout, so this isn't biting today; revisit if it becomes one.

## Related

- Specs: [`specs/coder.md`](../coder.md#todo-list-tool) — wire shape, lifecycle, frontend rendering rules.
- Roadmap: [`specs/roadmaps/phase-06-coder.md`](../roadmaps/phase-06-coder.md) § 6.3.
- Prior plans: [`specs/test-plans/0043-coder-sessions.md`](0043-coder-sessions.md) (Sessions on disk; the persistence model `TodosUpdate` plugs into).
