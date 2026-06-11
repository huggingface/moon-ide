# Review state — comments and reviewed-file marks

Two related capabilities layered on the
[Review changes tab](test-plans/0074-review-changes-tab.md), tracked as one
per-folder **review state**:

- **Review comments** — inline notes left while reading the diff, **local-first
  drafts** that live in the workspace session, anchored by content so they
  survive edits and rebases, then published to a GitHub PR as a single review and
  cleared locally.
- **Reviewed-file marks** — a per-file "Viewed" tick (GitHub's checkbox) so a
  large diff can be reviewed across several sittings. Content-pinned, so a new
  commit touching a ticked file auto-un-ticks just that file. Purely a local
  progress aid; never leaves the machine.

These are the first, deliberately GitHub-shaped steps toward a richer review
experience; the data model is built to grow.

Design rationale and the drift model are in
[ADR 0027](decisions/0027-review-comments.md). Phased build-out is Phase 5.7 in
[the roadmap](roadmap.md). This spec is the architectural contract: data shapes,
the anchoring algorithm, the publish flow, and the UI surface.

## Where comments live

A `ReviewComment` is a per-folder draft, persisted in
`FolderSession.review_comments` (`crates/moon-protocol/src/session.rs`, mirrored
by hand in `src/lib/protocol.ts`). Folder-scoped because each bound folder is its
own repo with its own review in flight — the same reasoning as
`compare_baseline` and `reviewRestore`.

```
ReviewComment {
  id: string            // ULID, assigned on create
  anchor: ReviewAnchor
  body: string          // markdown, the comment text
  createdAt: string     // RFC3339
}

ReviewAnchor {
  path: string          // workspace-relative, matches GitStatusEntry.path
  side: 'base' | 'working'   // GitHub LEFT (deleted/old) vs RIGHT (added/context)
  startLine: number     // 1-based, in the side's *current* text — a hint, see below
  endLine: number       // == startLine for single-line comments
  fingerprint: string   // hash of the trimmed anchored line text(s)
  baselineRev: string   // the merge-base / HEAD SHA the comment was written against
}
```

Reviewed-file marks live next to them in `FolderSession.reviewed_files`:

```
ReviewedFile {
  path: string
  reviewedRev: string   // blob SHA of the version that was ticked
  reviewedAt: string
}
```

Both schema fields are `#[serde(default)]`; old sessions without them load fine
(no migration, per AGENTS.md).

> Why not `git notes`? It's the native "attach metadata to code" mechanism, but
> it anchors to whole objects (not lines), can't anchor pre-commit working-tree
> state, and is a shared/pushable repo ref — wrong for private, line-level,
> delete-on-publish drafts. Full rationale in
> [ADR 0027 § "Considered and rejected: git notes"](decisions/0027-review-comments.md#considered-and-rejected-git-notes).

## Anchoring: content, not coordinates

`startLine`/`endLine` are a fast-path **hint**, never the source of truth. The
truth is `fingerprint` — a hash of the trimmed text of the anchored line(s). On
every `ReviewSection` rebuild, each comment for that file is re-resolved:

1. If the text at `startLine..=endLine` on the comment's `side` still hashes to
   `fingerprint`, render there. (Hot path; no scan.)
2. Otherwise scan a window of ±`ANCHOR_SEARCH_RADIUS` lines for a run matching
   `fingerprint`. If found, re-pin `startLine`/`endLine` to the new location and
   re-render. The persisted hint updates lazily on next save.
3. If not found anywhere in the window, the comment goes **stale**: rendered in a
   muted state with a "this line changed" affordance, still editable and
   deletable, **never silently dropped**.

This mirrors the review tab's scroll-restore approach (test plan 0074): re-derive
position from content rather than trusting a frozen coordinate. The match is
deliberately dumb — exact trimmed-text equality within a window, then give up.
No fuzzy/semantic matching; `fingerprint` is the seam to upgrade if the team ever
wants smarter relocation.

Comments default to the `working` side (GitHub `RIGHT`) — you comment on the code
as it will land. `base`-side comments are supported (GitHub allows commenting on
deleted lines) but the gutter UI nudges toward the working side.

## Reviewed-file marks

A `ReviewedFile` records that the user ticked a file at a specific content state.
`reviewedRev` is the **blob SHA of the ticked version** — `git hash-object` of the
working-tree file when it has uncommitted changes, or the committed blob SHA
otherwise. We reuse git's object identity instead of hashing bytes ourselves;
resolving it is a cheap call that fits the existing git-status refresh.

On every git-status refresh, each `ReviewedFile` is re-validated against the
file's _current_ blob SHA:

- **Match** — render the row ticked.
- **Mismatch** — the file changed since it was ticked (a local edit, a new local
  commit, or a pulled commit). The mark **auto-clears**: the row returns to
  unreviewed and the stale `ReviewedFile` is dropped. Only the files that moved
  un-tick; the rest of the review state is untouched.

This is the file-granularity twin of comment re-anchoring: same "re-derive
validity from content" principle, coarser fingerprint. Marks are independent of
comments (tick a file with no comments; comment on an unticked file) and are
**never published** — they're a local progress aid only.

## UI surface

Comments can be added from three surfaces, all driven by the same CM extension
(`src/lib/editor/reviewComments.ts`, host glue via its `ReviewWiring`
controller):

- **The Review changes tab** — both panes of every section (base side = GitHub
  `LEFT`, working side = `RIGHT`). Always available; opening the tab is already
  an explicit "I'm reviewing" signal.
- **The regular editor and the diff view's working pane** — gated on
  `workspace.isReviewableBranch`: the branch has an open PR, or is any branch
  other than the default. On the default branch with no PR the affordances stay
  out of the way entirely (no gutter, no keybinding).

The add-comment gutter reserves a **fixed-width column** so the hover "+"
appearing never shifts the code horizontally.

- **Add a comment.** Hover any line and click the gutter **"+"** (shown only on
  the hovered row), or select one or more lines and press `Ctrl+Alt+C`. Either
  opens a small inline composer below the line(s); submitting creates a
  `ReviewComment` with the anchor derived from the line range. Bodies render as
  markdown (the existing `renderMarkdown` pipeline).
- **Render.** Anchored comments show as inline widgets pinned below their line(s)
  inside the MergeView, styled like a review thread card: author (always the
  user), relative time, markdown body, edit/delete controls. Stale comments get
  the muted treatment described above.
- **Mark reviewed.** Each section header gets a "Viewed" checkbox. Ticking writes
  a `ReviewedFile` for the current blob SHA; the section collapses by default (the
  user has signed off on it) but stays expandable. A new commit that changes the
  file re-expands it and clears the tick on the next status refresh. The banner
  shows progress (`3 / 12 reviewed`) so cross-sitting reviews have a sense of how
  much is left.
- **Review summary.** The sticky banner (`Review changes · vs <base> · N files`)
  also gains a comment count and a **"Publish review →"** CTA when there are
  unposted comments. The CTA opens a small dialog: an optional review-summary
  `body`, a preview of which comments will post / drift / can't be placed, and a
  Publish button.
- **No-PR state.** If `gh pr view` finds no PR for the branch, the dialog shows
  "No open PR for `<branch>`" and links to the create-PR URL the SCM panel
  already builds (`<repo>/pull/new/<branch>`). Comments stay as local drafts.

State plumbing follows the existing review pattern: comment CRUD lives on
`WorkspaceState` proxying the active `FolderState` (alongside `reviewVisibleFile`,
`requestReviewScroll`, etc.); persistence rides the existing `FolderSession`
save/restore in `state.svelte.ts`.

## Publish flow

Publishing is one new `WorkspaceHost` method, `publish_pr_review`, following the
five-step chain `git_permalink` uses (trait decl in `crates/moon-core/src/host.rs`
→ `LocalHost` impl → `fs_publish_pr_review` Tauri command → register in
`src-tauri/src/lib.rs` → `ipc.ts` wrapper). The `LocalHost` impl shells out to
`gh` — never a raw token or `reqwest` — inheriting the host-`gh`-token auth model
(`detect_host_gh_token`, see [containers.md](containers.md)). GitHub-only.

Steps inside `publish_pr_review`:

1. **Resolve the PR + head SHA.**
   `gh pr view --json number,headRefOid,baseRefName,headRefName,state`. Non-zero
   exit → return a `NoPr` result with the branch name; the UI shows the create-PR
   CTA. The `headRefOid` is the PR head SHA and is the only `commit_id` we ever
   pass to GitHub.
2. **Reconcile drift against the PR head.** For each comment, fetch the PR-head
   version of its file (`git show <headRefOid>:<path>`) and re-resolve the anchor
   by `fingerprint`. Per comment:
   - **Clean** — fingerprint found, line known in the head version. Include.
   - **Drifted** — found at a different line than the local hint. Re-pin to the
     head-version line silently. Include.
   - **Lost** — fingerprint absent at the head version (a commit we don't have
     locally changed that line). **Exclude**; report back so the UI can flag
     "N comments couldn't be placed on the current PR head". They remain local
     drafts.
3. **Post one atomic review.**
   `gh api --method POST /repos/{owner}/{repo}/pulls/{n}/reviews --input -` with:
   ```json
   {
   	"commit_id": "<headRefOid>",
   	"event": "COMMENT",
   	"body": "<optional summary>",
   	"comments": [
   		{ "path": "...", "line": 42, "side": "RIGHT", "body": "..." },
   		{ "path": "...", "start_line": 10, "start_side": "RIGHT", "line": 14, "side": "RIGHT", "body": "..." }
   	]
   }
   ```
   One review event, one notification — not N standalone inline comments. We use
   `line`+`side` (and `start_line`+`start_side` for multi-line), never the legacy
   `position` offset, so anchoring is by file line at `commit_id`, exactly what
   we reconciled in step 2.
4. **Clear published comments locally.** On HTTP 200, delete the comments that
   posted (Clean + Drifted) from `FolderSession.review_comments`. Lost comments
   stay. The session save fires as usual.

`commit_id` is resolved immediately before posting, so the only race is a push
landing between step 1 and step 3 — GitHub's own 422 validation catches that, and
the affected comments simply stay local for a retry.

### Why the local diff and GitHub's diff can differ

The local review tab diffs against the merge-base; GitHub diffs against a base
ref that may have moved. We do **not** reconcile the two diffs — that would mean
reimplementing GitHub's diff engine. We only guarantee each published comment's
anchored line text exists at the PR head, which is precisely what GitHub
validates for `line`+`side` comments. Surrounding hunk context can differ
harmlessly.

## Protocol shapes

New DTOs in `crates/moon-protocol/src/git.rs` (or a new `review.rs` module if it
grows), `#[serde(rename_all = "camelCase")]`, `#[ts(export)]`, hand-mirrored in
`src/lib/protocol.ts`:

- `ReviewComment`, `ReviewAnchor`, `ReviewedFile` (above) — the `FolderSession`
  fields (`review_comments`, `reviewed_files`) use Rust field names (snake_case)
  on the wire, matching `compare_baseline`.
- `PublishReviewRequest { body: Option<String>, comments: Vec<ReviewComment> }`.
- `PublishReviewResult` — a tagged enum: `NoPr { branch }` |
  `Published { posted: u32, lost: Vec<String> /* comment ids */, reviewUrl }`.

## What this deliberately doesn't do

Carried from [ADR 0027](decisions/0027-review-comments.md):

- **No threading / replies / resolve**, and **no displaying other people's
  comments**. This is a write-my-own-review tool, not a GitHub review-thread
  client.
- **No GitLab / Bitbucket / Gitea** — GitHub-only, matching `remote_web_url`.
- **No `gh pr create`** — link to the create-PR URL and stop.
- **No APPROVE / REQUEST_CHANGES** initially — every review posts as
  `event: COMMENT`. The `event` field is the seam to add them later.
- **No round-tripping published comments back into the IDE** — once posted, the
  local copy is deleted and we don't poll GitHub.
