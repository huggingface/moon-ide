//! Git status surfacing for the file tree. See `specs/roadmap.md`
//! Phase 5 for the end-state plan; this module carries only what the
//! current slice ships:
//!
//! - A lowercase enum matching Pierre Trees' `GitStatus` vocabulary
//!   (`'added' | 'modified' | 'deleted' | 'untracked' | 'ignored'`)
//!   so the TS mirror maps directly without a conversion table.
//! - A `{ path, status }` pair; no staged-vs-worktree split, no
//!   conflict marker, no per-hunk counts. The tree needs one label
//!   per row; more granular views go in the SCM panel later.
//!
//! Renames aren't in the enum on purpose: we ask git with
//! `--no-renames` so a rename lands as `deleted(old)` +
//! `added(new)`. Matches the roadmap's rendering contract ("we don't
//! try to be cleverer than git here") and keeps the frontend model
//! flat.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum GitFileStatus {
	/// New file that's been `git add`'d but not yet committed. Not to
	/// be confused with `Untracked` — an untracked file becomes
	/// `Added` the moment it enters the index.
	Added,
	/// Tracked file with worktree or staged content changes relative
	/// to `HEAD`.
	Modified,
	/// Tracked file that no longer exists on disk, or has been
	/// `git rm`'d into the index. The tree keeps deleted rows visible
	/// (see the roadmap note) even though they have no filesystem
	/// entry, so the backend reports the path even if the frontend
	/// never enumerated it.
	Deleted,
	/// On disk, no rule ignores it, not yet in the index. VSCode and
	/// friends render these at a different tint from `Added`; we
	/// mirror that so "I forgot to `git add`" is a glance away.
	Untracked,
	/// Covered by a `.gitignore` / `.git/info/exclude` rule and not
	/// in the index. Entire directories may be reported collapsed
	/// (`target/`); the tree fades those rows without expanding.
	Ignored,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusEntry {
	/// Workspace-relative path, directories terminated with `/` to
	/// match the `read_dir` and tree conventions. Deleted entries
	/// never carry a trailing slash — git can only delete files, not
	/// whole directories, in this representation.
	pub path: String,
	pub status: GitFileStatus,
}

/// Aggregate change counts for a single bound folder. Drives the
/// per-folder badges on the project bar so a user can see at a
/// glance which folders an agent (or anything else) just modified
/// — even folders that aren't currently active. `Untracked` files
/// fold into `added` because the project bar's signal-to-noise is
/// "this folder has a new file in it"; the SCM panel inside the
/// active folder still distinguishes the two.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitChangeSummary {
	pub added: u32,
	pub modified: u32,
	pub deleted: u32,
}

/// Per-line blame: who last touched this line, when, and with what
/// commit. The inline current-line annotation uses `author_short` +
/// a frontend-computed relative date + `summary`; the hover tooltip
/// consumes the full set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitLineBlame {
	/// 40-char commit hash, or 40 zero characters for a line that's
	/// been edited locally but not yet committed. Frontend checks
	/// for `is_uncommitted` rather than comparing the string.
	pub sha: String,
	/// True iff `sha` is the all-zero "Not Committed Yet" sentinel
	/// that `git blame` emits for local edits. Peeled out for the UI
	/// so a "You, uncommitted" badge doesn't have to know the
	/// convention.
	pub is_uncommitted: bool,
	/// Full author name as recorded on the commit.
	pub author: String,
	/// Author e-mail without the angle brackets `git blame` puts
	/// around it. Frontend rarely shows this beyond the hover
	/// tooltip; useful for gravatars / SCM-tool links later.
	pub author_email: String,
	/// Unix timestamp (UTC seconds) of the commit's author-time
	/// (not committer-time — blame tools universally prefer the
	/// original authorship moment over a later rebase's stamp).
	pub author_time: i64,
	/// First line of the commit message. Subjects run the gamut from
	/// 10 to 200 chars; the widget renderer ellipsizes locally.
	pub summary: String,
	/// Full commit message (subject + body, unwrapped). Rendered
	/// verbatim in the hover tooltip with `white-space: pre-wrap`,
	/// so line breaks survive. Markdown is intentionally *not*
	/// interpreted — commit messages aren't meant to be rich text
	/// and rendering them as Markdown would be surprising when e.g.
	/// list-style bullets get chewed up.
	pub message: String,
}

