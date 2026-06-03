# Test plan 0076: folder-switch latency

- **Date**: 2026-05-15
- **Phase**: Phase 2.5 (multi-folder) — performance follow-up

## Results & status

Eight ships across one session, captured against six DevTools timeline recordings on a real moon-ide ⇄ moon-landing swap. Numbers are wall-clock from click → first painted frame, measured by the in-app `moon:setActiveFolder.firstFrame` mark.

| Recording | Ships landed before it                                                                      | Click → firstFrame         | Top single recalc          | Forced-layouts (total) | `rAF` requested |
| --------- | ------------------------------------------------------------------------------------------- | -------------------------- | -------------------------- | ---------------------- | --------------- |
| rec 1     | baseline (this session's first record)                                                      | 1.78 s                     | 240 ms                     | 177 ms                 | many            |
| rec 2     | 1–4 (LSP detach, fire-and-forget `loadPaths`, gitignore prune, `resetPaths` for empty→full) | 2.10 s                     | 270 ms                     | 214 ms                 | 117             |
| rec 3     | 5 (`*Panel.update` marks)                                                                   | 1.91 s                     | 270 ms                     | similar                | 117             |
| rec 4     | 6 (markdown memo)                                                                           | **1.29 s** (−33% vs rec 3) | 310 ms                     | 214 ms                 | **9**           |
| rec 5     | 7 (viewport-gated cold-cache render)                                                        | 1.50 s                     | **204 ms** (−34% vs rec 4) | 341 ms                 | 6               |
| rec 6     | 8 (shared `IntersectionObserver`)                                                           | 1.53 s                     | 230 ms                     | 295 ms (−14%)          | 6               |

Out-of-band wins not captured by the table because they only show up on the very-first folder swap in a session (when the LSP teardown was serial-blocking the IPC):

- LSP detach on a folder running TS + rust-analyzer + tailwind used to gate the snapshot return on 6–12 s of serialised shutdowns. Now `0` ms on the swap path; the broker re-spawns lazily on the next `lsp_*` request (ship 1).
- `applyPathsDiff` against 82,468 paths was 6.8 s; the same data through `resetPaths` is ~1 s (ship 5); and pruning gitignored descendants drops the path count to ~25k on moon-ide so even that 1 s is now ~50 ms (ships 4 + 5).
- The 117-callback `rAF` storm that the markdown render fan-out used to schedule per swap is gone (ship 6).

### Where the remaining ~1.5 s goes

By rec 6 the residual cost is split across three buckets, none of which is "moon-ide code we own":

1. **Pierre tree virtualization measure passes** — `pierre/trees/dist/render/FileTreeView.js` has 13 layout-read sites, all running after `resetPaths` to recompute visible rows. Cluster of small forced-layouts at 6.5–6.8 s in rec 6 is consistent with this.
2. **CodeMirror's post-`setState` measure pass** — schedules requestMeasure callbacks after the editor swaps state. They fire during the cascade and force layout to compute decoration positions.
3. **One ~230 ms style recalc** triggered when one of the above reads layout while many invalidations are still pending. Style invalidations are scoped to ~3 events per swap now (down from ~10 in rec 3), but their cost compounds because each touches a high-fanout selector.

### Recommendation

Call it done as-is. The original reports of multi-second stalls on large repos turn into ~1.5 s on the same workload, and the architecture now scales with viewport / workload instead of with transcript or path-list length. Three things on the bench if a fresh regression shows up:

1. ~~**Apply `visibleOnce` to `ToolBody*` components**.~~ **Done** — but via a simpler gate than first-intersection. Tool rows render collapsed by default, and a `<details>` keeps its slotted children mounted even while closed, so every `ToolBody*` was paying its grammar-load + syntax-highlight pass on first paint regardless of viewport. `CoderPanel.svelte` now renders a tool row's body only after its `<details>` has been opened at least once (tracked in a `SvelteSet<rowId>`, `ontoggle`), so a collapsed transcript does zero highlighter work on load no matter how many tool calls it holds. Cleared on session/transcript swap. This subsumes the `visibleOnce`-on-tool-bodies idea: collapsed-by-default already implies off-screen-and-closed, and first-open is a stronger gate than first-intersection (an on-screen-but-collapsed row pays nothing either).
2. **Defer `Editor.setState` one frame past `firstFrame`**. Would push CodeMirror's measure pass out of the cascade. Risk: editor renders blank for one frame, which most users won't notice but some will.
3. **Capture a sampling profile** (toggle "JavaScript samples" in the WebKit Inspector before recording). Would pinpoint which functions are inside the residual style recalcs. Recommended before doing more guess-driven refactoring.

## What shipped

Thirteen coordinated changes to take the user-perceived stall out of the folder-bar click on large projects. (1)–(3) were ship 1; (4)–(5) were ship 2; (6)–(7) were ship 3; (8)–(9) were ship 4; (10) was ship 5; (11) was ship 6; (12) was ship 7; (13) is ship 8. Ship 5's `moon:*Panel.update` marks pin-pointed CoderPanel's transcript fan-out as the trigger for the 270 ms style recalc; ship 6 (markdown memo) measurably moved the dial — a fresh recording with the cache in place shrunk the swap from 1.91 s → 1.29 s (−33 %) and collapsed the `rAF` storm from 117 callbacks per swap to 9. Ship 7 attacks the remaining cold-cache cost via a viewport gate, ship 8 makes that gate cheap by collapsing N per-row `IntersectionObserver` instances into one shared observer so the browser can batch the initial bounding-rect reads.

1. **Backend (`src-tauri/src/commands/workspace.rs`)**: `workspace_set_active_folder` (+ `_open_local` + `_remove_folder`) snips the old `LspHandle` out of the mutex synchronously and detaches `broker.shutdown_all()` onto a `tokio::spawn`. The IPC roundtrip used to block on every running LSP server's 2 s shutdown + 2 s child wait timeouts, serialised — on a folder with TS + rust-analyzer + tailwind running that's a 6–12 s freeze before the snapshot returns. The new broker spawns lazily on the next `lsp_*` request regardless of whether the old child has reaped.
2. **Frontend (`src/lib/state.svelte.ts`)**: `adoptWorkspaceSnapshot` now fires `loadPaths()` as `void this.loadPaths()` instead of `await`-ing it. Every caller of the snapshot apply path (folder switch, folder open, folder remove, startup hydrate) was previously gated on a full recursive backend walk + a Pierre tree refresh before any post-hydration step could run. The `loadingPaths` flag is still there for any UI that wants to surface a spinner, and the tree reactively swaps in once the walk lands.
3. **Frontend (`src/lib/components/FileTree.svelte`)**: the path-set effect now resets its `lastTreePaths` cursor when `workspace.activeFolderPath` changes, forcing the next run into `tree.resetPaths(merged)` instead of `applyPathsDiff`. Without this the effect double-fires on every folder switch — first with `prev=N old paths, next=[]` (Pierre eats an `N`-entry remove `batch`), then with `prev=[], next=M new paths` (Pierre eats the `M`-entry add `batch`). `resetPaths` rebuilds the path store wholesale and skips the per-op event emission `batch` runs, which on a ten-thousand-file repo is the difference between a single-digit-ms tree swap and a multi-hundred-ms one.
4. **Backend (`crates/moon-core/src/host.rs`)**: `collect_paths` now runs a one-shot `git status --porcelain=v1 -z --ignored=matching` before the walk to learn which directories git would collapse to a single `!! dir/` row (`node_modules/`, `target/`, `build/`, `dist/`, `.venv/`, … — whatever the repo's `.gitignore` covers as a whole directory). The walker emits each such directory as a single collapsed entry and **skips its descendants entirely**. Before: moon-ide's tree fed Pierre 126,806 paths, ~100k of which were `node_modules/**`. After: ~25k. Non-repo folders see an empty skip set and walk everything — the safe default since there's no authoritative ignore source to consult. Two new unit tests pin both branches.
5. **Frontend (`src/lib/components/FileTree.svelte`)**: the path-set effect's wholesale-rebuild branch now also fires when the previous snapshot was empty and the next one isn't. The initial `loadPaths` after a fresh mount, and every folder-switch post-fill, both hit this case (the prior effect run resets `lastTreePaths` to the empty set). Measured: applying 82,468 paths through `applyPathsDiff` took 6.8 s; switching to `resetPaths` on the same data drops to roughly 1 s.
6. **Frontend (`src/lib/state.svelte.ts`)**: `adoptWorkspaceSnapshot` now emits per-phase ms (`blame`, `assignWs`, `coder`, `folderStates`, `tail`, `total`) on each call. Pairs with the existing `setActiveFolder` line so the user can localise which sub-step inside the sync portion of the snapshot apply is eating wall time.
7. **Backend (`crates/moon-core/src/host.rs`) + frontend (`src/lib/components/FileTree.svelte`)**: lazy descendant fetch for gitignored directories. A new `fs_collect_paths_under(rel, max_depth)` IPC walks a single subtree without the gitignore-collapse filter; the file tree calls it with `max_depth=0` when the user expands a collapsed-ignored row (`node_modules/`, `target/`, …) and batch-adds the direct children. Each loaded sub-directory is itself recorded as lazy so drilling deeper re-issues the command at the next level. Loaded paths are unioned back into the merged path set on every refresh so the `applyPathsDiff` doesn't churn them; folder switches reset the bucket. Net effect: `node_modules/` is now browsable on demand without ever inflating the steady-state path count.
8. **Frontend (`src/lib/components/FileTree.svelte`)**: structural-equality skip in the path-set effect. The effect's reactive deps (`workspace.paths`, the deleted-paths signature, `scmFilterPaths`, `activeFolderPath`) flip in independent microtasks during a folder swap, so the effect re-runs 2–3 times per tree mode before the cascade settles. Most echo runs compute an identical `merged` because the relevant slice didn't actually change; without a skip Pierre still does the full `resetPaths` / `applyPathsDiff`, and the resulting shadow-DOM churn lands downstream as 200+ ms `recalculate-styles` events. Comparing `nextSet` to `lastTreePaths` (size + every-member check) before the Pierre call cuts the duplicated runs (observed: 4 per swap → 2) and the cascading style recalcs they drag in.
9. **Frontend (`src/lib/components/FileTree.svelte`)**: hidden-mode skip. Both `'all'` and `'changes'` trees stay mounted (CSS-toggled) so the SCM filter toggle is instant, but until this change a folder switch paid the full `resetPaths` cost twice — once for the visible tree and once for the hidden one that nobody was looking at. The path-set effect now short-circuits with `lastTreePaths = null` whenever its `mode` doesn't match `workspace.scmFilterOn`. The next visibility flip catches up via a one-shot `resetPaths(merged)` (`lastTreePaths === null` already takes that branch), which is acceptable: toggling the filter is a deliberate gesture, not a hot path.
10. **Frontend (`CoderPanel.svelte`, `ScmPanel.svelte`, `EditorPane.svelte`)**: post-update `performance.mark` calls (`moon:coderPanel.update`, `moon:scmPanel.update`, `moon:editorPane.{left,right}.update`) fire from a top-level `$effect` whose reactive deps are the panel's coarse signals (active-folder path, transcript rows, view mode, git status, scm filter, active file, view-mode flags). Pure instrumentation — no semantic change. With these marks laid down, a fresh DevTools recording lets you eyeball which panel's reconciliation lands closest to each big `recalculate-styles` event in the layout track; the panel that mutates the most DOM during a swap is the one whose marker timestamp clusters with the recalcs. The third recording's marks landed in order — `scmPanel.update` first, then `fileTree.update`, then `editorPane.left.update`, with `coderPanel.update` last at 19.518 s — and the 270 ms recalc fired ~270 ms later. That pin-pointed CoderPanel's transcript fan-out as the trigger, motivating ship 6.
11. **Frontend (`src/lib/markdown.ts` + `src/lib/components/CoderMarkdown.svelte`)**: module-level memo of rendered markdown HTML. `markdown.ts` keeps a FIFO `Map` keyed by `(linkify, source)` capped at 500 entries; `renderMarkdown` populates it on success, and a new `getCachedMarkdown` sync accessor lets `CoderMarkdown` skip the entire `rAF` + async `renderMarkdown` dance on a hit. On a folder swap back to an already-visited session, every `CoderMarkdown` instance now picks the cached HTML out of the map inside the same Svelte flush as its mount and assigns `html` synchronously — no rAF batch, no Promise-resolve fan-out, no N concurrent `{@html}` swaps landing in one frame. Cache misses still go through the original async pipeline so first-visit cold paths and streaming deltas are unchanged. The dirty in-flight token bump on the sync path makes sure a stale async resolve from the previous mount can't overwrite the cached value we just installed.
12. **Frontend (`src/lib/actions/visibleOnce.ts` + `src/lib/components/CoderMarkdown.svelte`)**: viewport-gated async render for cache misses. A new `visibleOnce` Svelte action wraps an `IntersectionObserver` (with a 400 px slop on either side of the viewport so the placeholder→content swap is invisible during normal scroll); `CoderMarkdown` uses it to set a sticky `visible` flag the first time the row scrolls into view. Cache misses now early-return without scheduling the rAF when `visible` is `false`; once the observer fires the effect re-runs and starts the render. The unrendered state shows a plain-text placeholder (`.coder-md-placeholder`, `white-space: pre-wrap`) that leaves the raw message in the DOM — `Ctrl+F` still finds the text, and the placeholder's wrapped layout approximates the formatted paragraph's height closely enough that the swap doesn't push surrounding rows around on most cases. Cache hits skip the gate (sync assignment lands during the same flush as the mount), and rows that are above the fold on mount intersect on the first observer pass so they render the same frame as the placeholder appears — visible rows pay roughly the same as before; off-screen rows pay zero until the user scrolls toward them. Net effect: a cold-cache transcript that previously fanned out 100 concurrent grammar loads and `{@html}` swaps now fans out one per viewport-worth of rows.
13. **Frontend (`src/lib/actions/visibleOnce.ts`)**: shared `IntersectionObserver` across all targets. The first ship-7 recording showed 70 short forced-layouts clustered during the transcript mount (~5 ms each), attributable to 70 per-instance `observer.observe()` calls each computing its own initial bounding rect. Switching to a single module-level observer and a `WeakMap<Element, callback>` lets WebKit batch those reads into one layout pass; the action still keeps its sticky semantics by `unobserve()`-ing and deleting the map entry on the first intersection. Per-action `destroy` paths still tear down cleanly on unmount.

Plus a profiling instrumentation pass so the next round of regressions doesn't require re-wiring timers from scratch:

- **Frontend `console.info`** in `setActiveFolder`, `adoptWorkspaceSnapshot`, `loadPaths`, `Editor.svelte`'s path-swap effect (only when it crosses 30 ms), and `FileTree.svelte`'s path-set effect. Each line tags the phase (`ipc`, `adopt`, `reactive+paint`, `toFirstFrame`, `blame`, `assignWs`, `coder`, `folderStates`, `tail`, `walk`, `assign`, `resetPaths` / `applyPathsDiff`, `build`, `setState`) and emits a single ms value per phase.
- **`performance.mark` / `performance.measure`** pairs under the `moon:` namespace at every phase boundary. Open DevTools > Performance, Record around a folder-switch gesture, then look at the "User Timing" track — every measure appears as a labelled bar so you can read the durations off the flame chart and correlate them with surrounding browser work (Svelte effect runs, paint, Pierre's tree work).
- **Backend `tracing::info!(target = "moon_profile", …)`** lines in `workspace_set_active_folder` and `fs_collect_paths` report per-phase ms (`set`, `snapshot`, `watcher`, `lsp_detach` on the active-folder swap; `require`, `walk` on the path collection). They print to stderr at the default `info` filter — visible in the terminal running `bun run tauri dev`. Use `RUST_LOG="moon=debug,moon_profile=info"` if you want to silence the rest of the log noise while keeping the perf lines.

## How to profile a slow folder switch

When a switch still feels slow, capture the breakdown before tweaking anything else. In the dev build, WebKitGTK's Web Inspector exposes a Timeline tab; in addition to the User Timing track (`moon:*` marks/measures), it captures every script execution, GC pause, forced layout, and style recalculation event with start time and duration. Export the recording (`File → Save Timeline Recording`) for offline analysis if needed — the file is plain JSON keyed by `recording.records[]`, `recording.markers[]`, and `recording.samples[0].stackTraces[]`. The two-recording diff workflow (one before a change, one after) is the most reliable way to confirm a tweak actually helped before pushing it.

For a quick read-out in-IDE:

1. Open the DevTools console (Ctrl+Shift+I in the dev build).
2. In a separate terminal, tail the `bun run tauri dev` output and grep for `moon_profile=`.
3. Click the folder bar of the slow target. You should see, in order:
   - Backend stderr — `workspace_set_active_folder path=… set=Xms snapshot=Ums watcher=Vms lsp_detach=Wms total=Tms`. `total` should be a handful of ms; anything else hides backend mutex contention or a synchronous detour we missed.
   - DevTools console — `setActiveFolder(…) ipc=Xms adopt=Yms reactive+paint=Zms toFirstFrame=Tms`. `ipc` matches the backend `total` plus IPC framing (~5–30 ms). `adopt` is the synchronous portion of `adoptWorkspaceSnapshot` (FolderState fan-out + workspace state assignment). `reactive+paint` is everything Svelte does after `await adoptWorkspaceSnapshot` returns up to the next animation frame — Svelte effect runs, derived recomputes, and browser layout + paint commit. `toFirstFrame` is end-to-end click → first painted frame.
   - DevTools console — `fileTree.update mode=… folder=… paths=N resetPaths=Dms` (or `applyPathsDiff=Dms`). This is Pierre's wall time for the path-set rebuild on the new folder.
   - Backend stderr — `fs_collect_paths folder=… require=Xms walk=Yms total=Tms count=N`. The recursive `read_dir` walk on the blocking pool.
   - DevTools console — `loadPaths(…) walk=Xms assign=Yms count=N`. `walk` includes IPC framing on top of the backend `total`; `assign` is how long it took Svelte's reactive flush to react to `this.paths = collected` (FileTree's path-set effect runs here, so big trees show their tree-rebuild time as part of this number).

4. If `toFirstFrame` is the dominant cost, open DevTools > Performance, Record across one folder switch, and look at the "User Timing" track for the `moon:setActiveFolder.*` / `moon:loadPaths.*` / `moon:fileTree.update` / `moon:coderPanel.update` / `moon:scmPanel.update` / `moon:editorPane.{side}.update` bars. The flame chart adjacent to them shows what JS / layout / paint work the browser is doing alongside; the panel marker that sits closest to a giant `recalculate-styles` event in the layout track is almost certainly the component whose reactive flush is mutating enough DOM to dirty the rest of the page.

## How to test

Prerequisites: `bun run tauri dev`, at least one large folder bound (capfi-international, moon-ide, or any repo with ~10k+ files), plus one smaller folder. A TypeScript or Rust project is ideal so an LSP is actually running when the switch fires.

### Folder switch happy path

1. Open a large folder. Wait for LSP to come up (status bar pill turns green or disappears).
2. Open ~3 TS / Rust files so multiple language servers are likely running.
3. Switch to the second folder via the folder bar. Open DevTools console.
4. Expected:
   - The new folder's bar / breadcrumb / tabs paint within ~200ms.
   - `moon-ide: setActiveFolder(...) ipc=Xms adopt=Yms total=Zms` lands in the console with `ipc` typically in the 5–60ms range. Before the fix this would have been multiple seconds.
   - The file tree paints empty briefly, then fills in once `loadPaths` resolves.
   - LSP status pill flashes "Stopped" then re-spawns on first interaction; no UI freeze.

### Folder switch back

5. Switch back to the first folder. Expected:
   - Tree paints near-instantly (paths were cached on the previous load).
   - `console.info` shows `adopt=<low ms>` since the path set is already populated.

### LSP correctness after detach

6. After a folder switch, open a TS / Rust file in the new folder, edit, save. Diagnostics arrive normally (the new broker built lazily). Hover and goto-definition both work.
7. Repeat 4–5 times quickly. No leaked LSP child processes (`ps -o pid,cmd -p $(pgrep tsserver)` should match the count of distinct active folders that have TS open buffers, not accumulate).

### Lazy-loaded gitignored directories

8. In the regular ('all') file tree view, locate the `node_modules/` row (or any gitignored directory the repo carries — `target/`, `dist/`, …). It paints with the standard "ignored" tint and no children visible.
9. Click the expand chevron. Expected: chevron flips, brief no-op frame, then the direct children of `node_modules/` appear one level deep. No console errors. The first expansion fires `fs_collect_paths_under` once; subsequent expand/collapse cycles on the same row don't re-fetch.
10. Drill into a sub-package (`node_modules/preact/` say). Same flow — direct children only, recursion happens one level at a time as the user keeps drilling.
11. Switch to a different folder, then back. The previous lazy-load state is cleared (the new folder has its own ignore set); `node_modules/` is collapsed again and re-expanding re-fetches.
12. With the file tree's SCM filter on (changes mode), gitignored directories are absent from the tree by definition — lazy load doesn't fire there. No regression for the all-changes flow.

### Markdown cache (ship 6)

13. Pick a folder with an existing coder session that has at least a handful of assistant replies (markdown-bearing rows). Open it. Wait for the transcript to render.
14. Switch to another folder, then back. Expected: transcript paints without the brief "empty body, then markdown pops in" flicker that the previous build had — `getCachedMarkdown` hits for every assistant row and `{@html}` lands in the same flush as the row mount.
15. Confirm streaming still works: send a fresh prompt in the same session. The assistant reply streams in deltas like before (cache miss on every new delta until the final state lands).
16. Confirm fenced code blocks still highlight correctly post-cache-hit (the cached HTML embeds the already-coloured spans, so colors must match the live editor for the same language).

### Viewport-gated markdown (ship 7)

17. **Cold-cache first visit**: hard-reload the dev build (`Ctrl+R` in DevTools) to clear the markdown memo, then open a folder with a long coder session that has plenty of assistant replies (≥ 30 rows). Click into the session view. Expected: transcript area lands quickly; the rows above the fold render their formatted markdown within a frame or two of the swap finishing; rows below the fold sit as plain-text placeholders. No console errors.
18. Scroll the transcript slowly downward. Each off-screen row swaps from placeholder → formatted markdown shortly before it enters the viewport (the 400 px `rootMargin` lead time means the swap happens out of sight on a normal scroll). No visible "pop-in" flash on typical scroll speeds; fast page-down may briefly show a placeholder before the async render resolves — acceptable.
19. **`Ctrl+F` still works on un-rendered rows**: with several rows still in placeholder state, open the browser find bar and search for a token you know exists in one of those off-screen messages. Expected: the match is found, the browser scrolls to the row, and the row swaps from placeholder → formatted markdown as the observer fires. Match highlight should re-anchor on the new DOM.
20. **Streaming into a freshly-mounted row**: send a new prompt while parked at the bottom of the transcript. The assistant reply mounts a new `CoderMarkdown` row, immediately intersects (it's on-screen), and streams in deltas. Each delta is a cache miss; the row renders normally (no placeholder flash since it's visible the whole time).
21. **No regression on cache-hit warm path**: navigate away from this session and back. Every row hits the cache and renders synchronously regardless of visibility — no placeholder should ever appear because `html` is non-empty before the first paint.

### Lazy tool-body render (tool-body follow-up)

22. **Cold load of a tool-heavy session**: open a folder with a coder session that has many `read_file` / `write_file` / `edit_file` / `grep` tool calls (≥ 30 tool rows, ideally over code with syntax highlighting). Click into the session. Expected: the transcript paints quickly; every tool row shows its collapsed one-line summary (dot, name, hint, status, elapsed) and no body. Open DevTools > Performance, record the click → paint, and confirm the row of highlighter / grammar-load work that used to fan out on mount is gone — `coderPanel.update → firstFrame` should not scale with the number of tool rows.
23. **First open renders the body**: click a collapsed `read_file` (or `write_file`) row. Expected: the `<details>` opens and the syntax-highlighted body renders within a frame or two (a brief plain-text frame before the grammar loads is acceptable, same as anywhere highlighting is async). The path header, line numbers, and colors match what the live editor shows for that language.
24. **Re-collapse keeps the body mounted**: collapse the row you just opened, then re-open it. Expected: the body reappears immediately with no re-highlight flash — the work was paid once and the body stays mounted across collapse/expand.
25. **Running tool expanded mid-flight**: send a prompt that triggers a long-running tool (e.g. a `bash` that sleeps, or a `task` sub-agent). Expand the running tool's row before it finishes. Expected: the body renders and updates live as the result lands (args show first, result fills in on completion) — opening a running row does not freeze its live updates.
26. **Session swap resets the gate**: open one tool-heavy session, expand a couple of rows, then switch to a different session (or folder) and back. Expected: every tool row is collapsed again and bodies are unmounted (the `openedToolRows` set cleared on the row-count reset); re-expanding re-renders cleanly. No console errors, no leaked highlighter work.

### Edge cases

17. **Welcome-state transition**: remove the only bound folder. The `workspace_remove_folder` path still detaches the LSP teardown; the welcome screen paints immediately.
18. **Same-folder click**: clicking the already-active folder is a no-op (`setActiveFolder` early-returns before the `performance.now()` capture).
19. **Failing IPC**: cosmic — kill the workspace backend mid-switch. The `flash` toast says "Could not switch folder: …" and nothing else changes.

## Out of scope

- Backend-side presort of paths so the frontend can use `preparePresortedFileTreeInput`. Worth revisiting if `loadPaths` itself becomes the new bottleneck — for now the dominant cost was the LSP shutdown wait, not the walk or the tree rebuild.
- Migrating away from Pierre's `batch` API to its `resetPaths(merged, { preparedInput })` for non-folder-switch incremental updates. Same idea — small win on individual save / git operations, large work to wire through, not the bottleneck the user reported.
- Persisting lazy-load expansion state across folder switches. Re-expanding `node_modules/foo/` after a folder-bar round-trip is cheap (single IPC, small batch), and persisting expansion plus loaded subtrees would complicate the `lazyLoaded` bookkeeping for marginal benefit.
- ~~Applying the same `visibleOnce` gate to `ToolBody*`~~ — landed as a follow-up, but as a lazy first-open mount rather than a visibility gate (see recommendation 1 above). Tool rows are collapsed by default, so gating the body on `<details>` first-open removes the entire mount-time highlighter cascade for every unexpanded row; an on-screen-but-collapsed row pays nothing, which a viewport gate would not have achieved. Test steps for the follow-up are in the "Lazy tool-body render" section below.
