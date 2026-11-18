//! A GitHub pull request as the IDE shows it (ADR 0113): description,
//! checks, review threads, and the nit-level notes moonbot keeps in
//! the description. Read for the branch a checkout is on (the review
//! tab) or by number, with its diff and no checkout (the PR tab).

use camino::Utf8Path;
use moon_protocol::inbox::{ConversationComment, ConversationReview, InboxItemState, InboxReview};
use moon_protocol::review::{
	PrAction, PrCheck, PrCheckState, PrDiff, PrMergeMethod, PrMergeStatus, PrMinorNote, PrThread, PrThreadComment,
	PullRequestDetail, ReviewSide,
};
use moon_protocol::{MoonError, MoonResult};
use serde::Deserialize;
use serde_json::Value;

use crate::forge::{detect_forge_remote, ForgeKind};
use crate::host::WorkspaceHost;
use crate::inbox::{gh_graphql, run_gh_in};

/// Delimiters of the block moonbot rewrites on each review pass. The
/// last start marker wins, as in the bot's own parser.
const NOTES_START: &str = "<!-- moonbot:minor-notes:start -->";
const NOTES_END: &str = "<!-- moonbot:minor-notes:end -->";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
	number: u32,
	title: String,
	url: String,
	#[serde(default)]
	body: String,
	state: String,
	#[serde(default)]
	is_draft: bool,
	author: Option<RawAuthor>,
	review_decision: Option<String>,
	base_ref_name: String,
	head_ref_name: String,
	head_ref_oid: String,
	#[serde(default)]
	is_cross_repository: bool,
	#[serde(default)]
	status_check_rollup: Vec<RawCheck>,
	#[serde(default)]
	comments: Vec<RawComment>,
	#[serde(default)]
	reviews: Vec<RawReview>,
	#[serde(default)]
	mergeable: String,
	#[serde(default)]
	merge_state_status: String,
}

const PR_VIEW_FIELDS: &str = "number,title,url,body,state,isDraft,author,reviewDecision,baseRefName,headRefName,\
	headRefOid,isCrossRepository,statusCheckRollup,comments,reviews,mergeable,mergeStateStatus";

/// `mergeStateStatus` and `mergeable` as one verdict. A conflict wins:
/// GitHub can report it under either field.
fn merge_status(merge_state: &str, mergeable: &str) -> PrMergeStatus {
	if merge_state == "DIRTY" || mergeable == "CONFLICTING" {
		return PrMergeStatus::Conflicting;
	}
	match merge_state {
		"CLEAN" | "HAS_HOOKS" => PrMergeStatus::Clean,
		"UNSTABLE" => PrMergeStatus::Unstable,
		"BLOCKED" => PrMergeStatus::Blocked,
		"BEHIND" => PrMergeStatus::Behind,
		_ => PrMergeStatus::Unknown,
	}
}

const ALL_MERGE_METHODS: [PrMergeMethod; 3] = [PrMergeMethod::Squash, PrMergeMethod::Merge, PrMergeMethod::Rebase];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMergeSettings {
	squash_merge_allowed: bool,
	merge_commit_allowed: bool,
	rebase_merge_allowed: bool,
	#[serde(default)]
	viewer_default_merge_method: String,
}

/// The allowed methods, the viewer's default first.
fn methods_from_settings(raw: &RawMergeSettings) -> Vec<PrMergeMethod> {
	let default = match raw.viewer_default_merge_method.as_str() {
		"SQUASH" => Some(PrMergeMethod::Squash),
		"MERGE" => Some(PrMergeMethod::Merge),
		"REBASE" => Some(PrMergeMethod::Rebase),
		_ => None,
	};
	let mut methods: Vec<PrMergeMethod> = ALL_MERGE_METHODS
		.into_iter()
		.filter(|method| match method {
			PrMergeMethod::Squash => raw.squash_merge_allowed,
			PrMergeMethod::Merge => raw.merge_commit_allowed,
			PrMergeMethod::Rebase => raw.rebase_merge_allowed,
		})
		.collect();
	methods.sort_by_key(|method| Some(*method) != default);
	methods
}

/// Merge methods `owner_repo` allows. Every method when the settings
/// can't be read: GitHub still refuses one the repository doesn't
/// allow, and says so.
async fn merge_methods(owner_repo: &str) -> Vec<PrMergeMethod> {
	let Some((owner, name)) = owner_repo.split_once('/') else {
		return ALL_MERGE_METHODS.to_vec();
	};
	let query = "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { \
		squashMergeAllowed mergeCommitAllowed rebaseMergeAllowed viewerDefaultMergeMethod } }";
	let settings = match gh_graphql(query, &[("owner", owner), ("name", name)]).await {
		Ok(mut data) => data.pointer_mut("/repository").map(Value::take),
		Err(err) => {
			tracing::debug!(error = %err.detail(), "pull_request: merge settings fetch failed");
			None
		}
	};
	match settings.and_then(|value| serde_json::from_value::<RawMergeSettings>(value).ok()) {
		Some(raw) => methods_from_settings(&raw),
		None => ALL_MERGE_METHODS.to_vec(),
	}
}

