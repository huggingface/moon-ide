//! Review state — inline review comments and reviewed-file marks
//! layered on the Review changes tab. See
//! [specs/review-comments.md](../../specs/review-comments.md) and
//! [ADR 0027](../../specs/decisions/0027-review-comments.md).
//!
//! Both are **local-first, per-folder** drafts persisted in the
//! workspace session ([`crate::session::FolderSession`]). Comments
//! are anchored by content (a line fingerprint) so they survive
//! edits and rebases, then published to a GitHub PR as one review
//! and cleared locally. Reviewed-file marks are pinned to the
//! ticked version's blob SHA so a new commit touching a ticked file
//! auto-un-ticks just that file.
//!
//! This phase (5.7.0) carries only the persisted shapes + the
//! publish request/result. The publish path itself (`gh` shell-out)
//! lands in 5.7.2.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Which side of the diff a comment is anchored to — mirrors
/// GitHub's `LEFT` / `RIGHT`. `Working` (GitHub `RIGHT`) is the
/// added / unchanged-context side, i.e. the code as it will land;
/// `Base` (GitHub `LEFT`) is the deleted / old side. Comments
/// default to `Working`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum ReviewSide {
	/// The merge-base / `HEAD` version — GitHub `LEFT`. Used when
	/// commenting on a deleted or pre-change line.
	Base,
	/// The working-tree version — GitHub `RIGHT`. The common case.
	Working,
}

/// Where a [`ReviewComment`] points. The line numbers are a
/// fast-path *hint* for rendering; `fingerprint` is the truth used
/// to re-locate the anchor after the text shifts (see the
/// content-based anchoring section of the spec). When the
/// fingerprint can't be found near the hint the comment goes
/// "stale" in the UI rather than being dropped.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAnchor {
	/// Workspace-relative path, matching [`crate::git::GitStatusEntry::path`].
	pub path: String,
	/// Diff side the comment lives on.
	pub side: ReviewSide,
	/// 1-based first line of the anchored range in the side's
	/// *current* text. A hint — re-derived from `fingerprint` on
	/// every section rebuild.
	pub start_line: u32,
	/// 1-based last line of the anchored range. Equal to
	/// `start_line` for a single-line comment.
	pub end_line: u32,
	/// Hash of the trimmed text of the anchored line(s). The
	/// source of truth for re-locating the anchor when line
	/// numbers drift.
	pub fingerprint: String,
	/// The merge-base / `HEAD` SHA the comment was written
	/// against. Recorded so the publish path can tell how far the
	/// world has moved since.
	pub baseline_rev: String,
}

/// One local-first review comment draft. One author (the user), one
/// body, one anchor — no threading or replies (deliberate non-goal,
/// see the spec). Persisted in [`crate::session::FolderSession::review_comments`]
/// until published to GitHub, then deleted locally.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
	/// Stable id (ULID) assigned on create. Used as the React-style
	/// keyed-list identity in the UI and as the handle the publish
	/// result reports back for "lost" comments.
	pub id: String,
	pub anchor: ReviewAnchor,
	/// Markdown comment text.
	pub body: String,
	/// RFC3339 creation timestamp.
	pub created_at: String,
}

/// A per-file "Viewed" mark (GitHub's checkbox). Pinned to the blob
/// SHA of the version that was ticked; on every git-status refresh
/// the frontend re-validates against the file's current blob SHA
/// and auto-clears the mark when they differ, so a new commit
/// touching the file un-ticks exactly that file. Never published —
/// a purely local progress aid for reviewing a large diff across
/// several sittings. Persisted in
/// [`crate::session::FolderSession::reviewed_files`].
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ReviewedFile {
	/// Workspace-relative path of the reviewed file.
	pub path: String,
	/// Blob SHA (`git hash-object`) of the version that was
	/// ticked. The mark stays valid only while the file's current
	/// blob SHA matches this.
	pub reviewed_rev: String,
	/// RFC3339 timestamp of when the file was marked reviewed.
	pub reviewed_at: String,
}

