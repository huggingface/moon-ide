# ADR 0019 — Coder writes defer the format-on-save chain to turn end

Date: 2025-11-22
Status: accepted; supersedes the "every coder/agent edit funnels
through `save_file`" claim in [ADR 0012 § Decision](0012-format-on-save.md#decision)
and the matching paragraph in [`specs/editorconfig.md`](../editorconfig.md).
[ADR 0013](0013-format-on-save-file-based.md) is otherwise unchanged —
the file-based lint-staged invocation, the editorconfig pre-save
stage, the best-effort posture, and the container routing all still
hold. The only thing that moves is **when** the chain runs for
writes issued by the coder's `write_file` / `edit_file` tools.

## Context

Phase 6.2 routed the coder's `write_file` and `edit_file` tools
through `WorkspaceHost::save_file` so agent-written bytes landed in
the same shape `Ctrl+S` would (see [test plan 0047](../test-plans/0047-format-on-save.md)).
The seam was clean in 2025-04: one write path, one formatter
invocation, one mental model.

Two things broke that posture as soon as the team started actually
using the agent on real codebases:

1.  **Same-file re-edits re-run the chain N times.** A turn that
    touches `src/lib/state.svelte.ts` five times spawns
    `prettier-svelte`, `oxlint --fix`, and `tsgo --noEmit` five times
    in series against the same file. On `moon-landing` the chain
    takes ~1.5–4 s; a five-edit turn was paying 7–20 s in formatter
    wall-time alone — for no benefit, since only the bytes after the
    _last_ edit matter to the human.
2.  **`eslint --fix` strips imports the model hasn't used yet.**
    The bug that triggered this ADR. A typical agent turn looks
    like:

        edit_file: add `import { foo } from './bar'` to top of file
        (format-on-save runs `eslint --fix`, sees `foo` is unused, deletes the import)
        edit_file: add `foo(x)` somewhere in the body
        (build fails: `foo` is not defined)

    The model's next iteration sees the build error and tries to
    re-add the import, which gets stripped again on the next save.
    In dogfooding this manifested as a few-percent regression in
    "first turn lands compiling code" that we initially mis-
    attributed to the LLM. It's actually the formatter eating the
    model's work between edits.

