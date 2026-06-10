# ADR 0027 — Local-first PR review comments

Date: 2026-06-13
Status: proposed

## Context

The "Review changes" pseudo-tab (test plan 0074, `ReviewView.svelte`) gives a
bird's-eye stacked diff of everything a branch changes against its baseline —
the default-branch merge-base in `'default'` mode, or `HEAD` in `'head'` mode.
It is the same view a reviewer sees when looking at a PR on GitHub, but today it
is **read-only-ish** (0086 added inline pane edits) and has **no notion of
review comments**.

The team wants to be able to leave review comments while reading the diff, the
same way they would on GitHub — but without leaving the IDE, and crucially
**before the work is pushed**. The common workflow is: review your own branch
locally, jot comments as you go (for yourself, or to drive an agent), and once
the branch is actually up as a PR, push those comments to GitHub as one review.

Two hard constraints shape the design:

1. **The comments exist before the PR does.** A comment is anchored to a line in
   a local diff that may not yet correspond to any commit GitHub knows about.
2. **Commit drift.** Between writing a comment and publishing it, the line it
   points at can move — because of local edits, new local commits, **or new
   commits pushed to the PR head on GitHub by someone else**. GitHub anchors
   review comments to a `(commit_id, path, line, side)` tuple; if our local line
   numbers don't match the PR head's diff, the publish fails or the comment
   lands "outdated" against the wrong line.

There is a second, closely related want: **marking files as reviewed** (GitHub's
"Viewed" checkbox), so a large diff can be reviewed across several sittings — and
when a new commit changes a file you'd already ticked, that tick auto-clears so
you re-review only what moved. This is the same anchoring problem as a comment,
just at _file_ granularity: a mark is valid for a specific content state and must
invalidate when the content drifts. We treat both as facets of one **per-folder
review state** rather than two unrelated features.

This ADR records how we resolve all of it. The protocol/IPC/storage shapes and
the UI surface live in [`specs/review-comments.md`](../review-comments.md); the
phased build-out is Phase 5.7 in [the roadmap](../roadmap.md). These are the
first steps — deliberately GitHub-shaped — toward a richer review experience;
the data model below is built to grow.

## Decision

### 1. Comments are local-first, stored per folder, published on demand

Review comments live in the workspace's persisted session — a new
`review_comments: Vec<ReviewComment>` field on `FolderSession`
(`crates/moon-protocol/src/session.rs`), mirrored by hand into
`src/lib/protocol.ts` per the existing convention. They are folder-scoped for
the same reason `compare_baseline` and `reviewRestore` are: each bound folder is
its own repo with its own review in flight.

They are **drafts**, not a parallel review system. There is no threading, no
resolve/unresolve, no replies to existing GitHub comments. A `ReviewComment` is
`{ id, path, anchor, body, createdAt }` — one author (the user), one body, one
anchor. Threading and replies are a deliberate non-goal (see below).

### 2. Anchoring is content-based, not line-number-based

A naive `(path, line)` anchor rots the instant the user edits the file or
rebases. We anchor instead to **the diff side plus a content fingerprint of the
anchored line(s)**:

```
ReviewAnchor {
  side: 'base' | 'working'   // LEFT vs RIGHT in GitHub terms
  startLine, endLine          // 1-based, in the side's current text — a hint
  fingerprint: string         // hash of the trimmed anchored line text(s)
  baselineRev: string         // the merge-base / HEAD SHA the comment was written against
}
```

The line numbers are a **hint for fast rendering**; the fingerprint is the
**truth for re-locating** the anchor after the text shifts. On every diff
rebuild `ReviewSection` re-resolves each comment: if the line at `startLine`
still matches `fingerprint`, render there; otherwise scan a small window
(±`N` lines) for the fingerprint and re-pin; if it can't be found at all, the
comment goes **stale** (rendered in a muted "this line changed" state, still
editable/deletable, never silently dropped). This is the same philosophy as the
review tab's scroll-restore (test plan 0074): re-derive position from content,
don't trust a frozen coordinate.