/// Lightweight info the SCM panel renders next to its commit
/// input: the active branch name, a short HEAD hash for the
/// detached state, and ahead/behind counts vs the upstream.
/// `None` (for the strings) and `0` (for the counts) are the
/// "this signal isn't available right now" fallbacks; the panel
/// renders the empty case as an inert label rather than a hard
/// error.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchInfo {
	/// Branch name, e.g. `"main"`. `None` when HEAD is detached,
	/// the folder isn't a git repo, or `git` itself isn't on PATH —
	/// the SCM panel falls back to `head_short_sha` for the
	/// detached case and to a plain "no branch" label otherwise.
	pub name: Option<String>,
	/// First seven characters of the HEAD commit's SHA. `None` when
	/// the repo has no commits yet (a fresh `git init`), HEAD is
	/// unreadable, or the folder isn't a git repo.
	pub head_short_sha: Option<String>,
	/// Whether the current branch has a configured upstream
	/// (`branch.<name>.remote` + `branch.<name>.merge`). `false`
	/// when the branch was just created locally and never pushed,
	/// when HEAD is detached, when the folder isn't a git repo,
	/// and when the branch's upstream is configured but currently
	/// unreachable. Distinguishes "in sync with upstream" (push +
	/// pull are no-ops, `ahead == behind == 0`, `has_upstream ==
	/// true`) from "no upstream yet" (the SCM panel renders a
	/// "Publish branch" affordance instead of the sync button).
	pub has_upstream: bool,
	/// Number of commits the local branch has that its configured
	/// upstream doesn't — commits that would be sent on the next
	/// `git push`. `0` when there's no upstream configured, no
	/// HEAD, or the count couldn't be determined.
	pub ahead: u32,
	/// Number of commits the upstream has that the local branch
	/// doesn't — commits that would be merged in on the next
	/// `git pull`. Same `0`-fallback semantics as `ahead`.
	pub behind: u32,
	/// Pre-built URL for opening a pull request against the
	/// repo's primary remote. `Some` only when the remote is a
	/// recognised host (currently `github.com`), HEAD is on a
	/// named branch (not detached), and the branch name has
	/// successfully URL-escaped. The SCM panel still gates the
	/// "Open PR" button on UI policy (non-main / non-master,
	/// `has_upstream`); the backend just produces the URL when
	/// it has the inputs.
	pub pr_url: Option<String>,
	/// Remote-tracking ref for the repo's default branch — e.g.
	/// `"origin/main"`. Resolved from `refs/remotes/origin/HEAD`
	/// when present, else falls back to `origin/main` →
	/// `origin/master`. `None` when no `origin` remote exists,
	/// the symbolic ref isn't set, and neither fallback is
	/// available; the SCM panel hides its "Update from <main>"
	/// affordance in that case.
	pub default_branch_remote_ref: Option<String>,
	/// Number of commits the default branch's remote-tracking
	/// ref has that the current branch's HEAD doesn't — what a
	/// `git merge <default_branch_remote_ref>` would land. `0`
	/// when no default can be resolved, when HEAD is already up
	/// to date, when we're already on the default branch (the
	/// regular `Sync Changes` button covers that case), or when
	/// the count couldn't be determined. The SCM panel shows the
	/// "Update from main" button iff this is `> 0`.
	pub default_branch_behind: u32,
}

