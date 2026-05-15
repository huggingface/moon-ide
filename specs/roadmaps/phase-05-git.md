# Phase 5 — Git

`gix`-based status / blame / diff plus a focused SCM panel. Tree
decorations ride [Pierre](https://github.com/pierrecomputer/trees)'s
built-in git status indicators; inline blame lives in CodeMirror;
the diff view is `@codemirror/merge` (CM6's native side-by-side
merge view, editable working-tree side); the SCM panel handles
commit / amend / sync / publish / revert.

This file owns the work breakdown for Phase 5 — what's landed,
what's outstanding, and the cross-cutting tree-marker contract. The
main [`roadmap.md`](../roadmap.md) keeps a one-paragraph summary and
a link here.

## Tree-marker contract

The file tree's git decorations ride Pierre's built-in `gitStatus`
API. We hand it `{ path, status: 'added' | 'modified' | 'deleted' }`
via `tree.setGitStatus(entries)`; folder bubble-up
(`data-item-contains-git-change="true"`) and per-row attributes
(`data-item-git-status="…"`) come for free.

The behaviours we layer on top:

- **Deleted rows stay visible.** Pierre only renders paths in the
  tree's `paths` array, so we hand it
  `union(workdir, status_only_deletions)` — deleted-but-not-committed
  entries persist with their `deleted` marker until the deletion is
  committed, breaking VSCode's convention of dropping them.
  Restoring is `git checkout HEAD -- <path>` (palette command);
  after the working tree matches HEAD the next refresh strips the
  ghost row.
- **Renames** map naturally to a `deleted` row at the old path and
  an `added` row at the new path; we don't try to be cleverer than
  git here.
- **Conflicts** can't ride Pierre's three-state model; surface them
  in the SCM panel and the editor gutter, and leave the tree row in
  whatever working-tree state it actually has.

Refresh runs on fs-watch events plus an explicit `setGitStatus`
call after any moon-ide-issued git op. Once the change reaches a
commit, the markers and ghost rows disappear in the same refresh
tick — no stale state surviving across commits.

Gitignored directories (`node_modules/`, `target/`, `dist/`) are
**collapsed by default** and faded, so noise doesn't render
thousands of entries on first paint. Expanding one is still cheap
and remembered for the session.

Until this phase fully lands, the file tree shows everything
except the `.git/` directory itself. Dotfiles like `.editorconfig`
and `.husky/` are real working files and stay visible by design.

## Sub-phases

This phase ships incrementally; each landed milestone has its own
test plan (linked below). The remaining sub-phases are tracked in
"Still outstanding" at the bottom rather than as separate
acceptance blocks — they're individually small.

### 5.0 — Tree markers + porcelain status

**Test plans**:
[0020](../test-plans/0020-file-tree-gitignore-fade.md),
[0021](../test-plans/0021-file-tree-full-git-status.md),
[0022](../test-plans/0022-discard-file-changes.md). Status:
shipped.

- Tree markers via Pierre's `gitStatus` for added / modified /
  deleted / untracked / ignored, backed by `git status
--porcelain=v1` with a `WalkBuilder` fallback for non-repo
  folders.
- Deleted rows stay visible by union-ing git's `deleted` set into
  the tree's `paths` array, matching the contract above.
- Auto-refresh: a `notify::RecommendedWatcher` rooted at the
  active folder emits debounced `fs:changed` Tauri events;
  window-focus events are a second-class fallback for when inotify
  is exhausted or the folder lives on NFS / SSHFS. Palette has
  "Refresh File Tree" as a manual escape hatch for the integrated
  terminal.
- Per-row "Discard changes" via a hover / right-click context menu
  on changed rows: routes modified + deleted through `git restore
--source=HEAD --staged --worktree` and untracked rows to the OS
  trash, confirming every time. First consumer of Pierre's
  `composition.contextMenu` API, via a reusable
  `ContextMenu.svelte` popover.

### 5.1 — Inline blame

**Test plan**: [0029](../test-plans/0029-inline-git-blame.md).
Status: shipped.

GitLens-style: a dim `author, relative-date • summary` badge sits
at end-of-line for the caret's current row, and hovering the badge
opens a tooltip with the full author, commit date, short hash, and
commit subject. Backed by `WorkspaceHost::git_blame` /
`fs_git_blame` shelling out to `git blame --porcelain -w`.
Uncommitted edits render as `Uncommitted changes`; blame refreshes
on save. Stale across live edits by design — the widget is a
glance, not a ground truth.

### 5.2 — Diff view

**Test plans**: [0032](../test-plans/0032-git-diff-view.md),
[0033](../test-plans/0033-git-change-gutter.md),
[0035](../test-plans/0035-diff-view-codemirror-merge.md),
[0036](../test-plans/0036-diff-view-single-tab-toggle.md),
[0053](../test-plans/0053-diff-full-file-and-overview.md). Status:
shipped.

Diff view via `@codemirror/merge`. `HEAD` content is pulled via a
new `fs_git_head_content` command (`git show HEAD:<path>`);
`DiffView.svelte` builds a `MergeView` with the HEAD blob
(read-only) on the left and the working-tree buffer (editable) on
the right. Both editors share the rest of the editor's chrome —
language extension, theme, editorconfig, highlight-tabs — so the
diff feels like the regular editor side-by-side, not a separate
component to learn.

**Single-tab + mode toggle**: each open buffer flips between the
regular editor and the diff view via `workspace.diffModes`
(per-folder `Set<string>`), with toggle surfaces at:

1. A `Source` / `Preview` / `Diff` tri-state in the right-edge tab
   toolbar.
2. `Ctrl/Cmd+Shift+D`.
3. The file-tree context menu's `View diff` entry.
4. Clicks on per-line markers in the editor's git-change gutter.
5. The palette command **Git: Toggle Diff View** (title flips with
   mode).

Deleted rows are always in diff view (no editor counterpart). Edits
on the right side go through the same `updateText` / `saveActive`
path the editor uses — flip into diff, fix the line, flip back —
because the diff and editor share one OpenFile buffer. LSP / blame
/ goto-def stay on the editor view (one `didOpen` per path); the
diff view is intentionally a viewer + light-edit surface. The HEAD
side picks up external `git commit` / `checkout` via the existing
`headByPath` cache.

Scope expanded once: a per-folder **compare baseline**
(`'head'` / `'default'`, persisted in
`FolderSession.compare_baseline`) swaps the diff view's "before"
side and the file tree / change-gutter / SCM-filter status source
between two views of "what's different right now":

- `'head'` (default) — working tree vs `HEAD`, the regular
  per-commit gutter & status view.
- `'default'` — working tree vs the merge-base with the repo's
  default branch (`origin/main` / `origin/master` from the
  existing `defaultBranchRemoteRef` resolver). The file tree
  paints `(M)` / `(A)` / `(D)` against main, the SCM "filter to
  changes only" pill stacks on top to focus the tree on
  changed-vs-main paths, and the diff view's "before" side is
  the merge-base blob — exactly the view the user sees when
  reviewing their own PR. Untracked files are absent from
  `git diff <merge-base>` so they don't appear in this mode (the
  user hasn't committed them yet — they're "not part of the
  branch").

The `'default'` mode silently degrades to `'head'` semantics
(but the toggle stays sticky) when the host can't compute the
diff — no resolvable default branch, HEAD detached, on the
default branch itself, or no merge-base. Backed by
`WorkspaceHost::git_default_branch_diff` (returns
`Option<BranchDiffStatus { merge_base, default_branch_ref,
entries }>`) and a generalised `git_ref_content(rev, path)` —
`git_head_content` is now a thin wrapper that passes
`"HEAD"`. The merge-base SHA is cached on
`FolderState.defaultBranchMergeBase` so each open buffer's
"before"-side fetch is a single `git show <sha>:<path>`. No
per-hunk accept yet — same scope discipline as before.

**Git-change indicator** in the regular editor (test plan
[0033](../test-plans/0033-git-change-gutter.md), later switched
to line-number cell tinting). Diffs the live buffer against the
cached `HEAD` blob (`jsdiff::diffLines`) and paints the
line-number gutter cell with a tinted background — green for
additions, blue for modifications, red top/bottom border on the
adjacent line for pure deletions. The earlier dedicated wedge
gutter is gone (one less column to track); we reuse the
line-number column the eye already lands on, GitHub-style. Same
classes (`cm-gitline-added` / `cm-gitline-modified` /
`cm-gitline-deleted-above` / `cm-gitline-deleted-below`) cover
the diff view and the aggregated review pseudo-tab via
`diffGutterTintExtension` so all three surfaces share one
visual vocabulary. Recomputes on every transaction so the
indicator stays in sync as the user types; the `HEAD` cache
itself re-fetches whenever `refreshGitStatus` runs (covering
external commits / checkouts). A matching overview ruler
overlays the right-edge scrollbar with scaled-down, clickable
change markers so the user can jump to any diff region in the
file at a glance. Deleted buffers keep rendering in diff view
and suppress the inline indicator.

### 5.3 — SCM panel

**Test plans**:
[0037](../test-plans/0037-revert-icon-and-utf8-save.md),
[0052](../test-plans/0052-folder-bar-status.md),
[0062](../test-plans/0062-commit-to-new-branch.md),
[0064](../test-plans/0064-git-auto-fetch.md),
[0065](../test-plans/0065-scm-update-from-main.md). Status: shipped
(piecewise; the panel grew across several plans).

The right-side-of-the-folder-bar SCM panel:

- Branch label (or short HEAD SHA in the detached-HEAD case),
  open-PR button when the upstream is a recognised host and the
  branch isn't `main` / `master`, revert-all icon, an off-by-
  default `vs <default-branch>` pill (flips the per-folder
  compare baseline — see §5.2), and the change-count pill that
  doubles as the "filter to changes only" toggle. The `vs main`
  pill suppresses itself on the default branch and when no
  default branch resolves; it stacks orthogonally with the
  changes-only filter.
- **Review changes** entry point. When the baseline is
  `'default'` and there's at least one entry, a "stacked diff"
  icon button appears just before the `vs main` pill. Click
  opens (or focuses) the **Review changes** pseudo-tab — a
  scrollable stack of per-file diff sections against the
  merge-base, the same view a reviewer would see on a PR. See
  §5.4 for the tab itself.
- **Periodic auto-fetch.** `WorkspaceHost::git_fetch` shells out
  to `git fetch --quiet --no-tags` with prompts disabled
  (`GIT_TERMINAL_PROMPT=0`, blanked `GIT_ASKPASS` /
  `SSH_ASKPASS`) and a 30s deadline. The frontend wires a
  3-minute periodic loop (matches VSCode / Cursor's
  `git.autofetchPeriod` default), an initial fetch ~5s after
  startup, plus focus / folder-switch nudges throttled to a 30s
  minimum. Fetch only moves remote-tracking refs, so the followup
  is just `refreshGitBranch` (cheap) — the SCM panel's "Sync
  Changes" button surfaces from the refreshed `gitBranch.behind`
  count without any other refresh fanout. Failures (offline,
  auth, no upstream) downgrade to backend `tracing::debug!`; the
  loop pauses when the document is hidden.
- **Split commit button** `[Commit ...] [⎇] [✎]` — main label
  flips between `Commit` / `Amend` / `Commit to new branch`
  based on toggle state; toggles share the button's right edge
  with collapsed borders. Branch + amend are mutually exclusive
  (`setAmend` / `setNewBranch` flip the other off). Branch-mode
  reveals the branch-name input above the commit row.
- **Amend prefill.** Toggling amend on with an empty composer
  fetches `git log -1 --pretty=%B` (new
  `WorkspaceHost::git_head_commit_message` /
  `fs_git_head_commit_message`) and seeds the textarea. The
  bytes are tracked in `amendPrefill` so toggle-off only
  un-prefills when the user never edited them.
- **AI commit message** sparkle inset top-right of the textarea.
  `coder_suggest_commit_message` feeds the fast model
  (`DEFAULT_FAST_MODEL`) with the user's draft + a `git diff
HEAD --no-color` patch capped at ~64 KB (new
  `WorkspaceHost::git_diff_patch`); response cleaned via
  `sanitise_commit_message` (single line, drop labels / quotes /
  trailing period, clamp to 100 chars).
- **AI branch name** sparkle on the branch-name input (same
  surface as the commit-message sparkle, paired with the
  branch-name field).
- **Sync spinner.** The Sync Changes button rotates its refresh
  icon and flips the label to `Syncing…` while a pull / push
  roundtrip is in flight; same treatment for `Publishing…` on
  `Publish Branch`. Same accent-colored spinner appears next to
  the commit button label while busy.
- **Update from main.** Secondary outlined button below `Sync
Changes` that surfaces when the repo's default branch
  (`origin/HEAD` → `origin/main` → `origin/master`) has commits
  the current branch's HEAD doesn't, and we're not on the default
  branch ourselves. Drives `git merge --no-edit <remote_ref>` via
  `WorkspaceHost::git_merge_default_branch` /
  `fs_git_merge_default_branch`. The remote ref + behind count
  ride on the existing `git_branch` IPC as
  `defaultBranchRemoteRef` / `defaultBranchBehind` so no extra
  round-trip is needed. Conflicts / dirty-tree refusals propagate
  git's stderr verbatim via flash; an in-app abort affordance is
  a later concern (Phase 5's full conflict UI).
- **Branch switcher.** Cmd+Shift+B (and a click on the branch
  label) opens a Cmd+P-style palette listing recent local
  branches plus open GitHub PRs in a single filterable list.
  Local rows come from `git for-each-ref refs/heads
--sort=-committerdate` (cap 20); PR rows come from `gh pr list`
  (cap 30) on the host (no container routing today — the
  LocalHost binds the active folder's `.git` the container would
  see anyway). Backend lives in `WorkspaceHost::branch_list` /
  `branch_switch`; the `BranchSwitchTarget` discriminator picks
  `git switch <name>` vs `gh pr checkout <number>` so cross-fork
  PRs work without manual remote / fetch fiddling. The PR
  section's emptiness carries a `PrListStatus` so the empty-state
  row is the right hint: _Install gh_ / _gh is signed out_ /
  _gh pr list failed: …_ — the `notGithub` case suppresses the
  section entirely rather than rendering "no PRs". The free-text
  filter spans branch name, commit subject, PR number, title,
  author, and head ref so type-to-filter is the primary
  navigation gesture.

  PR rows are filtered by a per-folder `pr_scope` (persisted in
  `FolderSession.pr_scope`, surfaced as an `All PRs` /
  `Participating` toggle in the palette's hint bar). `All` mirrors
  the unfiltered `gh pr list --state open`. `Participating` runs
  two `gh pr list --search` calls in parallel —
  `state:open involves:@me` (author / assignee / mentioned /
  commenter) and `state:open review-requested:@me` — and merges
  by PR number, sorted by raw `updatedAt` so the merged list
  reads chronologically. Persistence is per folder so a busy
  monorepo can stay in `Participating` without dragging a
  side-project's palette into the same filter.

  Container `gh` shares the host's auth via the `~/.config/gh`
  read-only bind mount (see `specs/containers.md`).

### 5.4 — Review changes (aggregated diff)

**Status**: shipped (no test plan).

`Diff view` (§5.2) opens one file's changes at a time. When the
user wants to look at _the whole branch_ (their own PR, before
opening it on GitHub), per-file flipping is too slow — and the
existing changes-tree gives a list but not a "scroll through every
diff" surface. The Review changes pseudo-tab is that surface.

**Entry point.** The SCM panel header paints a stacked-diff icon
button immediately to the left of the `vs <default>` pill,
visible only when

- the per-folder compare baseline is `'default'` (so we have a
  merge-base SHA and `gitStatusEntries` already reflects
  branch-vs-base), and
- there's at least one changed entry.

Click opens or focuses the review tab in the current pane.

**The tab itself.** A synthetic `OpenFile` keyed on
`review://default-branch` (`isReviewPath()` in
`src/lib/util/reviewPath.ts`). The path uses a non-filesystem
scheme so every gate that would otherwise route to the host
(LSP open / update / close, blame, HEAD fetch, editorconfig,
persistence, format-on-save) skips it via the unified
`isSyntheticBufferPath` helper. The synthetic buffer carries
empty bytes; all data flows in through reactive reads of
`workspace.gitStatusEntries` and
`workspace.defaultBranchMergeBase` inside `ReviewView.svelte`.