The "every write funnels through `save_file`" rule was right in
spirit (consistent on-disk shape regardless of who wrote) but wrong
in granularity (the formatter shouldn't run between intermediate
states of a multi-step plan the model hasn't finished executing).

## Decision

**Defer the entire `save_file` pipeline — editorconfig pre-save
_and_ lint-staged chain — to turn end for writes issued by the
coder's tools.**

Concretely:

- A new `WorkspaceHost::format_file(path)` method runs the same
  two-stage pipeline `save_file` runs, but sourced from the bytes
  already on disk: read → editorconfig → write back (if it
  changed anything) → lint-staged chain → re-stat. Same best-
  effort posture: a missing tool or non-zero exit collapses to a
  `tracing::warn!` under source `format-on-save`, never aborts.
- A per-turn `FormatQueue` (lives on `ToolContext`, a `HashSet`
  of `(folder, relative_path)`) collects every path the coder's
  `write_file` / `edit_file` tools touched.
- Those tools now call `host.write_file` (raw bytes) instead of
  `host.save_file`, then push their path into the queue.
- At turn end — for **every** termination, including `Aborted`
  and `Err` — the runner drains the queue and calls
  `host.format_file` against each unique path exactly once.
- Sub-agents own their own `FormatQueue` and flush it before
  emitting `SubagentFinished`, so by the time the parent's next
  iteration can `read_file` against a sub-agent-touched path the
  file is already formatted.
- `WorkspaceHost::save_file` is unchanged. `Ctrl+S` in the
  editor, the LSP rename's closed-file path, the "open host
  file" save, and every other non-coder write keeps the old
  shape: editorconfig + lint-staged synchronously, one save =
  one chain run.

Net behaviour:

- Files touched by the agent during a turn carry the bytes the
  model literally wrote until the turn ends. The model sees the
  same bytes any subsequent `read_file` sees — there is **no**
  case where intermediate formatter output leaks into the
  model's view.
- At end of turn, every unique file the agent touched gets
  formatted exactly once, regardless of how many edits hit it.
- Failure modes (Esc, iteration cap, inference error) still
  flush. A partially-completed turn lands editorconfig-
  normalised, lint-staged-formatted bytes — same shape as if
  the user had manually opened each touched file and pressed
  `Ctrl+S` after the agent bailed.

## Consequences

### What changes for the model

- `edit_file`'s tool description used to say "Format-on-save runs
  after the edit, so `replace` doesn't need to match the
  formatter's exact output." It now says "Bytes the file holds
  between your edits in a turn are exactly what `write_file` /
  `edit_file` wrote — the format-on-save chain runs once per
  touched file at the end of the turn." The looser `find`
  matching is unchanged (the four-stage fuzzy-match cascade
  already handles indent / whitespace drift); what's new is the
  inter-edit-byte-stability contract.
- `read_file` issued between two `edit_file`s sees the bytes the
  previous `edit_file` wrote, byte-for-byte. Previously it saw
  formatter output. This is strictly easier for the model to
  reason about.
- `bash cargo check` (or any other shell-based verifier) issued
  mid-turn runs against un-formatted-but-editorconfig-pending
  bytes. In practice no compiler / linter the team uses cares
  about trailing whitespace or final newlines, so this is a
  non-issue.

### What changes for `WorkspaceHost`

- The trait grows `format_file`. Implementations:
  - `LocalHost::format_file` shares `restat_after_format` and
    `stat_as_write_result` helpers with `save_file`; the
    editorconfig stage runs against on-disk bytes via
    `read_file` → `pre_save::apply_pipeline` → `write_file` (no-
    op when canonical, so a turn full of trivial edits that
    leave bytes unchanged doesn't fire the fs watcher).
  - `RemoteHost` (Phase 2) will mirror the shape over JSON-RPC
    when it lands — same two-stage pipeline, just dispatched
    inside the container's `LocalHost`.
- `save_file` is unchanged, including the rustdoc, except for a
  pointer at `format_file` explaining that the coder tools
  deliberately route through `write_file` + `format_file`
  instead. The "every editor save funnels through `save_file`"
  invariant remains true for everything that isn't the coder.

### What changes for the format-on-save panel

- Per-tool-call log lines disappear. A multi-edit turn used to
  spam the `format-on-save` source in the diag-logs panel with
  N entries per tool-result; now it emits N entries at the end
  of the turn, grouped by the path the chain ran against. Per-
  process dedup ("tool not found" warnings) is unchanged.

### What doesn't change

- The lint-staged chain shape (file-path appended, command
  mutates in place, continue past failures).
- Container routing through `ShellTarget` (host vs. `docker
exec`) — `format_file` reuses `run_formatter_chain` verbatim.
- The default-formatter fallback table (Rust / Python rows).
- The `Ctrl+S` editor save path. Existing test plan steps in
  [0047](../test-plans/0047-format-on-save.md) and
  [0063](../test-plans/0063-format-on-save-file-based.md) that
  test the editor save path still pass.
- The "no toggle" stance. Hardcoded on, per AGENTS.md "hardcode
  first, configure later".

## Alternatives considered

### A. Keep the editorconfig stage inline, defer only lint-staged.

Editorconfig is cheap (in-process, pure functions) and idempotent;
running it on every edit gives subsequent `read_file`s
normalised bytes. Tempting because it splits the "expensive
stuff later, cheap stuff now" line at the obvious seam.

Rejected because:

1. The bytes-between-edits contract is easier to communicate
   when it's exact ("what you wrote is what's there") than
   when it's almost-exact ("what you wrote, with line endings
   and trailing whitespace normalised"). Models trip over the
   "almost" part.
2. The inline editorconfig pass is small but non-zero — a
   five-edit turn still pays five `editorconfig_for` lookups
   plus the pipeline. Not free.
3. Splitting the pipeline complicates `format_file` (the turn-
   end flush has to know whether editorconfig already ran or
   not). A single boundary is simpler to reason about.

### B. Skip the flush on `Err` / `Aborted` turns.

Considered. The argument for skipping: the user might be Esc'ing
_because_ they don't like what the agent is doing, and
formatting half-written code is arguably hostile.

Rejected because:

1. Format-on-save is what the user gets on `Ctrl+S` of the same
   half-written code anyway. The agent's partial work is now
   on disk; pretending it isn't doesn't help.
2. A common Esc pattern is "the model is taking too long but
   the first few edits look right, just stop and let me
   continue manually". Those first few edits should be in the
   same shape the user would have produced typing them.
3. The auto-rename hook already runs on `Err` / `Aborted` for
   the same reason (a long tool-heavy turn the user Esc'd
   should still earn a title); the flush is symmetric.

### C. Per-file debounce instead of per-turn batch.

A small in-process timer that coalesces saves to the same path
within a short window (say 500 ms). Would help the multi-edit
case but not the "import gets stripped before its first use"
case, since two edits in a coherent plan can easily span more
than half a second. Per-turn is the natural batch boundary; per-
file debounce is a worse approximation of the same idea.

### D. Track the queue on `SessionRuntime` instead of `ToolContext`.

The queue is per-turn, not per-session, and a session can host
multiple turns serially. Hanging it off `SessionRuntime` means
re-initialising it at every turn start. Per-`ToolContext`
ownership (with a fresh `Arc<FormatQueue>` in the runner's
wrapper task) matches the lifetime exactly and keeps sub-agent
ownership clean — every sub-agent's `cx` already gets a fresh
queue out of `ToolContext::new`.

## Tests

`crates/moon-core/src/host.rs` grows two `format_file_*` tests:
the editorconfig + lint-staged chain runs against pre-seeded on-
disk bytes, and the all-canonical no-rule path is a stat-only
no-op (mtime doesn't move).

Runner-level: the existing `cargo test -p moon-coder --lib` set
keeps passing because the format queue is plumbed but not
asserted from inside the registry tests. A higher-level test
exercising "five `edit_file`s, one flush" would be valuable but
needs a `WorkspaceHost` test double; punted to a follow-up
unless the manual test plan turns up a bite.

## References

- [ADR 0012 — Format on save via lint-staged](0012-format-on-save.md) (superseded `Decision`: now coder writes don't funnel through `save_file`).
- [ADR 0013 — Format on save: file-based lint-staged invocation](0013-format-on-save-file-based.md) (unchanged; the chain shape and routing still apply).
- [`specs/editorconfig.md`](../editorconfig.md) (the "every coder/agent edit funnels through `save_file`" paragraph updated to point here).
- [`crates/moon-core/src/host.rs`](../../crates/moon-core/src/host.rs) — `save_file`, `format_file`, `restat_after_format`, `stat_as_write_result`.
- [`crates/moon-coder/src/tools.rs`](../../crates/moon-coder/src/tools.rs) — `FormatQueue`, `ToolContext::with_format_queue`.
- [`crates/moon-coder/src/runner.rs`](../../crates/moon-coder/src/runner.rs) — `flush_format_queue` in the `send` wrapper task.
- [`crates/moon-coder/src/subagent.rs`](../../crates/moon-coder/src/subagent.rs) — inline flush loop between `run_subagent_inner` and the `SubagentFinished` event (sub-agent tools today only write inside `spec.folder`, so the flush calls `spec.folder.host.format_file` directly rather than re-routing through the workspace registry).
