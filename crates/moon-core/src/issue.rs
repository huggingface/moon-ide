//! An issue of a checkout's GitHub repository, for its tab (ADR 0117).

use camino::Utf8Path;
use moon_protocol::inbox::{IssueDetail, IssueLabel};
use moon_protocol::{MoonError, MoonResult};
use serde::Deserialize;

use crate::inbox::run_gh_in;
use crate::pull_request::{RawAuthor, RawComment};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssue {
	number: u32,
	title: String,
	url: String,
	#[serde(default)]
	body: String,
	state: String,
	author: Option<RawAuthor>,
	created_at: String,
	#[serde(default)]
	labels: Vec<IssueLabel>,
	#[serde(default)]
	assignees: Vec<RawAuthor>,
	#[serde(default)]
	comments: Vec<RawComment>,
}

const ISSUE_VIEW_FIELDS: &str = "number,title,url,body,state,author,createdAt,labels,assignees,comments";

fn detail_from_raw(raw: RawIssue) -> IssueDetail {
	IssueDetail {
		number: raw.number,
		title: raw.title,
		url: raw.url,
		body: raw.body,
		open: raw.state == "OPEN",
		author: raw.author.map(|author| author.login),
		created_at: raw.created_at,
		labels: raw.labels,
		assignees: raw.assignees.into_iter().map(|assignee| assignee.login).collect(),
		comments: raw.comments.into_iter().filter_map(RawComment::into_comment).collect(),
	}
}

/// Issue `number` of `root`'s GitHub repository. Errors when `gh`
/// can't answer or there is no such issue (a pull request number is
/// one of those: `gh issue view` refuses it).
pub async fn fetch(root: &Utf8Path, number: u32) -> MoonResult<IssueDetail> {
	let number_arg = number.to_string();
	let output = run_gh_in(Some(root), &["issue", "view", &number_arg, "--json", ISSUE_VIEW_FIELDS])
		.await
		.map_err(|err| MoonError::IoError(err.detail()))?;
	if !output.success {
		return Err(MoonError::IoError(format!("gh issue view: {}", output.stderr)));
	}
	let raw: RawIssue =
		serde_json::from_slice(&output.stdout).map_err(|e| MoonError::Internal(format!("gh issue view JSON: {e}")))?;
	Ok(detail_from_raw(raw))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn gh_output_becomes_the_detail() {
		let raw: RawIssue = serde_json::from_value(json!({
			"number": 77, "title": "Docs build fails", "url": "https://github.com/acme/docs/issues/77",
			"body": "It fails.", "state": "CLOSED",
			"author": { "login": "maria", "is_bot": false }, "createdAt": "2026-11-10T09:00:00Z",
			"labels": [{ "id": "L1", "name": "bug", "color": "d73a4a", "description": "" }],
			"assignees": [{ "id": "U1", "login": "sam", "name": "Sam" }],
			"comments": [
				{ "author": { "login": "sam" }, "body": "Looking.", "createdAt": "2026-11-10T10:00:00Z", "url": "https://example.test/c1" },
				{ "author": { "login": "spam" }, "body": "x", "createdAt": "2026-11-10T11:00:00Z", "url": "u", "isMinimized": true },
			],
		}))
		.unwrap();
		let detail = detail_from_raw(raw);
		assert!(!detail.open);
		assert_eq!(detail.author.as_deref(), Some("maria"));
		assert_eq!(
			(detail.labels[0].name.as_str(), detail.labels[0].color.as_str()),
			("bug", "d73a4a")
		);
		assert_eq!(detail.assignees, ["sam"]);
		assert_eq!((detail.comments.len(), detail.comments[0].review), (1, None));
	}

	#[test]
	fn missing_optional_fields_default() {
		let raw: RawIssue = serde_json::from_value(json!({
			"number": 1, "title": "T", "url": "u", "state": "OPEN", "author": null,
			"createdAt": "2026-11-10T09:00:00Z",
		}))
		.unwrap();
		let detail = detail_from_raw(raw);
		assert!(detail.open);
		assert_eq!(
			(detail.author, detail.body.as_str(), detail.comments.len()),
			(None, "", 0)
		);
	}
}