`EditorPane.svelte` recognises the prefix and mounts
`ReviewView` instead of `Editor` / `DiffView`. The view renders
a scrollable stack of `ReviewSection`s, one per non-ignored
entry — each a read-only `MergeView` with left = merge-base
blob (`ipc.fs.gitRefContent(mergeBase, path)`) and right = the
open buffer text (so unsaved edits show up in the review) or a
fresh `readFile` if the buffer isn't loaded.

**Unchanged regions collapse.** Each section runs `MergeView`
with `collapseUnchanged: { margin: 3, minSize: 5 }` so long
runs of identical lines fold behind a clickable `… N unchanged
lines` placeholder. Opposite trade-off from `DiffView`
(single-file mode, where the change-bar gutter and overview
ruler already locate the diff and the placeholder gets in the
way of `Ctrl+F` / scroll): in the aggregated view a 30-file
branch with a 2000-line file changed in 20 places would
otherwise force the reader to scroll past acres of unchanged
code between sections. `margin: 3` matches `git diff -U3`;
`minSize: 5` keeps small gaps between adjacent hunks expanded
so they read continuously.

A single tab per folder; clicking the SCM button while the
review is already open just focuses it. Closing the tab GCs the
synthetic buffer through the same `closeFile` path real buffers
use.

**Performance: lazy-mount.** The first two sections build their
`MergeView` eagerly so the user sees content immediately.
Everything else mounts on first viewport hit via
`IntersectionObserver` with a `rootMargin: 50%` pre-build window,
and _stays mounted_ once built — scroll position and fold state
survive a scroll-away. On a 100-file PR that's the difference
between "review tab is the new welcome screen" and "review tab
opens snappily".