/// One row in the branch-switcher palette. Two kinds today: a
/// local branch (or remote-tracking ref already fetched) and a
/// GitHub PR sourced from `gh pr list`. The discriminant drives
/// the switch verb on the backend — `git switch <name>` for
/// `Local`, `gh pr checkout <number>` for `Pr` (so cross-fork PRs
/// get the fork-fetching dance for free).
///
/// Frontend renders both in a single list with a section header
/// per kind; type-to-filter spans both. See
/// `src/lib/components/BranchSwitcher.svelte`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum BranchListEntry {
	/// A branch the local repo already knows about — cheap
	/// `git switch` target. Sourced from `git for-each-ref
	/// refs/heads`, sorted by committer date (newest first) at
	/// the backend so the UI renders the order verbatim.
	Local {
		/// Short branch name (`feat/foo`, `main`).
		name: String,
		/// First line of the tip commit (`%(subject)`). Empty
		/// string when the ref points at an empty tree (rare —
		/// freshly created branch with `--orphan` and no
		/// commit yet).
		last_commit_subject: String,
		/// Human-readable "3 hours ago" / "yesterday" style
		/// timestamp, computed by git itself
		/// (`%(committerdate:relative)`). The frontend renders
		/// it verbatim — no locale translation, matches what
		/// `git branch -v` would print.
		committer_date_relative: String,
		/// Marker for the row that's currently checked out so
		/// the UI can render it as inert (no point switching to
		/// the branch you're already on).
		is_current: bool,
	},
	/// A GitHub pull request, as reported by `gh pr list`.
	Pr {
		/// PR number (the `#42` segment). 32-bit fits every
		/// realistic GitHub repo's PR count.
		number: u32,
		/// PR title verbatim. Rendered with mono accent on the
		/// number, then a separator, then the title.
		title: String,
		/// GitHub login of the PR's author (no `@` prefix). The
		/// frontend prepends `@` itself so the wire format
		/// stays clean.
		author: String,
		/// Source branch (`headRefName`) the PR is open from.
		/// Used by `gh pr checkout` implicitly; we surface it
		/// in the UI for users who recognise the branch name
		/// faster than the title.
		head_ref: String,
		/// True iff the PR is currently a draft. The frontend
		/// renders a small `draft` badge inline; type-to-filter
		/// still matches drafts (no filter knob today, hardcode
		/// first per AGENTS.md).
		is_draft: bool,
		/// Human-readable last-update timestamp from gh's
		/// JSON. We compute the relative form on the backend so
		/// every row has the same format regardless of locale.
		updated_at_relative: String,
	},
}

/// Why the PR section of [`BranchList`] is empty. Surfaced in the
/// palette as the section's empty-state row so the user knows
/// whether to install gh, run `gh auth login`, or accept that
/// their remote isn't on GitHub.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum PrListStatus {
	/// PRs were fetched successfully (the section is populated
	/// or genuinely has no open PRs).
	#[default]
	Ok,
	/// `gh` isn't installed (or isn't on the resolved `PATH`).
	/// Frontend renders `"Install gh to see PR list"` plus a
	/// link to `https://cli.github.com/`.
	GhMissing,
	/// `gh` is installed but `gh auth status` reports no usable
	/// auth (signed out, expired token). Frontend offers a
	/// "Run `gh auth login`" hint that opens an integrated
	/// terminal pinned to the active folder.
	GhNotAuthed,
	/// The active folder's `origin` (or `upstream`) isn't a
	/// GitHub remote, so PRs aren't applicable. Frontend
	/// suppresses the section entirely (no "empty" row, no
	/// "missing" row — just no PR section).
	NotGithub,
	/// `gh pr list` ran but exited non-zero (network error, API
	/// rate limit, scope refused, …). Frontend surfaces the
	/// detail verbatim so the user gets the actionable hint.
	Failed { detail: String },
}