/// `gh` arguments carrying out `action` on the pull request at `url`.
/// `None` for a merge whose head isn't a full commit SHA.
fn action_args(url: &str, action: &PrAction) -> Option<Vec<String>> {
	let args: Vec<&str> = match action {
		PrAction::Ready => vec!["pr", "ready", url],
		PrAction::Draft => vec!["pr", "ready", url, "--undo"],
		PrAction::Close => vec!["pr", "close", url],
		PrAction::Reopen => vec!["pr", "reopen", url],
		// No `--delete-branch`: that also deletes a local branch and
		// can move the checkout. The repository's own setting decides
		// what happens to the remote one.
		PrAction::Merge { method, head_sha } => {
			if head_sha.len() != 40 || !head_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
				return None;
			}
			let flag = match method {
				PrMergeMethod::Squash => "--squash",
				PrMergeMethod::Merge => "--merge",
				PrMergeMethod::Rebase => "--rebase",
			};
			vec!["pr", "merge", url, flag, "--match-head-commit", head_sha]
		}
	};
	Some(args.into_iter().map(str::to_owned).collect())
}

/// Carry out `action` on the pull request at `url` (its GitHub page).
/// The repository comes from the URL, so no checkout is consulted or
/// touched. Errors carry what `gh` said.
pub async fn act(url: &str, action: &PrAction) -> MoonResult<()> {
	if pr_from_url(url).is_none() {
		return Err(MoonError::invalid(format!("not a pull request URL: {url}")));
	}
	let Some(args) = action_args(url, action) else {
		return Err(MoonError::invalid("not a commit SHA".to_owned()));
	};
	let args: Vec<&str> = args.iter().map(String::as_str).collect();
	let output = run_gh_in(None, &args)
		.await
		.map_err(|err| MoonError::IoError(err.detail()))?;
	if !output.success {
		return Err(MoonError::IoError(output.stderr.trim().to_owned()));
	}
	Ok(())
}

/// A top-level comment, as `gh pr view` and `gh issue view` list them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawComment {
	author: Option<RawAuthor>,
	#[serde(default)]
	body: String,
	created_at: String,
	#[serde(default)]
	url: String,
	/// Hidden on GitHub (outdated, resolved, spam, ...).
	#[serde(default)]
	is_minimized: bool,
}

