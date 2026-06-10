# Phase 5.7 — Review comments

A per-folder review state layered on the
[Review changes tab](../test-plans/0074-review-changes-tab.md): inline review
comments (local-first drafts in the workspace session, anchored by content so
they survive edits and rebases, publishable to a GitHub PR as a single review
once the branch is up, then cleared locally) and reviewed-file "Viewed" marks
(content-pinned, so a new commit touching a ticked file auto-un-ticks just that
file) for reviewing a large diff across several sittings.

Architectural spec: [`review-comments.md`](../review-comments.md). Decision:
[ADR 0027 — local-first review comments](../decisions/0027-review-comments.md),
which also records why `git notes` was rejected. Deliberately-deferred features:
[`review-comments.md` § "What this deliberately doesn't do"](../review-comments.md#what-this-deliberately-doesnt-do).

## Sub-phases

### 5.7.0 — Schema + state plumbing (shipped)

`ReviewComment` / `ReviewAnchor` / `ReviewedFile` DTOs in `crates/moon-protocol`
(camelCase, `#[ts(export)]`), hand-mirrored in `src/lib/protocol.ts`. New
`review_comments: Vec<ReviewComment>` and `reviewed_files: Vec<ReviewedFile>`
fields on `FolderSession` (`#[serde(default)]`, snake_case on the wire like
`compare_baseline`). Per-folder CRUD on `WorkspaceState` proxying the active
`FolderState` (comment create / edit-body / delete / list-for-path; reviewed-file
tick / untick / is-reviewed), riding the existing `FolderSession` save/restore in
`state.svelte.ts`. A `git_blob_sha(path)` host method (or reuse of an existing
status field) to fingerprint reviewed files. No UI yet — verified through state
tests + a manual session.json round-trip.

**Acceptance:** a comment and a reviewed-file mark created in a test survive a
save/restore cycle; old sessions without the fields load clean; both are scoped
to the folder that created them.

### 5.7.1 — Composer + anchored widgets + re-anchoring (shipped)

Inline composer in `ReviewSection.svelte`: select line(s) on the working side
(or base side), a gutter "+" / keybinding opens a small markdown composer below
the selection; submitting creates a `ReviewComment` whose anchor carries `side`,
the line hint, the content `fingerprint`, and `baselineRev`. Anchored comments
render as inline widget cards (author, relative time, markdown body,
edit/delete). On every section rebuild, re-resolve each comment by fingerprint:
exact hit at the hint line → render; else scan ±`ANCHOR_SEARCH_RADIUS` and re-pin;
else mark **stale** (muted "this line changed" state, still editable/deletable,
never dropped). A per-section "Viewed" checkbox writes a content-pinned
`ReviewedFile` (blob SHA), collapses the section, and auto-clears + re-expands on
the next status refresh if the file's blob changed. Comment count, reviewed
progress (`3 / 12 reviewed`), and a disabled-for-now "Publish review →"
affordance in the sticky banner.

**Acceptance:** add single- and multi-line comments; edit a commented line so the
text shifts within the window and confirm the comment follows; delete the line
entirely and confirm the comment goes stale rather than vanishing; tick a file
reviewed, then change it (edit or commit) and confirm only that file un-ticks;
comments and marks persist across a tab/folder switch and an IDE restart.

### 5.7.2 — Publish to GitHub (shipped)

`WorkspaceHost::publish_pr_review` following the `git_permalink` five-step chain
(trait decl → `LocalHost` impl → `fs_publish_pr_review` command →
`src-tauri/src/lib.rs` registration → `ipc.ts`). `LocalHost` shells out to `gh`:
`gh pr view --json number,headRefOid,...` to resolve the PR + head SHA (non-zero
→ `NoPr`); reconcile each comment's anchor against `git show <headRefOid>:<path>`
by fingerprint (Clean / Drifted / Lost); post survivors as one atomic review via
`gh api --method POST .../pulls/{n}/reviews --input -` with `commit_id =
headRefOid`, `event = COMMENT`, and `line`+`side` (never legacy `position`).
On HTTP 200, delete posted comments locally; keep Lost ones. The banner CTA opens
a publish dialog (optional summary, a preview of post/drift/lost counts, the
no-PR create-PR link).

**Acceptance:** with an open PR for the branch, publish a couple of comments and
confirm they appear as one review on GitHub anchored to the right lines; confirm
the posted ones disappear locally and a "lost" comment (anchored to a line the PR
head changed out from under us) stays as a draft with a flag; with no PR, the
dialog shows the create-PR link and nothing posts.

## Out of scope (this phase)

Threading / replies / resolve, displaying other people's comments, non-GitHub
hosts, `gh pr create`, and `APPROVE` / `REQUEST_CHANGES` review events. The
`event` field and the host-specific publish seam are where those grow later — see
the spec's non-goals section.