/// Result of `branch_list`. Three slots so the frontend can paint
/// local rows immediately even if the gh probe is slow / failing
/// — local always returns from `git for-each-ref` in single-digit
/// milliseconds, gh can stall on a network round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct BranchList {
	/// Recent local branches, newest committer-date first,
	/// capped at a small number (today's slice ships 20 — bumps
	/// when a real project hits the cap).
	pub local: Vec<BranchListEntry>,
	/// Open GitHub PRs against the active folder's repo. Empty
	/// when [`pr_status`](Self::pr_status) is anything other
	/// than `Ok`; capped at 30. Sub-filters (`@me`, "review
	/// requested") are deferred — type-to-filter handles the
	/// volume the team currently sees.
	pub prs: Vec<BranchListEntry>,
	/// Why `prs` is empty, if it is. `Ok` means "the section is
	/// populated with whatever gh returned, including the empty
	/// case of no open PRs" — the frontend distinguishes
	/// "section unavailable" from "section truthfully empty".
	pub pr_status: PrListStatus,
}

/// Scope filter for `branch_list`'s PR section. `All` mirrors
/// `gh pr list --state open` (every open PR in the repo);
/// `Participating` uses GitHub's search qualifiers to keep only
/// PRs the user is involved in — a focused list for repos with
/// dozens of in-flight changes.
///
/// "Participating" runs two `gh pr list --search` queries in
/// parallel and merges them by PR number:
///
/// - `state:open involves:@me` — author, assignee, mentioned, or
///   commenter (everything GitHub's notification "Participating"
///   filter covers).
/// - `state:open review-requested:@me` — review explicitly
///   requested from the user. Not covered by `involves:`.
///
/// The default (`All`) matches the previous slice's behaviour so
/// flipping the toggle in the palette is the gesture, not the
/// other way around.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum PrListScope {
	/// Every open PR in the active folder's repo.
	#[default]
	All,
	/// PRs the user is involved in — author / assignee /
	/// mentioned / commenter / review requested.
	Participating,
}

/// Argument for `branch_switch`. `Local` runs `git switch
/// <name>`; `Pr` runs `gh pr checkout <number>` so cross-fork
/// PRs work without manual remote / fetch fiddling.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum BranchSwitchTarget {
	/// Switch to a local branch by name. Errors propagate git's
	/// stderr verbatim ("Your local changes to the following
	/// files would be overwritten by checkout") so the user gets
	/// the actionable hint.
	Local { name: String },
	/// Check out a GitHub PR by number via `gh pr checkout`.
	/// gh's stderr propagates the same way — auth missing,
	/// network failure, dirty tree, etc.
	Pr { number: u32 },
}

/// Outcome of a successful `git_commit`. The SCM panel renders
/// `short_sha` + `summary` in the post-commit toast so the user
/// can verify the commit landed without opening a terminal.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitResult {
	/// First seven characters of the new commit's SHA.
	pub short_sha: String,
	/// First line of the commit message we just wrote, echoed back
	/// for display. Same string the user typed (modulo trailing
	/// whitespace), so callers don't have to round-trip it.
	pub summary: String,
}

/// Per-file blame report, one entry per source line. Indexing is
/// 0-based so it lines up directly with CM's `doc.line(n + 1)`
/// accessor; empty trailing lines (the "no-newline-at-EOF" corner
/// case) are not represented — `git blame` skips them.
///
/// `None` is returned to callers when blame is genuinely unavailable
/// for this file (outside a repo, path never tracked, or `git`
/// itself isn't on PATH). The UI treats "no blame" as "no widget",
/// which is the right outcome for all three.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitFileBlame {
	/// Workspace-relative path the blame was computed against.
	/// Echoed back so a late-arriving response can be ignored when
	/// the active buffer has moved on.
	pub path: String,
	/// Canonical HTTPS base URL of the repo's primary remote, when
	/// it's a host we know how to build PR / issue links for
	/// (currently `github.com` only). Trailing slash omitted — the
	/// frontend appends `/pull/<N>` or similar. Empty string when
	/// the remote isn't set, isn't a recognised host, or points at a
	/// protocol we don't normalise (e.g. `file://`, raw SSH to an
	/// arbitrary server).
	///
	/// Scoped per-file rather than per-workspace so a
	/// multi-folder workspace where each folder has a different
	/// remote keeps the link target correct without the frontend
	/// having to cross-reference folder bindings.
	pub remote_url: String,
	pub lines: Vec<GitLineBlame>,
}