**TOC: SCM tree doubles as navigation.** When the review tab is
visible in some pane (`workspace.isReviewTabVisible`),
`FileTree.svelte`'s `activateRowFromTree` reroutes click events
in `mode === 'changes'`: instead of opening that file as a new
editor tab, it calls `workspace.requestReviewScroll(path)` which
bumps a `{ path, tick }` signal on `WorkspaceState`. `ReviewView`
watches the signal and `scrollIntoView`s the matching section.
The `tick` field makes repeat-same-path clicks re-trigger the
effect — same pattern as `focusTick`. Plain (`mode === 'all'`)
tree clicks keep their open-as-editor behaviour.

**Keyboard nav inside the view.** `n` / `p` (terminal-pager style)
and `Alt-ArrowDown` / `Alt-ArrowUp` jump between adjacent file
sections. The listener sits on the scroll container; events that
originate inside a CodeMirror editor are ignored (so `n` keystrokes
in CM's search panel keep their normal meaning). The container
auto-focuses on mount so the bindings work without a click.

**What the review tab is not.** Read-only by design: editing
happens by opening the file in a normal editor tab (header has
an "open" affordance — clicking the path opens the file). No
per-hunk staging, no inline comments, no LSP wiring on the
diff editors. We skip those because

- editing diffs in-place is what `DiffView` is for (single file,
  editable right side); the review tab is the bird's-eye view,
- LSP `didOpen` per file would explode broker traffic on large
  PRs for zero new signal — the regular Editor view on each
  file already provides full LSP when needed, and
- the per-file `MergeView` is heavy enough that doubling it up
  with completion / hover popovers would push the lazy-mount
  budget into perceptible jank.

If those become real itches we revisit with hard numbers, not
speculation.

### 5.5 — Search ignores `.git/`

**Status**: shipped (no test plan).

Both walkers in `crates/moon-core/src/search.rs` set
`hidden(false)` so dotfiles like `.editorconfig` surface — that
also accidentally walked into `.git/logs/`, `.git/objects/`, etc.
and drowned `Ctrl+Shift+F` results in pack-file noise. Fixed by
adding `!.git/` via `OverrideBuilder` on top of the existing
`.git_ignore(true)` / `.git_exclude(true)` flags, so user
`.gitignore` exclusions (`node_modules/`, `target/`, …) keep
being respected for repos.

## Still outstanding

- **Conflict markers.** Tree row state when a file is in conflict
  (`UU`, `AU`, etc.). Probably surfaces as a fourth Pierre
  status colour plus an editor gutter widget; the SCM panel's
  conflict-resolution flow is what drives the design.
- **Per-hunk stage / discard.** Today's `Revert all` /
  `Discard changes` are file-level only. Per-hunk requires the
  diff view to surface a chooser; the architecture doesn't
  preclude it but no one's asked yet.
- **Unstage.** Discarding a staged-new file currently leaves the
  file on disk but staged. The "discard fully" gesture should
  unstage in the same pass.
- **Push / pull failure recovery UX.** Today: flash toast,
  user reads git's stderr verbatim, retries. A more guided
  surface (rebase mid-pull, force-with-lease toggle, etc.) is
  worth it once these failure modes get noisy in practice.

## Cross-references

- Architecture: [`architecture.md`](../architecture.md) §
  WorkspaceHost.
- ADRs: [0002 — workspace host](../decisions/0002-workspace-host.md)
  (every git op rides `WorkspaceHost`, which keeps Phase 2's
  containerised workspace working without re-routing).
- Frontend: [`frontend.md`](../frontend.md) for the diff view's
  CodeMirror / `@codemirror/merge` choice.