impl RawComment {
	/// `None` for a comment GitHub hides.
	pub(crate) fn into_comment(self) -> Option<ConversationComment> {
		if self.is_minimized {
			return None;
		}
		Some(ConversationComment {
			author: self.author.map(|author| author.login),
			body: self.body,
			created_at: self.created_at,
			url: self.url,
			review: None,
		})
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReview {
	author: Option<RawAuthor>,
	#[serde(default)]
	body: String,
	submitted_at: Option<String>,
	state: String,
}

/// The conversation of a pull request: its comments, plus each review
/// that says something. A `COMMENTED` review with no summary only
/// carries inline threads, which are shown on their lines; a pending
/// one isn't submitted yet.
fn conversation_from_raw(comments: Vec<RawComment>, reviews: Vec<RawReview>, pr_url: &str) -> Vec<ConversationComment> {
	let mut out: Vec<ConversationComment> = comments.into_iter().filter_map(RawComment::into_comment).collect();
	for review in reviews {
		let verdict = match review.state.as_str() {
			"APPROVED" => ConversationReview::Approved,
			"CHANGES_REQUESTED" => ConversationReview::ChangesRequested,
			"DISMISSED" => ConversationReview::Dismissed,
			"COMMENTED" if !review.body.trim().is_empty() => ConversationReview::Commented,
			_ => continue,
		};
		let Some(created_at) = review.submitted_at else {
			continue;
		};
		out.push(ConversationComment {
			author: review.author.map(|author| author.login),
			body: review.body,
			created_at,
			url: pr_url.to_owned(),
			review: Some(verdict),
		});
	}
	out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
	out
}

#[derive(Deserialize)]
pub(crate) struct RawAuthor {
	pub(crate) login: String,
}

/// One entry of `gh pr view --json statusCheckRollup`: a check run
/// (`name` / `conclusion` / `detailsUrl`) or a commit
/// status (`context` / `state` / `targetUrl`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheck {
	name: Option<String>,
	context: Option<String>,
	workflow_name: Option<String>,
	conclusion: Option<String>,
	state: Option<String>,
	details_url: Option<String>,
	target_url: Option<String>,
}

fn check_from_raw(raw: RawCheck) -> PrCheck {
	let state = match (raw.conclusion.as_deref(), raw.state.as_deref()) {
		(Some("SUCCESS"), _) | (_, Some("SUCCESS")) => PrCheckState::Success,
		(Some("FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED"), _) | (_, Some("FAILURE" | "ERROR")) => {
			PrCheckState::Failure
		}
		(Some("SKIPPED" | "CANCELLED" | "NEUTRAL" | "STALE"), _) => PrCheckState::Skipped,
		// A run still going has an empty conclusion.
		_ => PrCheckState::Pending,
	};
	let name = match (raw.workflow_name.filter(|w| !w.is_empty()), raw.name.or(raw.context)) {
		(Some(workflow), Some(name)) => format!("{workflow} / {name}"),
		(None, Some(name)) => name,
		(Some(workflow), None) => workflow,
		(None, None) => "check".to_owned(),
	};
	PrCheck {
		name,
		state,
		url: raw.details_url.or(raw.target_url).filter(|url| !url.is_empty()),
	}
}

/// Split a PR description into its prose and moonbot's minor notes.
/// Returns `(body without the block, notes, reviewed short SHA)`.
pub fn split_minor_notes(body: &str) -> (String, Vec<PrMinorNote>, Option<String>) {
	let Some(start) = body.rfind(NOTES_START) else {
		return (body.trim_end().to_owned(), Vec::new(), None);
	};
	let after_start = start + NOTES_START.len();
	let Some(end) = body[after_start..].find(NOTES_END).map(|offset| after_start + offset) else {
		return (body.trim_end().to_owned(), Vec::new(), None);
	};
	let block = &body[after_start..end];
	let sha = block
		.split_once("last reviewed `")
		.and_then(|(_, rest)| rest.split_once('`'))
		.map(|(sha, _)| sha.to_owned())
		.filter(|sha| !sha.is_empty());
	let notes = block.lines().filter_map(parse_note_line).collect();
	let mut prose = body[..start].trim_end().to_owned();
	let tail = body[end + NOTES_END.len()..].trim();
	if !tail.is_empty() {
		prose.push_str("\n\n");
		prose.push_str(tail);
	}
	(prose, notes, sha)
}

/// ``> - `path[:line]` — note`` → a note. Anything else is `None`.
fn parse_note_line(line: &str) -> Option<PrMinorNote> {
	let item = line.trim_start().trim_start_matches('>').trim_start();
	let rest = item.strip_prefix("- `")?;
	let (location, text) = rest.split_once('`')?;
	let text = text.trim_start().trim_start_matches(['—', '-', ':']).trim();
	let (path, line) = match location.rsplit_once(':') {
		Some((path, number)) if !path.is_empty() => match number.parse::<u32>() {
			Ok(number) => (path, Some(number)),
			Err(_) => (location, None),
		},
		_ => (location, None),
	};
	if path.is_empty() || text.is_empty() {
		return None;
	}
	Some(PrMinorNote {
		path: path.to_owned(),
		line,
		text: text.to_owned(),
	})
}

/// `https://github.com/<owner>/<name>/pull/<n>` → `(owner, name, n)`.
fn pr_from_url(url: &str) -> Option<(&str, &str, u32)> {
	let (owner, name) = repo_from_pr_url(url)?;
	let number = url.rsplit('/').next()?.parse().ok()?;
	Some((owner, name, number))
}

/// `https://github.com/<owner>/<name>/pull/<n>` → `(owner, name)`.
fn repo_from_pr_url(url: &str) -> Option<(&str, &str)> {
	let path = url.strip_prefix("https://")?.split_once('/')?.1;
	let mut parts = path.split('/');
	let owner = parts.next()?;
	let name = parts.next()?;
	(parts.next() == Some("pull") && !owner.is_empty() && !name.is_empty()).then_some((owner, name))
}

fn threads_from_data(mut data: Value) -> Vec<PrThread> {
	let nodes = match data
		.pointer_mut("/repository/pullRequest/reviewThreads/nodes")
		.map(Value::take)
	{
		Some(Value::Array(nodes)) => nodes,
		_ => return Vec::new(),
	};
	nodes
		.into_iter()
		.filter_map(|node| {
			let comments: Vec<PrThreadComment> = node
				.pointer("/comments/nodes")?
				.as_array()?
				.iter()
				.filter_map(|comment| {
					Some(PrThreadComment {
						author: comment
							.pointer("/author/login")
							.and_then(Value::as_str)
							.map(str::to_owned),
						body: comment.get("body")?.as_str()?.to_owned(),
						created_at: comment.get("createdAt")?.as_str()?.to_owned(),
						url: comment.get("url")?.as_str()?.to_owned(),
					})
				})
				.collect();
			if comments.is_empty() {
				return None;
			}
			Some(PrThread {
				path: node.get("path")?.as_str()?.to_owned(),
				line: node.get("line").and_then(Value::as_u64).map(|line| line as u32),
				side: match node.get("diffSide").and_then(Value::as_str) {
					Some("LEFT") => ReviewSide::Base,
					_ => ReviewSide::Working,
				},
				resolved: node.get("isResolved").and_then(Value::as_bool).unwrap_or(false),
				outdated: node.get("isOutdated").and_then(Value::as_bool).unwrap_or(false),
				comments,
			})
		})
		.collect()
}

async fn fetch_threads(owner: &str, name: &str, number: u32) -> Vec<PrThread> {
	let query = format!(
		"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ \
		 pullRequest(number: {number}) {{ reviewThreads(first: 100) {{ nodes {{ isResolved isOutdated path line \
		 diffSide comments(first: 50) {{ nodes {{ author {{ login }} body createdAt url }} }} }} }} }} }} }}"
	);
	match gh_graphql(&query, &[("owner", owner), ("name", name)]).await {
		Ok(data) => threads_from_data(data),
		Err(err) => {
			tracing::debug!(error = %err.detail(), "pull_request: review threads fetch failed");
			Vec::new()
		}
	}
}

/// `gh pr view` output as the detail the UI shows, review threads
/// aside: those are a second request.
fn detail_from_raw(raw: RawPr) -> PullRequestDetail {
	let (body, minor_notes, minor_notes_sha) = split_minor_notes(&raw.body);
	let conversation = conversation_from_raw(raw.comments, raw.reviews, &raw.url);
	PullRequestDetail {
		number: raw.number,
		title: raw.title,
		url: raw.url,
		body,
		state: match raw.state.as_str() {
			"MERGED" => InboxItemState::Merged,
			"CLOSED" => InboxItemState::Closed,
			_ if raw.is_draft => InboxItemState::Draft,
			_ => InboxItemState::Open,
		},
		author: raw.author.map(|author| author.login),
		review: match raw.review_decision.as_deref() {
			Some("APPROVED") => Some(InboxReview::Approved),
			Some("CHANGES_REQUESTED") => Some(InboxReview::ChangesRequested),
			Some("REVIEW_REQUIRED") => Some(InboxReview::ReviewRequired),
			_ => None,
		},
		base_ref: raw.base_ref_name,
		head_ref: raw.head_ref_name,
		head_sha: raw.head_ref_oid,
		is_cross_repository: raw.is_cross_repository,
		checks: raw.status_check_rollup.into_iter().map(check_from_raw).collect(),
		minor_notes,
		minor_notes_sha,
		threads: Vec::new(),
		conversation,
		merge_status: merge_status(&raw.merge_state_status, &raw.mergeable),
		merge_methods: ALL_MERGE_METHODS.to_vec(),
	}
}

/// Read a pull request of `root`'s GitHub repository, without its
/// review threads: `number`, or the one open for the current branch
/// when `None`. One `gh` call, so a tab can paint the description
/// while the threads and the diff are still on their way. Errors when
/// the remote isn't GitHub, `gh` can't answer, or there is no such PR.
pub async fn fetch_detail(root: &Utf8Path, number: Option<u32>) -> MoonResult<PullRequestDetail> {
	let owned = root.to_owned();
	let remote = tokio::task::spawn_blocking(move || detect_forge_remote(&owned))
		.await
		.map_err(|e| MoonError::Internal(format!("pull_request join error: {e}")))?;
	let Some(remote) = remote.filter(|remote| remote.kind == ForgeKind::GitHub) else {
		return Err(MoonError::invalid(format!("{root} is not a GitHub repository")));
	};
	let number_arg = number.map(|number| number.to_string());
	let mut args = vec!["pr", "view"];
	args.extend(number_arg.as_deref());
	args.extend(["--json", PR_VIEW_FIELDS]);
	// Side by side: the settings are a second request, and the tab's
	// first paint waits on this function.
	let (output, methods) = tokio::join!(run_gh_in(Some(root), &args), merge_methods(&remote.owner_repo));
	let output = output.map_err(|err| MoonError::IoError(err.detail()))?;
	if !output.success {
		return Err(MoonError::IoError(format!("gh pr view: {}", output.stderr)));
	}
	let raw: RawPr =
		serde_json::from_slice(&output.stdout).map_err(|e| MoonError::Internal(format!("gh pr view JSON: {e}")))?;
	Ok(PullRequestDetail {
		merge_methods: methods,
		..detail_from_raw(raw)
	})
}

/// The review threads of the pull request at `url` (its GitHub page,
/// as [`PullRequestDetail::url`] has it). Empty when they can't be
/// read: the tab is still useful without them.
pub async fn fetch_threads_at(url: &str) -> MoonResult<Vec<PrThread>> {
	let Some((owner, name, number)) = pr_from_url(url) else {
		return Err(MoonError::invalid(format!("not a pull request URL: {url}")));
	};
	Ok(fetch_threads(owner, name, number).await)
}

/// [`fetch_detail`] with the review threads filled in.
pub async fn fetch(root: &Utf8Path, number: Option<u32>) -> MoonResult<PullRequestDetail> {
	let mut detail = fetch_detail(root, number).await?;
	if let Some((owner, name)) = repo_from_pr_url(&detail.url) {
		detail.threads = fetch_threads(owner, name, detail.number).await;
	}
	Ok(detail)
}

/// The open pull request for `root`'s current branch. `None` when
/// there isn't one, the remote isn't GitHub, or `gh` can't answer:
/// the review tab works without PR context, so none of those is an
/// error worth interrupting it for.
pub async fn fetch_current(root: &Utf8Path) -> Option<PullRequestDetail> {
	match fetch(root, None).await {
		Ok(detail) => Some(detail),
		Err(err) => {
			tracing::debug!(error = %err, "pull_request: no PR for the current branch");
			None
		}
	}
}

/// The diff of pull request `number` for a tab that needs no checkout
/// of the branch: bring its commits into the repository behind `host`
/// and diff `head_sha` against where it left `base_ref`.
pub async fn diff(host: &dyn WorkspaceHost, number: u32, base_ref: &str, head_sha: &str) -> MoonResult<PrDiff> {
	let diff = host.git_pull_request_diff(number, base_ref, head_sha).await?;
	Ok(PrDiff {
		merge_base: diff.merge_base,
		files: diff.files,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn minor_notes_are_split_from_the_description() {
		let body = "Fixes the thing.\n\n---\nRequested by @me\n\n<!-- moonbot:minor-notes:start -->\n\
			> **Minor notes** <sub>last reviewed `abc1234`</sub>\n>\n\
			> - `src/app.ts:123` — rename `x` to `count`\n\
			> - `README.md` — mention the new flag\n\
			> - `docs/a:b.md:7` — typo\n\
			<!-- moonbot:minor-notes:end -->\n";
		let (prose, notes, sha) = split_minor_notes(body);
		assert_eq!(prose, "Fixes the thing.\n\n---\nRequested by @me");
		assert_eq!(sha.as_deref(), Some("abc1234"));
		assert_eq!(
			notes,
			[
				PrMinorNote {
					path: "src/app.ts".into(),
					line: Some(123),
					text: "rename `x` to `count`".into()
				},
				PrMinorNote {
					path: "README.md".into(),
					line: None,
					text: "mention the new flag".into()
				},
				PrMinorNote {
					path: "docs/a:b.md".into(),
					line: Some(7),
					text: "typo".into()
				},
			]
		);
	}

	#[test]
	fn a_description_without_a_complete_block_is_left_alone() {
		assert_eq!(
			split_minor_notes("Just prose.\n"),
			("Just prose.".to_owned(), Vec::new(), None)
		);
		let unterminated = format!("Prose.\n{NOTES_START}\n> - `a.rs:1` — x");
		assert_eq!(split_minor_notes(&unterminated).1, Vec::new());
	}

	#[test]
	fn the_last_start_marker_wins() {
		let body = format!("{NOTES_START}\n> - `old.rs:1` — stale\n{NOTES_START}\n> - `new.rs:2` — current\n{NOTES_END}");
		let (_, notes, _) = split_minor_notes(&body);
		assert_eq!(notes.len(), 1);
		assert_eq!(notes[0].path, "new.rs");
	}

	#[test]
	fn checks_map_runs_and_statuses() {
		let checks: Vec<RawCheck> = serde_json::from_value(json!([
			{ "__typename": "CheckRun", "name": "test", "workflowName": "CI", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://example.test/1" },
			{ "__typename": "CheckRun", "name": "lint", "workflowName": "", "status": "IN_PROGRESS", "conclusion": "" },
			{ "__typename": "CheckRun", "name": "deploy", "status": "COMPLETED", "conclusion": "CANCELLED" },
			{ "__typename": "StatusContext", "context": "security/scan", "state": "FAILURE", "targetUrl": "" },
		]))
		.unwrap();
		let checks: Vec<PrCheck> = checks.into_iter().map(check_from_raw).collect();
		assert_eq!(checks[0].name, "CI / test");
		assert_eq!(checks[0].state, PrCheckState::Success);
		assert_eq!(checks[0].url.as_deref(), Some("https://example.test/1"));
		assert_eq!(
			(checks[1].name.as_str(), checks[1].state),
			("lint", PrCheckState::Pending)
		);
		assert_eq!(checks[2].state, PrCheckState::Skipped);
		assert_eq!(
			(checks[3].name.as_str(), checks[3].state),
			("security/scan", PrCheckState::Failure)
		);
		assert_eq!(checks[3].url, None);
	}

	#[test]
	fn threads_keep_position_side_and_comments() {
		let data = json!({ "repository": { "pullRequest": { "reviewThreads": { "nodes": [
			{ "isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 12, "diffSide": "RIGHT",
			  "comments": { "nodes": [
				{ "author": { "login": "moon-bot-app" }, "body": "### Title", "createdAt": "2026-11-17T10:00:00Z", "url": "https://example.test/c1" },
				{ "author": null, "body": "reply", "createdAt": "2026-11-17T11:00:00Z", "url": "https://example.test/c2" }
			  ] } },
			{ "isResolved": true, "isOutdated": true, "path": "src/b.rs", "line": null, "diffSide": "LEFT",
			  "comments": { "nodes": [ { "author": { "login": "a" }, "body": "old", "createdAt": "2026-11-16T10:00:00Z", "url": "https://example.test/c3" } ] } },
			{ "isResolved": false, "isOutdated": false, "path": "src/c.rs", "line": 1, "diffSide": "RIGHT", "comments": { "nodes": [] } }
		] } } } });
		let threads = threads_from_data(data);
		assert_eq!(threads.len(), 2);
		assert_eq!(
			(threads[0].path.as_str(), threads[0].line, threads[0].side),
			("src/a.rs", Some(12), ReviewSide::Working)
		);
		assert_eq!(threads[0].comments.len(), 2);
		assert_eq!(threads[0].comments[1].author, None);
		assert!(threads[1].resolved && threads[1].outdated);
		assert_eq!((threads[1].line, threads[1].side), (None, ReviewSide::Base));
	}

	#[test]
	fn pr_view_json_becomes_the_detail() {
		let raw: RawPr = serde_json::from_value(json!({
			"number": 7, "title": "Add scopes", "url": "https://github.com/acme/app/pull/7",
			"body": "Why.\n<!-- moonbot:minor-notes:start -->\n> - `a.rs:3` — nit\n<!-- moonbot:minor-notes:end -->",
			"state": "OPEN", "isDraft": true, "author": { "login": "octocat" }, "reviewDecision": "CHANGES_REQUESTED",
			"baseRefName": "main", "headRefName": "scopes", "headRefOid": "0123456789abcdef0123456789abcdef01234567",
			"isCrossRepository": true,
			"statusCheckRollup": [{ "name": "test", "conclusion": "SUCCESS" }],
		}))
		.unwrap();
		let detail = detail_from_raw(raw);
		assert_eq!(detail.state, InboxItemState::Draft);
		assert_eq!(detail.review, Some(InboxReview::ChangesRequested));
		assert_eq!((detail.base_ref.as_str(), detail.head_ref.as_str()), ("main", "scopes"));
		assert!(detail.is_cross_repository);
		assert_eq!(detail.body, "Why.");
		assert_eq!(detail.minor_notes.len(), 1);
		assert_eq!(detail.checks.len(), 1);
		assert!(detail.threads.is_empty());
	}

	#[test]
	fn a_pr_view_without_the_fork_flag_is_same_repository() {
		let raw: RawPr = serde_json::from_value(json!({
			"number": 1, "title": "t", "url": "u", "state": "MERGED", "author": null, "reviewDecision": "",
			"baseRefName": "main", "headRefName": "f", "headRefOid": "abc",
		}))
		.unwrap();
		let detail = detail_from_raw(raw);
		assert!(!detail.is_cross_repository);
		assert_eq!(detail.state, InboxItemState::Merged);
		assert_eq!((detail.author, detail.review), (None, None));
	}

	/// The git half of opening a PR, against a local bare `origin`
	/// that has `refs/pull/1/head` the way GitHub does: the clone ends
	/// up with the PR's commits and its diff, on the same branch and
	/// with the same working tree as before.
	#[tokio::test]
	async fn open_fetches_the_head_and_diffs_it_against_the_merge_base() {
		use crate::test_util::{run_git, which_git};
		use moon_protocol::git::GitFileStatus;
		use moon_protocol::review::PrDiffFile;

		let Some(git) = which_git() else {
			eprintln!("git not on PATH — skipping pull request open test");
			return;
		};
		let rev = |cwd: &std::path::Path, name: &str| {
			let out = std::process::Command::new(&git)
				.arg("-C")
				.arg(cwd)
				.args(["rev-parse", "--verify", "--quiet", name])
				.output()
				.unwrap();
			String::from_utf8_lossy(&out.stdout).trim().to_owned()
		};

		let root = tempfile::tempdir().unwrap();
		let origin = root.path().join("origin.git");
		let author = root.path().join("author");
		let local = root.path().join("local");
		run_git(&git, root.path(), &["init", "--bare", "-q", "-b", "main", "origin.git"]);
		std::fs::create_dir_all(&author).unwrap();
		crate::test_util::init_committed_repo(&git, &author);
		std::fs::write(author.join("gone.txt"), "old\n").unwrap();
		run_git(&git, &author, &["add", "."]);
		run_git(&git, &author, &["commit", "-q", "-m", "base"]);
		run_git(&git, &author, &["remote", "add", "origin", origin.to_str().unwrap()]);
		run_git(&git, &author, &["push", "-q", "origin", "main"]);
		let fork_point = rev(&author, "HEAD");

		// The reviewer's clone knows `main` as of the fork point only.
		run_git(&git, root.path(), &["clone", "-q", origin.to_str().unwrap(), "local"]);
		std::fs::write(local.join("scratch.txt"), "uncommitted\n").unwrap();

		// The PR: one file changed, one added, one deleted. It reaches
		// `origin` only as `refs/pull/1/head`, like a fork's branch.
		run_git(&git, &author, &["checkout", "-q", "-b", "feature"]);
		std::fs::write(author.join("README.md"), "hi\nthere\n").unwrap();
		std::fs::write(author.join("new.txt"), "new\n").unwrap();
		std::fs::remove_file(author.join("gone.txt")).unwrap();
		run_git(&git, &author, &["add", "-A"]);
		run_git(&git, &author, &["commit", "-q", "-m", "the change"]);
		run_git(&git, &author, &["push", "-q", "origin", "HEAD:refs/pull/1/head"]);
		let head_sha = rev(&author, "HEAD");

		// `main` moves on after the PR branched; the diff must not
		// count that as the PR removing it.
		run_git(&git, &author, &["checkout", "-q", "main"]);
		std::fs::write(author.join("later.txt"), "later\n").unwrap();
		run_git(&git, &author, &["add", "."]);
		run_git(&git, &author, &["commit", "-q", "-m", "main moves on"]);
		run_git(&git, &author, &["push", "-q", "origin", "main"]);
		let base_tip = rev(&author, "HEAD");

		let local_root = camino::Utf8PathBuf::from_path_buf(local.canonicalize().unwrap()).unwrap();
		let host = crate::LocalHost::new(local_root);
		let detail = PullRequestDetail {
			head_sha: head_sha.clone(),
			..detail_from_raw(
				serde_json::from_value(json!({
					"number": 1, "title": "The change", "url": "https://github.com/acme/app/pull/1", "state": "OPEN",
					"author": null, "reviewDecision": null, "baseRefName": "main", "headRefName": "feature",
					"headRefOid": "",
				}))
				.unwrap(),
			)
		};
		let head_before = rev(&local, "HEAD");

		let opened = diff(&host, detail.number, &detail.base_ref, &detail.head_sha)
			.await
			.unwrap();

		assert_eq!(opened.merge_base, fork_point);
		let entry = |path: &str, status, additions, deletions| PrDiffFile {
			path: path.to_owned(),
			status,
			additions: Some(additions),
			deletions: Some(deletions),
		};
		assert_eq!(
			opened.files,
			[
				entry("README.md", GitFileStatus::Modified, 1, 0),
				entry("gone.txt", GitFileStatus::Deleted, 0, 1),
				entry("new.txt", GitFileStatus::Added, 1, 0),
			]
		);
		assert_eq!(rev(&local, "refs/moon/pr/1"), head_sha);
		assert_eq!(rev(&local, "refs/remotes/origin/main"), base_tip);
		// Nothing a checkout or a branch list would notice.
		assert_eq!(rev(&local, "HEAD"), head_before);
		assert_eq!(rev(&local, "refs/heads/feature"), "");
		let status = std::process::Command::new(&git)
			.arg("-C")
			.arg(&local)
			.args(["status", "--porcelain", "--branch"])
			.output()
			.unwrap();
		assert_eq!(
			String::from_utf8_lossy(&status.stdout),
			"## main...origin/main [behind 1]\n?? scratch.txt\n"
		);

		// A PR whose ref `origin` doesn't have is an error, with git's words.
		let err = diff(&host, 2, &detail.base_ref, &detail.head_sha).await.unwrap_err();
		assert!(err.to_string().contains("git fetch"), "{err}");
	}

	#[test]
	fn conversation_merges_comments_and_reviews_that_say_something() {
		let raw: RawPr = serde_json::from_value(json!({
			"number": 7, "title": "T", "url": "https://github.com/acme/app/pull/7", "state": "OPEN",
			"baseRefName": "main", "headRefName": "f", "headRefOid": "",
			"comments": [
				{ "author": { "login": "sam" }, "body": "Second", "createdAt": "2026-11-17T12:00:00Z", "url": "https://example.test/c2" },
				{ "author": null, "body": "First", "createdAt": "2026-11-17T10:00:00Z", "url": "https://example.test/c1" },
			],
			"reviews": [
				{ "author": { "login": "bot" }, "body": "", "submittedAt": "2026-11-17T11:00:00Z", "state": "COMMENTED" },
				{ "author": { "login": "bot" }, "body": "Summary", "submittedAt": "2026-11-17T11:30:00Z", "state": "COMMENTED" },
				{ "author": { "login": "li" }, "body": "", "submittedAt": "2026-11-17T13:00:00Z", "state": "APPROVED" },
				{ "author": { "login": "li" }, "body": "draft", "submittedAt": null, "state": "PENDING" },
			],
		}))
		.unwrap();
		let conversation = detail_from_raw(raw).conversation;
		let seen: Vec<_> = conversation
			.iter()
			.map(|c| (c.author.as_deref(), c.body.as_str(), c.review))
			.collect();
		assert_eq!(
			seen,
			[
				(None, "First", None),
				(Some("bot"), "Summary", Some(ConversationReview::Commented)),
				(Some("sam"), "Second", None),
				(Some("li"), "", Some(ConversationReview::Approved)),
			]
		);
		assert_eq!(conversation[3].url, "https://github.com/acme/app/pull/7");
	}

	#[test]
	fn merge_status_reads_both_fields() {
		assert_eq!(merge_status("CLEAN", "MERGEABLE"), PrMergeStatus::Clean);
		assert_eq!(merge_status("HAS_HOOKS", "MERGEABLE"), PrMergeStatus::Clean);
		assert_eq!(merge_status("UNSTABLE", "MERGEABLE"), PrMergeStatus::Unstable);
		assert_eq!(merge_status("BLOCKED", "MERGEABLE"), PrMergeStatus::Blocked);
		assert_eq!(merge_status("BEHIND", "MERGEABLE"), PrMergeStatus::Behind);
		assert_eq!(merge_status("DIRTY", "CONFLICTING"), PrMergeStatus::Conflicting);
		assert_eq!(merge_status("BLOCKED", "CONFLICTING"), PrMergeStatus::Conflicting);
		assert_eq!(merge_status("UNKNOWN", "UNKNOWN"), PrMergeStatus::Unknown);
		assert_eq!(merge_status("", ""), PrMergeStatus::Unknown);
	}

	#[test]
	fn merge_methods_are_the_allowed_ones_default_first() {
		let settings = |squash, merge, rebase, default: &str| RawMergeSettings {
			squash_merge_allowed: squash,
			merge_commit_allowed: merge,
			rebase_merge_allowed: rebase,
			viewer_default_merge_method: default.to_owned(),
		};
		assert_eq!(
			methods_from_settings(&settings(true, false, false, "SQUASH")),
			[PrMergeMethod::Squash]
		);
		assert_eq!(
			methods_from_settings(&settings(true, true, true, "REBASE")),
			[PrMergeMethod::Rebase, PrMergeMethod::Squash, PrMergeMethod::Merge]
		);
		assert_eq!(
			methods_from_settings(&settings(true, true, false, "")),
			[PrMergeMethod::Squash, PrMergeMethod::Merge]
		);
	}

	#[test]
	fn actions_become_gh_arguments() {
		let url = "https://github.com/acme/app/pull/7";
		let args = |action| action_args(url, &action).map(|args| args.join(" "));
		assert_eq!(
			args(PrAction::Ready).as_deref(),
			Some("pr ready https://github.com/acme/app/pull/7")
		);
		assert_eq!(
			args(PrAction::Draft).as_deref(),
			Some("pr ready https://github.com/acme/app/pull/7 --undo")
		);
		assert_eq!(
			args(PrAction::Close).as_deref(),
			Some("pr close https://github.com/acme/app/pull/7")
		);
		assert_eq!(
			args(PrAction::Reopen).as_deref(),
			Some("pr reopen https://github.com/acme/app/pull/7")
		);
		let sha = "b2293412bfa8143daf8e38b92063aa55b279e63e";
		let merge = |method, head: &str| {
			args(PrAction::Merge {
				method,
				head_sha: head.to_owned(),
			})
		};
		assert_eq!(
			merge(PrMergeMethod::Squash, sha),
			Some(format!("pr merge {url} --squash --match-head-commit {sha}"))
		);
		assert!(merge(PrMergeMethod::Rebase, sha).unwrap().contains(" --rebase "));
		// Never a flag gh would take for something else.
		assert_eq!(merge(PrMergeMethod::Merge, "--admin"), None);
		assert_eq!(merge(PrMergeMethod::Merge, "abc1234"), None);
	}

	#[tokio::test]
	async fn an_action_on_something_that_is_no_pull_request_is_refused() {
		for url in [
			"--admin",
			"https://github.com/acme/app/issues/7",
			"https://github.com/acme/app/pull/x",
		] {
			let err = act(url, &PrAction::Close).await.unwrap_err();
			assert!(err.to_string().contains("not a pull request URL"), "{err}");
		}
	}

	#[test]
	fn repo_and_number_are_read_from_the_pr_url() {
		assert_eq!(
			pr_from_url("https://github.com/acme/app/pull/42"),
			Some(("acme", "app", 42))
		);
		assert_eq!(pr_from_url("https://github.com/acme/app/pull/x"), None);
		assert_eq!(pr_from_url("https://github.com/acme/app/issues/42"), None);
	}

	#[test]
	fn repo_is_read_from_the_pr_url() {
		assert_eq!(
			repo_from_pr_url("https://github.com/acme/app/pull/42"),
			Some(("acme", "app"))
		);
		assert_eq!(repo_from_pr_url("https://github.com/acme/app/issues/42"), None);
		assert_eq!(repo_from_pr_url("nonsense"), None);
	}
}