Anchoring to the **`working` side by default** (GitHub `RIGHT`) is the common
case — you comment on the code as it will land. `base`-side comments (GitHub
`LEFT`, commenting on a deleted/old line) are supported because GitHub supports
them, but the UI nudges toward the working side.

### 3. Publish translates local anchors to PR-head coordinates via `gh`

Publishing goes through a new `WorkspaceHost::publish_pr_review` method,
following the same five-step chain as `git_permalink` (trait decl → `LocalHost`
impl → `fs_*` Tauri command → `lib.rs` registration → `ipc.ts`). The `LocalHost`
impl **shells out to `gh`**, never a raw token or `reqwest` — consistent with
the existing `gh pr list` / `gh pr checkout` calls, and inheriting the
host-`gh`-token-forwarded-as-`GH_TOKEN` container auth model
([containers.md](../containers.md), `detect_host_gh_token`). GitHub-only, like
`remote_web_url`.

The publish flow is:

1. `gh pr view --json number,headRefOid,baseRefName,headRefName,state` — resolve
   the PR for the current branch and, critically, **`headRefOid`** (the PR head
   SHA). Non-zero exit = "no PR for this branch" → surface a CTA to create one
   (`gh pr create` is out of scope for this phase; we link to the
   `pull/new/<branch>` URL the SCM panel already builds).
