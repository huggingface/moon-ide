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

Scope is deliberately minimal — `HEAD` vs working tree only, no
staging / no branch compare / no per-hunk accept — matching what
the team actively needs right now.

**Git-change gutter** in the regular editor (test plan
[0033](../test-plans/0033-git-change-gutter.md)). A dedicated
CodeMirror gutter diffs the live buffer against the cached `HEAD`
blob (`jsdiff::diffLines`) and paints a thin green bar for added
lines, a thin blue bar for modified lines, and a red wedge at the
top / bottom of the line bordering a pure deletion. Recomputes on
every transaction so the markers stay in sync as the user types;
the `HEAD` cache itself re-fetches whenever `refreshGitStatus`
runs (covering external commits / checkouts). A matching overview
ruler overlays the right-edge scrollbar with scaled-down,
clickable change markers so the user can jump to any diff region
in the file at a glance. Deleted buffers keep rendering in diff
view and suppress the inline gutter.

### 5.3 — SCM panel

**Test plans**:
[0037](../test-plans/0037-revert-icon-and-utf8-save.md),
[0052](../test-plans/0052-folder-bar-status.md),
[0062](../test-plans/0062-commit-to-new-branch.md),
[0064](../test-plans/0064-git-auto-fetch.md). Status: shipped
(piecewise; the panel grew across several plans).

The right-side-of-the-folder-bar SCM panel:

- Branch label (or short HEAD SHA in the detached-HEAD case),
  open-PR button when the upstream is a recognised host and the
  branch isn't `main` / `master`, revert-all icon, change-count
  pill that doubles as the "filter to changes only" toggle.
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
HEAD --no-color` patch capped at ~16 KB (new
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

### 5.4 — Search ignores `.git/`

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
