# Test plan 0074: Review changes pseudo-tab

- **Date**: 2026-05-15
- **Phase**: post-Phase 5 SCM polish

## What shipped

- A new **Review changes** pseudo-tab opens an aggregated diff of every file changed against the active folder's default-branch merge-base. Same view a reviewer would see when looking at the user's own PR, but inline in the IDE.
- Entry point: a stacked-diff icon button in the SCM panel header, immediately left of the `vs <default>` pill. Visible only when the per-folder compare baseline is `'default'` and there's at least one changed entry.
- Routing: synthetic `OpenFile` keyed on `review://default-branch` (`src/lib/util/reviewPath.ts`). `EditorPane.svelte` recognises the prefix and mounts `ReviewView.svelte` instead of the regular `Editor` / `DiffView`. A new unified `isSyntheticBufferPath` helper gates LSP open / update / close, blame, HEAD fetch, persistence, and format-on-save against both `untitled:` and `review://` — drops 9 inline `startsWith('untitled:')` checks in `state.svelte.ts`.
- Per-file section: a read-only `MergeView` with merge-base content on the left (via `ipc.fs.gitRefContent`) and the open-buffer text (or a fresh `readFile`) on the right. Sticky header with status badge, dim-dir / bright-name path label (click to open the file as a normal editor tab), and a caret collapse/expand toggle.
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
5. Scroll down. Expected: lazily-mounted sections render their `MergeView` as they enter the viewport (you can confirm by widening the window mid-scroll so a previously-collapsed section comes into view, then watching for the `Loading diff…` → diff transition — happens once per section, fast). Each section shows only the changed hunks plus ~3 lines of context; long unchanged runs collapse behind `… N unchanged lines` placeholders. Click a placeholder to expand that region in place.
6. Open the SCM panel's changes-tree (click the change-count badge to filter), and click any file row. Expected: the review tab scrolls smoothly so that file's section sits at the top. Click the same row a second time. Expected: smooth scroll re-fires (no "second click is silently dropped" feel).
7. With focus on the review view (click anywhere outside an editor pane inside it, or it auto-focuses on first mount), press `n` repeatedly. Expected: the view scrolls section-by-section down the list. Press `p`: walks back up. Try `Alt+ArrowDown` / `Alt+ArrowUp`: same behaviour.
8. Click a section header's path label (the `dir/name` text). Expected: the file opens as a normal editor tab in the active pane — pushed onto nav history.
9. Click the `▾` caret on a section header. Expected: that section collapses to header-only height; click again to expand. The diff state survives the round-trip (collapse → scroll → expand re-renders the same diff with the same fold / scroll state).
10. Make an edit to one of the modified files in a regular editor tab without saving, then switch back to the review tab and re-scroll its section into view. Expected: the right-hand pane shows the unsaved bytes (open buffer takes precedence over disk).
11. Commit those changes so the entry leaves `gitStatusEntries`. Expected: the matching section disappears from the review tab on the next status refresh. If every entry is gone the tab shows `No changes against main.`
12. Close the review tab via its `×` button. The synthetic `OpenFile` is GC'd. Press `Alt+Left` to navigate back through history. Expected: history re-opens the review tab (re-creating the synthetic `OpenFile` via the `isReviewPath` short-circuit in `openFile`).
13. Switch the per-folder compare baseline back to `'head'`. Expected: the review icon disappears from the SCM panel header. If the review tab is currently open, leave it as-is (the tab is keyed off the default-branch baseline at open time; flipping the baseline doesn't auto-close the tab — the user closes it).
14. Open the same folder in a _split_ pane (drag a tab to split), keep the regular Editor in one pane, the review tab in the other. Switch focus between panes. Expected: `n` / `p` in the review pane scrolls its sections; the same keystrokes in the Editor pane behave as plain text input (the listener correctly bails when the event originates inside a CM editor).

## What must keep working

Regression checks.

- Untitled buffers (`Ctrl+N`) keep working exactly as before — same LSP-skip, blame-skip, no-persist semantics. The renamed `isSyntheticBufferPath` helper still answers `true` for them.
- Plain editor tabs and the per-file diff view (`Ctrl+Shift+D`) keep their existing behaviour. The new entry button only renders when `compareBaseline === 'default'` _and_ `changeCount > 0`.
- Plain (`mode === 'all'`) file-tree clicks always open the row as an editor tab, even when the review tab is open in some pane. Only the changes-only tree re-routes clicks into the review's scroll signal.
- The `vs main` pill still toggles the per-folder compare baseline and the changes-tree status source on its own — clicking it doesn't open the review tab.
- Closing the last copy of the review tab GCs the synthetic buffer through the normal `closeFile` path. No stale `review://...` entry in `openFiles` after close. `persistAppState` never writes a `review://` path into the session JSON.
- LSP servers are not pinged with `didOpen` for `review://default-branch`. The review tab is a viewer, not an LSP context — confirm with `RUST_LOG=moon_core::lsp=debug bun dev` and a quick scroll through the review.

## Known limitations

- **Read-only.** The review tab doesn't let you edit. To fix something, click the section header's path to open the file in a regular editor tab. We considered making the right pane editable (mirroring `DiffView`) but the bird's-eye view stays simpler if editing lives on a different surface; revisit if anyone actually asks.
- **No per-hunk staging or comments.** Not in scope for this commit — the same scope discipline as the rest of Phase 5.
- **No LSP / hover / completion in the diff editors.** Issuing `didOpen` per file would explode broker traffic on a 50-file branch for almost no value (the user clicks through to the regular editor when they want LSP). The pure-add / pure-delete decoration plugin (`diffPureChangeExtension`) is still wired so the highlight stays sane.
- **Single baseline.** Only the `'default'` baseline opens a review; `'head'` (working tree vs `HEAD`) doesn't get an aggregated view because the existing per-file diff is faster for the single-commit case. Could be generalised later if a real need surfaces.
- **No `+N −M` line counts in the section headers** — would require an extra diff pass per file to count line additions / deletions. Skipped until someone asks; the MergeView itself already paints the per-line gutter once the section mounts.
- **No persistence of the review tab across launches.** The `review://` path is filtered out of `persistAppState`, same as `untitled:`. Reopen the IDE and the review tab is gone; click the icon again to bring it back. Cheaper than tracking "was the tab open?" across a baseline that might have shifted overnight (commits / pulls / branch switches).

## Related

- Specs: [`specs/roadmaps/phase-05-git.md`](../roadmaps/phase-05-git.md) § 5.4 (Review changes), [`specs/frontend.md`](../frontend.md) § "Diff and conflict surfaces".
- Prior test plans: [0032](0032-git-diff-view.md), [0034](0034-diff-view-dedicated-tab.md), [0035](0035-diff-view-codemirror-merge.md), [0036](0036-diff-view-single-tab-toggle.md), [0053](0053-diff-full-file-and-overview.md), [0070](0070-diff-mode-goto-def.md).
- ADRs: [0005 — bootstrap](../decisions/0005-bootstrap.md) (reviewing moon-ide's own branches inside moon-ide is bootstrap, hence the in-scope status).
