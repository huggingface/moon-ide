# Test plan 0074: Review changes pseudo-tab

- **Date**: 2026-05-15
- **Phase**: post-Phase 5 SCM polish

## What shipped

- A new **Review changes** pseudo-tab opens an aggregated diff of every file changed against the active folder's compare baseline — the default-branch merge-base in `'default'` mode (same view a reviewer would see when looking at the user's own PR) or `HEAD` in `'head'` mode (the equivalent of opening every changed file's per-file `DiffView` at once).
- Entry point: a stacked-diff icon button in the SCM panel header, immediately left of the `vs <default>` pill (or on its own when the pill is hidden, e.g. on the default branch itself). Visible whenever there's at least one changed entry, regardless of the active baseline.
- Routing: synthetic `OpenFile` keyed on `review://default-branch` (`src/lib/util/reviewPath.ts`). `EditorPane.svelte` recognises the prefix and mounts `ReviewView.svelte` instead of the regular `Editor` / `DiffView`. A new unified `isSyntheticBufferPath` helper gates LSP open / update / close, blame, HEAD fetch, persistence, and format-on-save against both `untitled:` and `review://` — drops 9 inline `startsWith('untitled:')` checks in `state.svelte.ts`.
- Per-file section: a read-only `MergeView`. Left side comes from the merge-base blob (`ipc.fs.gitRefContent`) in default-branch mode or the `HEAD` blob (`ipc.fs.gitHeadContent`) in working-tree mode; right side is the open-buffer text (so unsaved edits show through) or a fresh `readFile`. The active baseline is woven into the section's `(path | mergeBase)` key so toggling the SCM `vs <default>` pill remounts every section against the right "before" content. Sticky header with status badge, dim-dir / bright-name path label (click to open the file as a normal editor tab), and a caret collapse/expand toggle.
- Long runs of unchanged lines collapse behind a `… N unchanged lines` placeholder (`collapseUnchanged: { margin: 3, minSize: 5 }`) so the reader scrolls past hunks, not whole files. Opposite trade-off from the single-file `DiffView`, which expands everything because its gutter / overview ruler already locate the changes.
- Performance: first two sections build their `MergeView` eagerly; everything else lazy-mounts on `IntersectionObserver` hit with a `rootMargin: 50%` pre-build window, and stays mounted once built so scroll position / fold state survive scroll-away.
- TOC: the SCM changes tree doubles as navigation. When `workspace.isReviewTabVisible` is true, clicking a file row in the `mode === 'changes'` tree calls `workspace.requestReviewScroll(path)` instead of opening that file as a new editor tab; `ReviewView` watches the `{ path, tick }` signal and scrolls the matching section into view.
- Keyboard: `n` / `p` and `Alt-ArrowDown` / `Alt-ArrowUp` jump between adjacent file sections; the scroll container auto-focuses on mount.

## How to test

Prerequisites: any git repo with an `origin/main` (or `origin/master`) the current branch isn't on. The moon-ide repo itself works once you're on a feature branch.

1. Run `bun run check`, `bun run lint`, `bun run fmt`. Expected: clean.
2. Launch `bun dev`. Open a folder that's on a feature branch with several changed files vs `main`. The SCM panel should display the `vs main` pill **off** by default and the changes pill should show the working-tree change count.
3. Click the `vs main` pill once. It turns active; the changes badge now counts merge-base-vs-HEAD entries and the file tree paints `(M)` / `(A)` / `(D)` badges against the merge-base. **A new icon button (two side-by-side rectangles)** appears immediately to the left of the `vs main` pill. Hover: tooltip reads `Open aggregated diff against main` (or whatever the default branch is).
4. Click the icon. A new `Review changes` tab opens in the active pane and immediately shows a banner row (`Review changes · vs main · N files`) and stacked diff sections for each changed file. Each section header shows a status badge (A/M/D/U), the path, and a `▾` caret.
5. Scroll down. Expected: lazily-mounted sections render their `MergeView` as they enter the viewport (you can confirm by widening the window mid-scroll so a previously-collapsed section comes into view, then watching for the `Loading diff…` → diff transition — happens once per section, fast). Each section shows only the changed hunks plus ~3 lines of context; long unchanged runs collapse behind `… N unchanged lines` placeholders, each suffixed with the enclosing definition of the code that follows (e.g. `… 37 unchanged lines │ export function foo(...)`) when a heuristic match is found. Click a placeholder to expand that region in place.
6. Open the SCM panel's changes-tree (click the change-count badge to filter), and click any file row. Expected: the review tab scrolls smoothly so that file's section sits at the top. Click the same row a second time. Expected: smooth scroll re-fires (no "second click is silently dropped" feel).
7. With focus on the review view (click anywhere outside an editor pane inside it, or it auto-focuses on first mount), press `n` repeatedly. Expected: the view scrolls section-by-section down the list. Press `p`: walks back up. Try `Alt+ArrowDown` / `Alt+ArrowUp`: same behaviour.
8. Click a section header's path label (the `dir/name` text). Expected: the file opens as a normal editor tab in the active pane — pushed onto nav history.
9. Click the `▾` caret on a section header. Expected: that section collapses to header-only height; click again to expand. The diff state survives the round-trip (collapse → scroll → expand re-renders the same diff with the same fold / scroll state).
10. Make an edit to one of the modified files in a regular editor tab without saving, then switch back to the review tab and re-scroll its section into view. Expected: the right-hand pane shows the unsaved bytes (open buffer takes precedence over disk).
11. Commit those changes so the entry leaves `gitStatusEntries`. Expected: the matching section disappears from the review tab on the next status refresh. If every entry is gone the tab shows `No changes against main.`
12. Close the review tab via its `×` button. The synthetic `OpenFile` is GC'd. Press `Alt+Left` to navigate back through history. Expected: history re-opens the review tab (re-creating the synthetic `OpenFile` via the `isReviewPath` short-circuit in `openFile`).
13. Switch the per-folder compare baseline back to `'head'`. Expected: the review icon stays in the SCM panel header (its tooltip flips to `Open aggregated diff against HEAD`); if the review tab is currently open, every section remounts against `HEAD` and the banner reads `Review changes · vs HEAD · N files`. Flip back to `'default'`: sections remount again, banner flips to `vs main`. Now switch to a folder that's _on_ its default branch (or any branch where no upstream default resolves) and stage some changes: the `vs <default>` pill is hidden but the review icon is still there — clicking it opens the aggregated working-tree diff against `HEAD`.
14. Open the same folder in a _split_ pane (drag a tab to split), keep the regular Editor in one pane, the review tab in the other. Switch focus between panes. Expected: `n` / `p` in the review pane scrolls its sections; the same keystrokes in the Editor pane behave as plain text input (the listener correctly bails when the event originates inside a CM editor).

## What must keep working

Regression checks.

- Untitled buffers (`Ctrl+N`) keep working exactly as before — same LSP-skip, blame-skip, no-persist semantics. The renamed `isSyntheticBufferPath` helper still answers `true` for them.
- Plain editor tabs and the per-file diff view (`Ctrl+Shift+D`) keep their existing behaviour. The entry button renders whenever `changeCount > 0`; with `changeCount === 0` it disappears entirely (no orphan icon).
- Plain (`mode === 'all'`) file-tree clicks always open the row as an editor tab, even when the review tab is open in some pane. Only the changes-only tree re-routes clicks into the review's scroll signal.
- The `vs main` pill still toggles the per-folder compare baseline and the changes-tree status source on its own — clicking it doesn't open the review tab.
- Closing the last copy of the review tab GCs the synthetic buffer through the normal `closeFile` path. No stale `review://...` entry in `openFiles` after close. `persistAppState` never writes a `review://` path into the session JSON.
- LSP servers are not pinged with `didOpen` for `review://default-branch`. The review tab is a viewer, not an LSP context — confirm with `RUST_LOG=moon_core::lsp=debug bun dev` and a quick scroll through the review.

## Known limitations

- **Read-only.** The review tab doesn't let you edit. To fix something, click the section header's path to open the file in a regular editor tab. We considered making the right pane editable (mirroring `DiffView`) but the bird's-eye view stays simpler if editing lives on a different surface; revisit if anyone actually asks.
- **No per-hunk staging or comments.** Not in scope for this commit — the same scope discipline as the rest of Phase 5.
- **No LSP / hover / completion in the diff editors.** Issuing `didOpen` per file would explode broker traffic on a 50-file branch for almost no value (the user clicks through to the regular editor when they want LSP). The pure-add / pure-delete decoration plugin (`diffPureChangeExtension`) is still wired so the highlight stays sane.
- **Two baselines, same surface.** The review tab serves both `'default'` (vs the default-branch merge-base) and `'head'` (vs the last commit) — the active baseline is implied by the SCM panel's `vs <default>` pill state. There's no third "vs arbitrary commit" baseline yet; revisit if anyone asks.
- **No `+N −M` line counts in the section headers** — would require an extra diff pass per file to count line additions / deletions. Skipped until someone asks; the MergeView itself already paints the per-line gutter once the section mounts.
- **No persistence of the review tab across launches.** The `review://` path is filtered out of `persistAppState`, same as `untitled:`. Reopen the IDE and the review tab is gone; click the icon again to bring it back. Cheaper than tracking "was the tab open?" across a baseline that might have shifted overnight (commits / pulls / branch switches). In-session restore is different and _is_ supported — see below.

## Sticky header + in-session scroll restore

Two later refinements layered onto the original aggregated view:

- **Sticky banner with the current file.** The banner row (`Review changes · vs <base> · N files`) is `position: sticky` at the top of the scroller and shows the path of the file whose section is currently nearest the top (sourced from `workspace.reviewVisibleFile`, the same pointer the review icon's "back to file" toggle reads). Even when a tall diff fills the whole viewport and that section's own header has scrolled past, the banner still answers "which file am I looking at?". Each section's own `position: sticky` header parks just below the banner via a shared `--m-review-banner-h` custom property (also fed into `scroll-margin-top` so `scrollIntoView` from the SCM tree / `n` / `p` doesn't tuck a header under the banner).
- **Scroll restore across tab and folder switches.** `ReviewView` snapshots `{ path, offset }` on unmount — the nearest section's path plus the signed pixel offset into it (a path-relative offset survives the lazy MergeView rebuild that an absolute `scrollTop` would not). On the next mount it eager-builds every section up to the restore target (so their heights are settled) and re-applies the offset across animation frames until layout stops shifting, then stops. Any genuine interaction (`wheel` / `keydown` / `pointerdown`) aborts the settle loop.
  - **Per-folder.** The snapshot lives on `FolderState.reviewRestore`, not on `WorkspaceState`, so each bound folder keeps its own review position. `ReviewView` reads/writes it through `workspace.reviewRestoreFor(folder)` / `setReviewRestoreFor(folder, …)` keyed off the folder it captured **at mount** — not the live active-folder pointer, because `onDestroy` fires _after_ a folder switch has already flipped `active_folder`, and the live pointer would stash this folder's position under the next folder's state. `captureRestore` walks the mounted `sectionEls` directly (not the reactive `entries`, which may have already flipped to the new folder during teardown).
  - **Lifetime.** Survives a tab switch (the synthetic buffer stays in this folder's `openFiles`) and a folder switch (the folder's `FolderState` is preserved in `folderStates`). Dropped in `closeFile` when the review tab is actually closed, so a fresh open starts at the top.
  - To verify: (a) scroll a few files deep into a multi-file review, switch to a regular editor tab, switch back — lands where you left it, same sections already rendered (no flash to top, no re-lazy-load of the sections above). (b) With two folders bound, open a review in folder A scrolled to file 5, switch to folder B and open _its_ review scrolled to file 2, switch back to A — A restores to file 5, B stays at file 2. (c) Close the review tab via its `×` and reopen — starts at the top.

## Related

- Specs: [`specs/roadmaps/phase-05-git.md`](../roadmaps/phase-05-git.md) § 5.4 (Review changes), [`specs/frontend.md`](../frontend.md) § "Diff and conflict surfaces".
- Prior test plans: [0032](0032-git-diff-view.md), [0034](0034-diff-view-dedicated-tab.md), [0035](0035-diff-view-codemirror-merge.md), [0036](0036-diff-view-single-tab-toggle.md), [0053](0053-diff-full-file-and-overview.md), [0070](0070-diff-mode-goto-def.md).
- ADRs: [0005 — bootstrap](../decisions/0005-bootstrap.md) (reviewing moon-ide's own branches inside moon-ide is bootstrap, hence the in-scope status).