2. **Reconcile drift.** Each comment was written against `anchor.baselineRev`. We
   re-resolve every comment's anchor against the **PR head SHA's** version of the
   file (`git show <headRefOid>:<path>`), using the content fingerprint. Three
   outcomes per comment:
   - **Clean** — fingerprint found, we have a `(line, side)` valid in the head
     diff. Include it.
   - **Drifted** — found at a different line than the local hint. Re-pin
     silently to the head-diff line; include it.
   - **Lost** — fingerprint not present in the head version at all (the line was
     changed/removed by a commit we don't have locally). **Do not publish it.**
     Report it back so the UI can flag "N comments couldn't be placed on the
     current PR head" and leave them as local drafts.
3. Post the survivors as **one atomic review**:
   `gh api --method POST /repos/{owner}/{repo}/pulls/{n}/reviews --input -` with
   `{ commit_id: headRefOid, event: 'COMMENT', body, comments: [{path, line,
side, body, [start_line, start_side]}] }`. One review, not N inline comments —
   so it shows up as a single review event and fires one notification.
4. On success, **delete the published comments locally** (the user asked for
   this: local drafts disappear once they're on GitHub). Lost comments stay.

`commit_id` is always the freshly-resolved `headRefOid`, never the local HEAD —
this is the single most important defence against drift. We resolve it
immediately before posting so a push that lands between step 1 and step 3 is the
only (vanishingly small) race, and GitHub's own 422 validation catches it.

### 4. "Reviewed" file marks are content-pinned and auto-clear on drift

A reviewed-file mark is stored alongside comments, as a per-folder
`reviewed_files: Vec<ReviewedFile>` on `FolderSession`:

```
ReviewedFile {
  path: string
  reviewedRev: string   // the content fingerprint the tick was made against
  reviewedAt: string
}
```

`reviewedRev` is the **blob SHA of the version the user ticked** — the working-tree
blob (`git hash-object <path>`, cheap and exact) when the file has uncommitted
changes, or the committed blob SHA otherwise. We reuse git's own object identity
rather than hashing file bytes ourselves.

On every git-status refresh, each `ReviewedFile` is re-validated: if the file's
_current_ version still hashes to `reviewedRev`, the row renders ticked;
otherwise the mark **auto-clears** (the row goes back to unreviewed) and the
stale `ReviewedFile` is dropped. So a new commit — local or pulled — that touches
a previously-reviewed file un-ticks exactly that file and leaves the rest of the
review untouched. This is the file-granularity twin of the line-granularity
comment re-anchoring in decision 2: same principle, coarser fingerprint.

The mark is purely a local progress aid. It is **not** pushed to GitHub (the
review-comments publish in decision 3 is the only thing that ever leaves the
machine), and it's independent of comments — you can tick a file with no comments
and comment on a file you haven't ticked.

### 5. The review-changes baseline and the PR head can disagree — and that's fine

The local review tab diffs against the **merge-base** (`'default'` mode). The PR
diff on GitHub is also merge-base-based, but against a base ref that may have
moved. We do **not** try to make the local diff identical to GitHub's diff — that
way lies reimplementing GitHub's diff engine. We only guarantee that **each
published comment's anchored line text exists at the PR head**, which is exactly
what GitHub validates. If the surrounding diff context differs, the comment still
lands on the right line because we anchor by content, not by hunk position
(`line`+`side`, never the legacy `position` offset).

## Consequences

- A new persisted schema field (`FolderSession.review_comments`). Per AGENTS.md's
  "no premature migrations", it's `#[serde(default)]` and absent-on-old-sessions
  is fine.
- A new `WorkspaceHost` method, so `ContainerHost` (Phase 2) must implement it
  when it lands — it's a `gh` shell-out, so the container impl is the same call
  inside the shell.
- The fingerprint re-anchoring runs on every `ReviewSection` rebuild. It's a hash
  compare over a handful of lines per comment; negligible next to the MergeView
  diff itself.
- We own a small amount of "where did this line go" logic. It is deliberately
  dumb (exact trimmed-text match within a window, then give up to "stale"); we do
  not attempt fuzzy/semantic matching. If the team wants smarter relocation
  later, the fingerprint field is the seam to upgrade.

## Considered and rejected: `git notes`

`git notes` attaches arbitrary metadata to a git object (commit/blob/tree) in a
parallel ref (`refs/notes/<namespace>`) without changing the object's SHA. It's
the obvious "native git way to attach stuff to code", so we evaluated it
explicitly. It's the right tool for durable, shareable, commit-pinned metadata
(CI results, provenance annotations meant to live with the repo forever). It is
the wrong tool for review-comment drafts, for four concrete reasons:

1. **Granularity is the object, not the line.** A note attaches to a whole
   commit or blob — there is no native line anchor. We would _still_ encode
   `(path, line, fingerprint)` inside the note body ourselves, so notes buy us a
   storage backend, not anchoring. The hard part of this design (re-anchoring
   across drift) is unchanged either way.
2. **Our comments exist before any commit.** The point is to review the working
   tree against a merge-base, often pre-commit. There is no commit object to
   attach a working-tree comment to; we'd attach to HEAD as a proxy and keep the
   real anchor in the body, at which point the notes ref is just a worse JSON
   store.
3. **Notes are repo-global and pushable.** They live in the shared `.git`, can be
   pushed/fetched, and `refs/notes/*` merges are a known sharp edge. Our comments
   are deliberately **private, machine-local, per-folder drafts that are deleted
   on publish** — exactly what we _don't_ want leaking into a shared, conflict-prone
   ref.
4. **Lifecycle and schema freedom.** Deleting a session-JSON entry on publish is
   trivial; rewriting (and deciding whether to push) a notes ref is not. And a
   notes encoding would be a quasi-public on-disk format in everyone's `.git`,
   the opposite of the "no premature migrations / schema is ours to restructure"
   freedom AGENTS.md grants the session file until the final phase.

So drafts live in the per-folder session file (decision 1 above), not in git
notes.

## Non-goals (this phase)

- **No threading / replies / resolve.** A comment is a one-shot draft. Replying
  to existing GitHub review threads, resolving conversations, or showing other
  people's comments is not in scope. We are a _write-my-own-review_ tool, not a
  GitHub review-thread client.
- **No GitLab / Bitbucket / Gitea.** GitHub-only, matching `remote_web_url`. The
  publish path is the only host-specific seam; add mapping when someone on
  another host asks.
- **No `gh pr create`.** If there's no PR, we link to the create-PR URL and stop.
- **No APPROVE / REQUEST_CHANGES from the IDE initially.** The review is always
  posted as `event: COMMENT`. Approving/requesting-changes is a heavier social
  action we can add once the comment flow proves out — the `event` field is the
  seam.
- **No round-tripping published comments back into the IDE.** Once posted, a
  comment lives on GitHub; we don't poll or re-display it. The local copy is
  deleted.
