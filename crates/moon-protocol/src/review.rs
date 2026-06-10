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
/// survivors as one `event: COMMENT` review.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct PublishReviewRequest {
	/// Optional review-summary body (the top-level review comment).
	pub body: Option<String>,
	/// The local comment drafts to publish. The backend decides
	/// which actually post based on drift reconciliation.
	pub comments: Vec<ReviewComment>,
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