/// Request to publish a batch of local comments as a single GitHub
/// PR review. The `gh` shell-out (5.7.2) resolves the PR head SHA,
/// reconciles each comment's anchor against it, and posts the
/// survivors as one review carrying `event`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PublishReviewRequest {
	/// Optional review-summary body (the top-level review comment).
	pub body: Option<String>,
	/// The local comment drafts to publish. The backend decides
	/// which actually post based on drift reconciliation. May be
	/// empty: an approval needs no comment.
	pub comments: Vec<ReviewComment>,
	/// The review's verdict.
	#[serde(default)]
	pub event: ReviewEvent,
	/// Pull request to post to. `None` posts to the one open for the
	/// folder's current branch; the PR tab, which has no checkout of
	/// the branch, names it.
	#[serde(default)]
	pub pr_number: Option<u32>,
}

/// Verdict a published review carries (ADR 0113).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
	#[default]
	Comment,
	Approve,
	RequestChanges,
}

/// Outcome of [`PublishReviewRequest`]. A tagged enum so the UI can
/// branch cleanly between "no PR to post to" and a successful post.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum PublishReviewResult {
	/// No open PR for the current branch — the UI shows a
	/// create-PR CTA. `branch` is echoed back for the message.
	NoPr { branch: String },
	/// The review posted. `posted` is how many comments landed
	/// (clean + drifted), `lost` is the ids of comments whose
	/// anchored line wasn't present at the PR head (kept as local
	/// drafts), and `review_url` links to the posted review.
	Published {
		posted: u32,
		lost: Vec<String>,
		review_url: String,
	},
}

/// The pull request open for a folder's current branch, as the review
/// tab shows it (ADR 0113). GitHub only.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDetail {
	pub number: u32,
	pub title: String,
	pub url: String,
	/// Markdown description, without the bot's minor-notes block
	/// (that is [`minor_notes`](Self::minor_notes)).
	pub body: String,
	pub state: crate::inbox::InboxItemState,
	pub author: Option<String>,
	pub review: Option<crate::inbox::InboxReview>,
	pub base_ref: String,
	pub head_ref: String,
	pub head_sha: String,
	/// The head branch lives in a fork, so `origin` has no branch of
	/// that name to check out.
	pub is_cross_repository: bool,
	pub checks: Vec<PrCheck>,
	/// Nit-level notes moonbot keeps in the description instead of
	/// posting as review comments.
	pub minor_notes: Vec<PrMinorNote>,
	/// Short SHA the notes were written against; their line numbers
	/// refer to that commit.
	pub minor_notes_sha: Option<String>,
	/// Review threads, oldest first. Empty when they could not be
	/// fetched.
	pub threads: Vec<PrThread>,
}

/// A pull request opened in its own tab, without a checkout: the
/// detail plus the diff GitHub shows under "Files changed". Both
/// ends of that diff are in the folder's object store once this is
/// returned, so the file contents read like any other revision.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PrOpened {
	pub detail: PullRequestDetail,
	/// Full SHA of the commit the head branched from the base at: the
	/// "before" side of every file.
	pub merge_base: String,
	/// Files changed from `merge_base` to the PR head.
	pub files: Vec<crate::git::GitStatusEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PrCheck {
	pub name: String,
	pub state: PrCheckState,
	pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum PrCheckState {
	Success,
	Failure,
	Pending,
	/// Skipped, cancelled or neutral: finished without a verdict.
	Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PrMinorNote {
	pub path: String,
	/// 1-based line in the head version of `path`; `None` for a note
	/// about the file as a whole.
	pub line: Option<u32>,
	pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PrThread {
	pub path: String,
	/// 1-based line the thread sits on at the PR head; `None` once the
	/// code under it changed (the thread is then `outdated`).
	pub line: Option<u32>,
	pub side: ReviewSide,
	pub resolved: bool,
	pub outdated: bool,
	pub comments: Vec<PrThreadComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PrThreadComment {
	pub author: Option<String>,
	/// Markdown.
	pub body: String,
	/// RFC 3339.
	pub created_at: String,
	pub url: String,
}
