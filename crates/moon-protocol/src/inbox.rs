//! GitHub inbox: the cross-repo list of pull requests and notification
//! threads that want the user's attention (ADR 0112).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::workspace::Workspace;

/// One poll of GitHub. Every source is fetched independently, so a
/// snapshot can be partial: [`failures`](Self::failures) names the
/// sources whose rows are missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxSnapshot {
	pub status: InboxStatus,
	/// Login of the `gh` account. Empty when `status` isn't `ok`.
	pub viewer: String,
	pub items: Vec<InboxItem>,
	/// Bound project folders whose remote is a GitHub repository. An
	/// item whose `repo` appears here can be opened in a worktree.
	pub repos: Vec<InboxRepo>,
	pub failures: Vec<InboxFailure>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxStatus {
	#[default]
	Ok,
	GhMissing,
	GhNotAuthed,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxRepo {
	/// `owner/name`.
	pub repo: String,
	pub folder_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxFailure {
	pub source: InboxSource,
	pub detail: String,
}

/// Why an item is in the inbox. An item found by several sources
/// carries all of them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq, Hash)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum InboxSource {
	/// Open PRs where the user, or a team they are on, is a requested
	/// reviewer.
	ReviewRequested,
	/// Open PRs by one of the watched authors, in the bound repos.
	Watched,
	/// Open PRs that mention the user.
	Mentioned,
	/// The user's own open PRs.
	Mine,
	/// Unread notification threads.
	Notification,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum InboxItemKind {
	PullRequest,
	Issue,
	/// A notification subject that is neither (release, discussion,
	/// check suite, …). Only `title`, `repo` and `url` are meaningful.
	Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum InboxItemState {
	Open,
	Draft,
	Merged,
	Closed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum InboxReview {
	Approved,
	ChangesRequested,
	ReviewRequired,
}

/// Rollup of the head commit's checks, as GitHub reports it. A
/// cancelled check counts as a failure there, so it does here too.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum InboxCi {
	Success,
	Failure,
	Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxNotification {
	/// Notification thread id, the handle for `inbox_mark_read`.
	pub thread_id: String,
	/// GitHub's reason string (`review_requested`, `mention`,
	/// `author`, `ci_activity`, …), verbatim.
	pub reason: String,
	/// RFC 3339.
	pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
	/// `owner/name#123` for a PR or issue, `thread:<id>` otherwise.
	pub id: String,
	pub kind: InboxItemKind,
	/// `owner/name`.
	pub repo: String,
	pub number: Option<u32>,
	pub title: String,
	/// Web URL.
	pub url: String,
	/// `None` when it isn't known: an [`InboxItemKind::Other`] subject,
	/// or an issue (issues aren't hydrated).
	pub state: Option<InboxItemState>,
	pub author: Option<String>,
	pub author_avatar_url: Option<String>,
	/// RFC 3339. The notification's time when the subject wasn't
	/// hydrated.
	pub updated_at: String,
	pub head_ref: Option<String>,
	pub base_ref: Option<String>,
	/// The head branch lives in a fork, so it can't be checked out by
	/// name from `origin`.
	pub is_cross_repository: bool,
	pub additions: Option<u32>,
	pub deletions: Option<u32>,
	pub comment_count: Option<u32>,
	pub review: Option<InboxReview>,
	pub ci: Option<InboxCi>,
	pub sources: Vec<InboxSource>,
	pub notification: Option<InboxNotification>,
}

/// Result of `inbox_open_pr`: the folder now holding the PR's branch.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxOpenedPr {
	pub workspace: Workspace,
	/// The worktree bound for the branch, or the project folder itself
	/// when it already has the branch checked out.
	pub folder_path: String,
}

/// An issue as its tab shows it (ADR 0117).
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct IssueDetail {
	pub number: u32,
	pub title: String,
	pub url: String,
	/// Markdown.
	pub body: String,
	pub open: bool,
	pub author: Option<String>,
	/// RFC 3339.
	pub created_at: String,
	pub labels: Vec<IssueLabel>,
	pub assignees: Vec<String>,
	/// Oldest first.
	pub comments: Vec<ConversationComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct IssueLabel {
	pub name: String,
	/// Six hex digits, no `#`.
	pub color: String,
}

/// One entry of an issue's or pull request's conversation: a
/// top-level comment, or a review's summary.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct ConversationComment {
	pub author: Option<String>,
	/// Markdown. Empty for a review submitted without a summary.
	pub body: String,
	/// RFC 3339.
	pub created_at: String,
	pub url: String,
	/// Set when this is a review's summary rather than a comment.
	pub review: Option<ConversationReview>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ConversationReview {
	Approved,
	ChangesRequested,
	Commented,
	Dismissed,
}

/// A pull request the inbox asks more about.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct InboxPrRef {
	/// `owner/name`.
	pub repo: String,
	pub number: u32,
}

/// What a pull request's row shows beyond the listing, and what
/// grouping reads of it (ADR 0119). Fetched apart from the snapshot,
/// and only for rows that changed.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxPrDigest {
	/// [`InboxItem::id`].
	pub id: String,
	/// Moonbot's minor notes in the description.
	pub minor_notes: u32,
	pub open_threads: u32,
	pub resolved_threads: u32,
	/// The first changed paths.
	pub paths: Vec<String>,
	/// The start of the description, as plain text.
	pub summary: String,
}

/// A named set of pull requests to review together.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct InboxGroup {
	pub name: String,
	/// What its pull requests have in common, in a few words.
	pub reason: String,
}

/// Which [`InboxGroup`] (by name) an item belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct InboxGroupAssignment {
	/// [`InboxItem::id`].
	pub id: String,
	pub group: String,
}

/// One pull request as `inbox_group` describes it to the model.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct InboxGroupItem {
	pub id: String,
	pub repo: String,
	pub title: String,
	pub author: Option<String>,
	pub head_ref: Option<String>,
	pub additions: Option<u32>,
	pub deletions: Option<u32>,
	pub paths: Vec<String>,
	pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct InboxGroupRequest {
	/// Pull requests to place.
	pub items: Vec<InboxGroupItem>,
	/// Groups that already exist: reused by name when one fits.
	pub existing: Vec<InboxGroup>,
}

/// Answer of `inbox_group`. `groups` are the ones `assignments` name;
/// an item the model left out has no assignment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct InboxGrouping {
	pub groups: Vec<InboxGroup>,
	pub assignments: Vec<InboxGroupAssignment>,
}
