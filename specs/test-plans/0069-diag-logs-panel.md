# Test plan 0069: diagnostic logs panel (step A)

- **Date**: 2026-05-11

## What shipped

- New `LogSink` actor in `moon-core`: per-source ring buffers
  (cap 2000) + `tokio::broadcast` fan-out. Held by `AppState`
  as `Arc<LogSink>`, wired into the LSP broker so status
  transitions, discovery hits/misses, fallback routing, and
  server stderr land in a user-visible buffer instead of only
  in the launcher terminal.
- New Tauri commands `logs_snapshot` / `logs_sources` /
  `logs_clear` / `logs_emit` plus a `logs:entry` Tauri event
  that pumps live entries from the broadcast.
- New bottom-panel `kind: 'diag'` tab driven by a unified
  `diagLogs` Svelte store that merges backend pump + local
  `frontendLog` emits into one per-source view. Source-picker
  popover next to the existing terminal launcher.
- `lspCompletionSource` and the editor's high-priority
  Ctrl+Space binding both call `frontendLog('editor.completion',
…)` so the user can see, from inside the IDE, whether
  Ctrl+Space reached CodeMirror, whether `lsp_completion` ran,
  and how many items came back.
- **LSP crash detection.** `LspClient` now tracks liveness
  via an `AtomicBool`. Either I/O loop exiting flips the flag
  and fires a `Notify`; `LspServer::spawn` parks a watcher
  task on that signal which emits `LspStatusEvent::Crashed`
  and an `error`-level log entry the moment the child dies.
  The broker's `ensure_server` checks `is_alive()` before
  handing out cached slots and evicts dead ones so the next
  request re-spawns. The frontend's `lsp:status` listener
  reacts to a fresh `crashed` transition by re-`lspOpen`-ing
  the active file, priming the re-spawned server with the
  live buffer text so the user's next request lands cleanly
  instead of hitting an empty doc set.
- Convention: source ids are `<area>.<sub-area>`
  (`lsp.typescript`, `editor.completion`, …); free-form
  otherwise. New sources show up in the picker on first emit.

## How to test

1. `bun run dev` (or run the release build). Open a workspace
   with a TypeScript project.
2. In the bottom panel, click the new **Logs** button next to
   **+ Terminal**. The popover should list at least
   `editor.completion`, `format-on-save`, `lsp.typescript`,
   `lsp.rust` even before any are populated (well-known set).
3. Pick `lsp.typescript`. Open a `.ts` file. Expected entries
   (in order):
   - `INFO  starting server (bin = tsgo)`
   - `DEBUG host discovery → …/tsgo` (or container path)
   - `INFO  server ready on primary route`
     When the project doesn't have `@typescript/native-preview`
     installed, expect a `WARN binary not found …` and a final
     `WARN not available; install hint: …` instead.
4. Pick `editor.completion`. Press **Ctrl+Space** in the editor.
   Expected entries on each press:
   - `INFO  Ctrl+Space pressed → invoking startCompletion`
   - `INFO  invoked (explicit=true, path=…, lang=…, line=…,
char=…) → calling lsp_completion`
   - `INFO  lsp_completion returned N items (isIncomplete=…)`
     If `Ctrl+Space` produces **no** entry, the keystroke isn't
     reaching CodeMirror (IBus / IME hijack, focus elsewhere).
     If only the first appears, `lspCompletionSource` was
     short-circuited (no path bound or no language id for the
     file extension).
5. Toolbar: `Pause` toggles auto-scroll-to-tail; manual scroll
   up auto-pauses; scrolling back to the bottom resumes. `Clear`
   empties both the local buffer and the backend ring for that
   source. `Close` removes the tab.
6. Open `lsp.rust` (or any source) before any rust LSP traffic,
   leave it open, then open a `.rs` file in the same window.
   The empty pane should populate live without re-opening the
   tab.

## What must keep working

- Existing bottom-panel tabs (`compose_logs`, terminal) render
  unchanged. The new `kind: 'diag'` slot doesn't share state
  with either.
- LSP status pill in the status bar still updates on every
  transition — the logs panel is a side-channel, not a
  replacement.
- Pre-existing `cargo test` suite passes
  (`crates/moon-core/src/logs.rs` adds five unit tests; no
  existing test depends on the new module).
- Ring buffer cap (2000 entries per source) is enforced on both
  sides; opening a chatty source on a long-running session
  shouldn't unbounded-grow the renderer.

## How to verify the LSP-crash auto-recovery

1. Open a TS workspace, open any `.ts` file (`lsp.typescript`
   pill should be `Running`).
2. From a host terminal, find the tsgo process
   (`pgrep -f "@typescript/native-preview"` or look for `tsgo`
   in `ps`) and `kill -9 <pid>` it.
3. Within milliseconds: the status bar pill flips to
   `Crashed`; the `lsp.typescript` source in the diag panel
   shows `ERROR server died (…); slot evicted, next request
will re-spawn.`; the frontend's re-`open` for the active
   file fires (visible via a new `INFO starting server` then
   `INFO server ready on primary route` shortly after).
4. Press Ctrl+Space. Expected: completions return as normal,
   no `lsp client shut down` error. The pill goes back to
   `Running` on the next status event.

## How to verify the `format-on-save` source

1. Open any file in the bottom-panel's Logs → `format-on-save`
   source.
2. **Plain `.txt` (no formatter wired)**: Ctrl+S — expect:
   - `INFO save: <path> (formatter dispatch target = host)`
   - `INFO lint-staged: …no glob matched …` (or "no rules
     configured" if the workspace has no lint-staged config)
   - `INFO default formatter: no built-in rule for .txt`
   - `INFO no formatter configured for <path> (…); bytes left
as-is`
3. **`.ts` inside a lint-staged repo** (e.g. `moon-landing`):
   - `INFO save: <path> (formatter dispatch target = …)`
   - `INFO lint-staged: running `prettier --write -- …` in
<config_dir>` (plus the truncation note when the chain
     has more than one command)
   - `INFO lint-staged: `prettier …` succeeded in NNNms`
4. **Format failure** (rename `node_modules/.bin/prettier` to
   simulate "tool not found"):
   - `WARN lint-staged: `prettier …` failed (see warnings
above) in NNNms`

## Known limitations

- Only the LSP broker and the format-on-save pipeline emit
  backend log entries today. fs-watcher and the git layer
  have not yet been wired.
- The auto-recovery only re-`open`s the **active** file. If
  the user has multiple TS files open across splits and the
  server dies, only the active one is primed; the inactive
  buffers will re-open on next focus (same path `setActiveFile`
  takes through `lspOpen`). Acceptable for now — multi-doc
  replay belongs in a later iteration that tracks open paths
  at the broker level.
- Frontend-emitted entries (`editor.completion`, future ones)
  live in the renderer only — they don't ride through the
  backend's ring and so won't appear in a different window's
  panel. That's by design: per-process diagnostics for a
  per-process workspace.
- No persistence across launches. The panel is for live
  triage, not historical analysis.

## Related

- [specs/lsp.md](../lsp.md) — broker behaviour the panel
  surfaces.
- [crates/moon-core/src/logs.rs](../../crates/moon-core/src/logs.rs).
- [crates/moon-protocol/src/logs.rs](../../crates/moon-protocol/src/logs.rs).
- Sibling plan: [0067-multi-workspace-windows.md](0067-multi-workspace-windows.md)
  established the per-process model that the LogSink lives
  inside.
