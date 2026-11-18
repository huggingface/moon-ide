//! GitHub inbox data (ADR 0112): searches and unread notification
//! threads fetched through the host's `gh`, merged into one item list.
//!
//! Every source is its own `gh` call. One GraphQL request carrying all
//! the searches regularly ran past GitHub's 10 s limit and came back as
//! a 502 with nothing in it; separate calls each stay under it and let
//! a slow source fail alone.

use std::collections::HashMap;
use std::time::Duration;

use moon_protocol::inbox::{
	InboxCi, InboxFailure, InboxItem, InboxItemKind, InboxItemState, InboxNotification, InboxPrDigest, InboxPrRef,
	InboxRepo, InboxReview, InboxSnapshot, InboxSource, InboxStatus,
};
use serde::Deserialize;
use serde_json::Value;

const GH_TIMEOUT: Duration = Duration::from_secs(20);

/// Rows per search. GitHub caps a search page at 100; 50 keeps the
/// request inside its time limit with the check rollup included.
const SEARCH_PAGE: usize = 50;

/// Watched authors beyond this are ignored: each one is a search.
const MAX_WATCHED_AUTHORS: usize = 5;

/// Notification subjects resolved per GraphQL request.
const HYDRATE_CHUNK: usize = 20;

const PR_FIELDS: &str = "number title url state isDraft updatedAt additions deletions headRefName baseRefName \
	isCrossRepository reviewDecision repository { nameWithOwner } author { login avatarUrl } \
	comments { totalCount } commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhError {
	Missing,
	NotAuthed,
	Failed(String),
}

impl GhError {
	pub(crate) fn detail(&self) -> String {
		match self {
			GhError::Missing => "gh is not installed".to_owned(),
			GhError::NotAuthed => "gh is not logged in".to_owned(),
			GhError::Failed(detail) => detail.clone(),
		}
	}
}

pub(crate) struct GhOutput {
	pub(crate) success: bool,
	pub(crate) stdout: Vec<u8>,
	pub(crate) stderr: String,
}

pub(crate) async fn run_gh(args: &[&str]) -> Result<GhOutput, GhError> {
	run_gh_in(None, args).await
}

/// Run the host's `gh`, in `cwd` when the command resolves its
/// repository from the checkout (`gh pr view`).
pub(crate) async fn run_gh_in(cwd: Option<&camino::Utf8Path>, args: &[&str]) -> Result<GhOutput, GhError> {
	let gh = moon_container::resolve_host_gh().unwrap_or_else(|| "gh".into());
	let mut cmd = tokio::process::Command::new(gh);
	if let Some(cwd) = cwd {
		cmd.current_dir(cwd.as_std_path());
	}
	cmd
		.args(args)
		.env("GH_PROMPT_DISABLED", "1")
		.env("LC_ALL", "C")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.kill_on_drop(true);
	let child = match cmd.spawn() {
		Ok(child) => child,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(GhError::Missing),
		Err(err) => return Err(GhError::Failed(format!("gh spawn: {err}"))),
	};
	let output = match tokio::time::timeout(GH_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(output)) => output,
		Ok(Err(err)) => return Err(GhError::Failed(format!("gh: {err}"))),
		Err(_) => return Err(GhError::Failed(format!("gh timed out after {}s", GH_TIMEOUT.as_secs()))),
	};
	Ok(GhOutput {
		success: output.status.success(),
		stdout: output.stdout,
		stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
	})
}

fn classify_failure(stderr: &str) -> GhError {
	let lower = stderr.to_ascii_lowercase();
	if lower.contains("not logged into") || lower.contains("gh auth login") || lower.contains("authentication") {
		return GhError::NotAuthed;
	}
	GhError::Failed(stderr.lines().next().unwrap_or("gh failed").to_owned())
}

fn is_gateway_error(err: &GhError) -> bool {
	matches!(err, GhError::Failed(detail) if detail.contains("HTTP 502") || detail.contains("HTTP 504"))
}

/// Run a GraphQL query and return its `data`. A response carrying both
/// `data` and `errors` (one unreadable repository among several) is
/// still a success: `gh` exits non-zero for it, but the readable part
/// is what we came for. A gateway timeout is retried once, since the
/// second attempt usually hits GitHub's warmed cache.
pub(crate) async fn gh_graphql(query: &str, variables: &[(&str, &str)]) -> Result<Value, GhError> {
	let first = gh_graphql_once(query, variables).await;
	match first {
		Err(err) if is_gateway_error(&err) => gh_graphql_once(query, variables).await,
		other => other,
	}
}

async fn gh_graphql_once(query: &str, variables: &[(&str, &str)]) -> Result<Value, GhError> {
	let query_arg = format!("query={query}");
	let variable_args: Vec<String> = variables
		.iter()
		.map(|(name, value)| format!("{name}={value}"))
		.collect();
	let mut args = vec!["api", "graphql", "-f", query_arg.as_str()];
	for arg in &variable_args {
		args.push("-f");
		args.push(arg);
	}
	let output = run_gh(&args).await?;
	let data = serde_json::from_slice::<Value>(&output.stdout)
		.ok()
		.and_then(|mut body| body.get_mut("data").map(Value::take))
		.filter(Value::is_object);
	match data {
		Some(data) => Ok(data),
		None if output.success => Err(GhError::Failed("gh api graphql: no data in response".to_owned())),
		None => Err(classify_failure(&output.stderr)),
	}
}

async fn gh_rest(method: &str, path: &str) -> Result<Vec<u8>, GhError> {
	let output = run_gh(&["api", "--method", method, path]).await?;
	if !output.success {
		return Err(classify_failure(&output.stderr));
	}
	Ok(output.stdout)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
	number: u32,
	title: String,
	url: String,
	state: String,
	#[serde(default)]
	is_draft: bool,
	updated_at: String,
	additions: Option<u32>,
	deletions: Option<u32>,
	head_ref_name: Option<String>,
	base_ref_name: Option<String>,
	#[serde(default)]
	is_cross_repository: bool,
	review_decision: Option<String>,
	repository: RawRepository,
	author: Option<RawAuthor>,
	comments: Option<RawCount>,
	commits: Option<RawCommits>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRepository {
	name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAuthor {
	login: String,
	avatar_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCount {
	total_count: u32,
}

#[derive(Deserialize)]
struct RawCommits {
	nodes: Vec<RawCommitNode>,
}

#[derive(Deserialize)]
struct RawCommitNode {
	commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
	status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
	state: String,
}

fn pr_item_id(repo: &str, number: u32) -> String {
	format!("{repo}#{number}")
}

/// One search or hydrate node as an item with no sources yet. `None`
/// for anything that isn't a complete pull request (search returns
/// `{}` for node types the fragment doesn't cover).
fn pr_from_node(node: Value) -> Option<InboxItem> {
	let raw: RawPr = serde_json::from_value(node).ok()?;
	let state = match raw.state.as_str() {
		"MERGED" => InboxItemState::Merged,
		"CLOSED" => InboxItemState::Closed,
		_ if raw.is_draft => InboxItemState::Draft,
		_ => InboxItemState::Open,
	};
	let review = match raw.review_decision.as_deref() {
		Some("APPROVED") => Some(InboxReview::Approved),
		Some("CHANGES_REQUESTED") => Some(InboxReview::ChangesRequested),
		Some("REVIEW_REQUIRED") => Some(InboxReview::ReviewRequired),
		_ => None,
	};
	let rollup = raw
		.commits
		.and_then(|commits| commits.nodes.into_iter().next())
		.and_then(|node| node.commit.status_check_rollup);
	let ci = match rollup.as_ref().map(|rollup| rollup.state.as_str()) {
		Some("SUCCESS") => Some(InboxCi::Success),
		Some("FAILURE" | "ERROR") => Some(InboxCi::Failure),
		Some("PENDING" | "EXPECTED") => Some(InboxCi::Pending),
		_ => None,
	};
	let repo = raw.repository.name_with_owner;
	let (author, author_avatar_url) = match raw.author {
		Some(author) => (Some(author.login), author.avatar_url),
		None => (None, None),
	};
	Some(InboxItem {
		id: pr_item_id(&repo, raw.number),
		kind: InboxItemKind::PullRequest,
		repo,
		number: Some(raw.number),
		title: raw.title,
		url: raw.url,
		state: Some(state),
		author,
		author_avatar_url,
		updated_at: raw.updated_at,
		head_ref: raw.head_ref_name,
		base_ref: raw.base_ref_name,
		is_cross_repository: raw.is_cross_repository,
		additions: raw.additions,
		deletions: raw.deletions,
		comment_count: raw.comments.map(|count| count.total_count),
		review,
		ci,
		sources: Vec::new(),
		notification: None,
	})
}

struct SearchResult {
	viewer: String,
	items: Vec<InboxItem>,
}

async fn search_prs(query: &str) -> Result<SearchResult, GhError> {
	let graphql = format!(
		"query($q: String!) {{ viewer {{ login }} search(query: $q, type: ISSUE, first: {SEARCH_PAGE}) \
		 {{ nodes {{ ... on PullRequest {{ {PR_FIELDS} }} }} }} }}"
	);
	let mut data = gh_graphql(&graphql, &[("q", query)]).await?;
	let viewer = data
		.pointer("/viewer/login")
		.and_then(Value::as_str)
		.unwrap_or_default()
		.to_owned();
	let nodes = match data.pointer_mut("/search/nodes").map(Value::take) {
		Some(Value::Array(nodes)) => nodes,
		_ => Vec::new(),
	};
	Ok(SearchResult {
		viewer,
		items: nodes.into_iter().filter_map(pr_from_node).collect(),
	})
}

/// Search qualifier for a watched author, or `None` when the name
/// can't be one. Accepts a login (`octocat`, `@octocat`), a bot as
/// GitHub displays it (`moon-bot-app[bot]`) or in search syntax
/// (`app/moon-bot-app`).
fn author_qualifier(author: &str) -> Option<String> {
	let name = author.trim().trim_start_matches('@');
	let (prefix, login) = match (name.strip_suffix("[bot]"), name.strip_prefix("app/")) {
		(Some(bot), _) => ("app/", bot),
		(None, Some(app)) => ("app/", app),
		(None, None) => ("", name),
	};
	let valid = !login.is_empty() && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
	valid.then(|| format!("author:{prefix}{login}"))
}

fn is_repo_slug(repo: &str) -> bool {
	let part = |s: &str| {
		!s.is_empty()
			&& s
				.chars()
				.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
	};
	matches!(repo.split_once('/'), Some((owner, name)) if part(owner) && part(name))
}

/// The searches one poll runs, in display-priority order. Every one
/// is scoped to the bound repositories; with none bound there is
/// nothing to search.
fn search_specs(watched_authors: &[String], repos: &[InboxRepo]) -> Vec<(InboxSource, String)> {
	let repo_qualifiers: Vec<String> = repos
		.iter()
		.filter(|repo| is_repo_slug(&repo.repo))
		.map(|repo| format!("repo:{}", repo.repo))
		.collect();
	if repo_qualifiers.is_empty() {
		return Vec::new();
	}
	let scope = repo_qualifiers.join(" ");
	let mut specs = vec![
		(
			InboxSource::ReviewRequested,
			format!("is:pr is:open review-requested:@me {scope}"),
		),
		(InboxSource::Mentioned, format!("is:pr is:open mentions:@me {scope}")),
		(InboxSource::Mine, format!("is:pr is:open author:@me {scope}")),
	];
	for qualifier in watched_authors
		.iter()
		.filter_map(|author| author_qualifier(author))
		.take(MAX_WATCHED_AUTHORS)
	{
		specs.push((InboxSource::Watched, format!("is:pr is:open {qualifier} {scope}")));
	}
	specs
}

#[derive(Deserialize)]
struct RawThread {
	id: String,
	reason: String,
	updated_at: String,
	subject: RawSubject,
	repository: RawThreadRepository,
}

#[derive(Deserialize)]
struct RawSubject {
	title: String,
	url: Option<String>,
	#[serde(rename = "type")]
	kind: String,
}

#[derive(Deserialize)]
struct RawThreadRepository {
	full_name: String,
	html_url: String,
}

/// Trailing number of a subject API URL (`…/pulls/42`, `…/issues/7`).
fn subject_number(url: Option<&str>) -> Option<u32> {
	let (rest, number) = url?.rsplit_once('/')?;
	if !rest.ends_with("/pulls") && !rest.ends_with("/issues") {
		return None;
	}
	number.parse().ok()
}

/// A thread as an item, from the notification alone. Used as-is for
/// issues and other subjects; a pull request is replaced by its
/// hydrated row when that fetch succeeds.
fn item_from_thread(thread: RawThread) -> InboxItem {
	let number = subject_number(thread.subject.url.as_deref());
	let repo = thread.repository.full_name;
	let (kind, id, url) = match (thread.subject.kind.as_str(), number) {
		("PullRequest", Some(number)) => (
			InboxItemKind::PullRequest,
			pr_item_id(&repo, number),
			format!("{}/pull/{number}", thread.repository.html_url),
		),
		("Issue", Some(number)) => (
			InboxItemKind::Issue,
			pr_item_id(&repo, number),
			format!("{}/issues/{number}", thread.repository.html_url),
		),
		_ => (
			InboxItemKind::Other,
			format!("thread:{}", thread.id),
			thread.repository.html_url,
		),
	};
	InboxItem {
		id,
		kind,
		repo,
		number: number.filter(|_| kind != InboxItemKind::Other),
		title: thread.subject.title,
		url,
		state: None,
		author: None,
		author_avatar_url: None,
		updated_at: thread.updated_at.clone(),
		head_ref: None,
		base_ref: None,
		is_cross_repository: false,
		additions: None,
		deletions: None,
		comment_count: None,
		review: None,
		ci: None,
		sources: vec![InboxSource::Notification],
		notification: Some(InboxNotification {
			thread_id: thread.id,
			reason: thread.reason,
			updated_at: thread.updated_at,
		}),
	}
}

/// Page size of the notifications API (its maximum), and how many
/// pages a poll reads before giving up on finding more threads in the
/// bound repositories.
const THREADS_PAGE: usize = 50;
const THREADS_MAX_PAGES: usize = 3;

/// Unread threads in `repos`. The API lists the whole account's, so
/// the others are dropped here; one account-wide call is cheaper than
/// a call per bound repository.
async fn unread_threads(repos: &[InboxRepo]) -> Result<Vec<InboxItem>, GhError> {
	let mut items = Vec::new();
	for page in 1..=THREADS_MAX_PAGES {
		let body = gh_rest("GET", &format!("notifications?per_page={THREADS_PAGE}&page={page}")).await?;
		let threads: Vec<RawThread> =
			serde_json::from_slice(&body).map_err(|e| GhError::Failed(format!("notifications JSON: {e}")))?;
		let full_page = threads.len() == THREADS_PAGE;
		items.extend(
			threads
				.into_iter()
				.filter(|thread| is_bound(repos, &thread.repository.full_name))
				.map(item_from_thread),
		);
		if !full_page {
			break;
		}
	}
	Ok(items)
}

/// GitHub treats repository names case-insensitively, and a remote
/// URL need not match the canonical casing the API returns.
fn is_bound(repos: &[InboxRepo], full_name: &str) -> bool {
	repos.iter().any(|repo| repo.repo.eq_ignore_ascii_case(full_name))
}

/// Query reading `fields` of the pull requests at `(repo, number)`,
/// aliased `n0`, `n1`, … in input order. Pairs whose repo isn't a
/// plain `owner/name` are left out (their alias is simply absent).
fn subjects_query(subjects: &[(String, u32)], fields: &str) -> String {
	let fields: Vec<String> = subjects
		.iter()
		.enumerate()
		.filter(|(_, (repo, _))| is_repo_slug(repo))
		.filter_map(|(index, (repo, number))| {
			let (owner, name) = repo.split_once('/')?;
			Some(format!(
				"n{index}: repository(owner: \"{owner}\", name: \"{name}\") {{ pullRequest(number: {number}) {{ {fields} }} }}"
			))
		})
		.collect();
	format!("query {{ {} }}", fields.join(" "))
}

/// Fetch the pull requests behind notification threads that no search
/// returned. Best-effort: a thread whose PR can't be read keeps its
/// notification-only row.
async fn hydrate_prs(subjects: &[(String, u32)]) -> HashMap<String, InboxItem> {
	let mut hydrated = HashMap::new();
	for chunk in subjects.chunks(HYDRATE_CHUNK) {
		let mut data = match gh_graphql(&subjects_query(chunk, PR_FIELDS), &[]).await {
			Ok(data) => data,
			Err(err) => {
				tracing::debug!(error = %err.detail(), "inbox: hydrating notification subjects failed");
				continue;
			}
		};
		for index in 0..chunk.len() {
			let node = data.pointer_mut(&format!("/n{index}/pullRequest")).map(Value::take);
			if let Some(item) = node.and_then(pr_from_node) {
				hydrated.insert(item.id.clone(), item);
			}
		}
	}
	hydrated
}

const DIGEST_FIELDS: &str = "body reviewThreads(first: 100) { nodes { isResolved } } \
	 files(first: 100) { totalCount nodes { path additions deletions } }";

/// How many changed paths [`InboxPrDigest::paths`] keeps.
const DIGEST_PATHS: usize = 30;

/// How much of a description [`InboxPrDigest::summary`] keeps.
const SUMMARY_CHARS: usize = 400;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDigest {
	#[serde(default)]
	body: String,
	review_threads: RawNodes<RawDigestThread>,
	files: Option<RawDigestFiles>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDigestFiles {
	#[serde(default)]
	total_count: usize,
	nodes: Vec<RawDigestFile>,
}

#[derive(Deserialize)]
struct RawNodes<T> {
	nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDigestThread {
	is_resolved: bool,
}

#[derive(Deserialize)]
struct RawDigestFile {
	path: String,
	#[serde(default)]
	additions: u32,
	#[serde(default)]
	deletions: u32,
}

/// A description as one line of plain-ish text: HTML comments (bot
/// markers, templates) dropped, whitespace collapsed, cut short.
fn summarise(prose: &str) -> String {
	let mut text = String::new();
	let mut rest = prose;
	while let Some(start) = rest.find("<!--") {
		text.push_str(&rest[..start]);
		rest = rest[start..].find("-->").map_or("", |end| &rest[start + end + 3..]);
	}
	text.push_str(rest);
	let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
	match collapsed.char_indices().nth(SUMMARY_CHARS) {
		Some((cut, _)) => format!("{}…", &collapsed[..cut]),
		None => collapsed,
	}
}

fn digest_from_node(id: String, node: Value) -> Option<InboxPrDigest> {
	let raw: RawDigest = serde_json::from_value(node).ok()?;
	let (prose, notes, _) = crate::pull_request::split_minor_notes(&raw.body);
	let open = raw
		.review_threads
		.nodes
		.iter()
		.filter(|thread| !thread.is_resolved)
		.count();
	// Lines in test files, known only when every file was read.
	let in_tests = raw
		.files
		.as_ref()
		.filter(|files| files.total_count <= files.nodes.len())
		.map(|files| {
			files
				.nodes
				.iter()
				.filter(|file| crate::pull_request::is_test_path(&file.path))
				.fold((0, 0), |(added, removed), file| {
					(added + file.additions, removed + file.deletions)
				})
		});
	Some(InboxPrDigest {
		id,
		minor_notes: notes.len() as u32,
		open_threads: open as u32,
		resolved_threads: (raw.review_threads.nodes.len() - open) as u32,
		paths: raw
			.files
			.map(|files| {
				files
					.nodes
					.into_iter()
					.take(DIGEST_PATHS)
					.map(|file| file.path)
					.collect()
			})
			.unwrap_or_default(),
		summary: summarise(&prose),
		test_additions: in_tests.map(|(added, _)| added),
		test_deletions: in_tests.map(|(_, removed)| removed),
	})
}

/// Notes, review threads, changed paths and the start of the
/// description for each of `subjects`. Best-effort: a pull request
/// that can't be read is missing from the answer.
pub async fn digests(subjects: &[InboxPrRef]) -> Vec<InboxPrDigest> {
	let pairs: Vec<(String, u32)> = subjects.iter().map(|s| (s.repo.clone(), s.number)).collect();
	let mut out = Vec::new();
	for chunk in pairs.chunks(HYDRATE_CHUNK) {
		let mut data = match gh_graphql(&subjects_query(chunk, DIGEST_FIELDS), &[]).await {
			Ok(data) => data,
			Err(err) => {
				tracing::debug!(error = %err.detail(), "inbox: reading pull request digests failed");
				continue;
			}
		};
		for (index, (repo, number)) in chunk.iter().enumerate() {
			let node = data.pointer_mut(&format!("/n{index}/pullRequest")).map(Value::take);
			out.extend(node.and_then(|node| digest_from_node(pr_item_id(repo, *number), node)));
		}
	}
	out
}

/// Fold search results and unread threads into one list, newest first.
/// An item several sources returned appears once with all of them.
fn merge(
	searches: Vec<(InboxSource, Vec<InboxItem>)>,
	threads: Vec<InboxItem>,
	mut hydrated: HashMap<String, InboxItem>,
) -> Vec<InboxItem> {
	let mut order: Vec<String> = Vec::new();
	let mut by_id: HashMap<String, InboxItem> = HashMap::new();
	for (source, items) in searches {
		for item in items {
			let entry = by_id.entry(item.id.clone()).or_insert_with(|| {
				order.push(item.id.clone());
				item
			});
			if !entry.sources.contains(&source) {
				entry.sources.push(source);
			}
		}
	}
	for thread in threads {
		let entry = by_id.entry(thread.id.clone()).or_insert_with(|| {
			order.push(thread.id.clone());
			match hydrated.remove(&thread.id) {
				Some(item) => item,
				None => InboxItem {
					sources: Vec::new(),
					notification: None,
					..thread.clone()
				},
			}
		});
		entry.sources.push(InboxSource::Notification);
		entry.notification = thread.notification;
	}
	let mut items: Vec<InboxItem> = order.into_iter().filter_map(|id| by_id.remove(&id)).collect();
	items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
	items
}

/// Poll GitHub for what wants the user in `repos`, the bound folders'
/// GitHub repositories. With none there is nothing to ask.
pub async fn fetch(watched_authors: &[String], repos: Vec<InboxRepo>) -> InboxSnapshot {
	if repos.is_empty() {
		return InboxSnapshot::default();
	}
	let mut searches = tokio::task::JoinSet::new();
	for (index, (source, query)) in search_specs(watched_authors, &repos).into_iter().enumerate() {
		searches.spawn(async move { (index, source, search_prs(&query).await) });
	}
	let threads = unread_threads(&repos);
	let (mut search_results, threads) = tokio::join!(searches.join_all(), threads);
	search_results.sort_by_key(|(index, _, _)| *index);

	let mut failures: Vec<(InboxSource, GhError)> = Vec::new();
	let mut viewer = String::new();
	let mut found: Vec<(InboxSource, Vec<InboxItem>)> = Vec::new();
	for (_, source, result) in search_results {
		match result {
			Ok(result) => {
				if viewer.is_empty() {
					viewer = result.viewer;
				}
				found.push((source, result.items));
			}
			Err(err) => failures.push((source, err)),
		}
	}
	let threads = match threads {
		Ok(threads) => threads,
		Err(err) => {
			failures.push((InboxSource::Notification, err));
			Vec::new()
		}
	};

	let nothing_worked = found.is_empty() && failures.iter().any(|(source, _)| *source == InboxSource::Notification);
	let status = match failures.first() {
		Some((_, GhError::Missing)) if nothing_worked => InboxStatus::GhMissing,
		Some((_, GhError::NotAuthed)) if nothing_worked => InboxStatus::GhNotAuthed,
		_ => InboxStatus::Ok,
	};
	if status != InboxStatus::Ok {
		return InboxSnapshot {
			status,
			repos,
			..InboxSnapshot::default()
		};
	}

	let known: std::collections::HashSet<&str> = found
		.iter()
		.flat_map(|(_, items)| items.iter().map(|item| item.id.as_str()))
		.collect();
	let to_hydrate: Vec<(String, u32)> = threads
		.iter()
		.filter(|thread| thread.kind == InboxItemKind::PullRequest && !known.contains(thread.id.as_str()))
		.filter_map(|thread| Some((thread.repo.clone(), thread.number?)))
		.collect();
	let hydrated = hydrate_prs(&to_hydrate).await;

	InboxSnapshot {
		status,
		viewer,
		items: merge(found, threads, hydrated),
		repos,
		failures: failures
			.into_iter()
			.map(|(source, err)| InboxFailure {
				source,
				detail: err.detail(),
			})
			.collect(),
	}
}

/// Mark a notification thread read. GitHub then drops it from the
/// unread list until the subject has new activity.
pub async fn mark_read(thread_id: &str) -> Result<(), GhError> {
	if thread_id.is_empty() || !thread_id.chars().all(|c| c.is_ascii_digit()) {
		return Err(GhError::Failed(format!("not a notification thread id: {thread_id}")));
	}
	gh_rest("PATCH", &format!("notifications/threads/{thread_id}")).await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn pr_node(repo: &str, number: u32, updated_at: &str) -> Value {
		json!({
			"number": number,
			"title": format!("PR {number}"),
			"url": format!("https://github.com/{repo}/pull/{number}"),
			"state": "OPEN",
			"isDraft": false,
			"updatedAt": updated_at,
			"additions": 12,
			"deletions": 3,
			"headRefName": "feature",
			"baseRefName": "main",
			"isCrossRepository": false,
			"reviewDecision": "REVIEW_REQUIRED",
			"repository": { "nameWithOwner": repo },
			"author": { "login": "octocat", "avatarUrl": "https://example.test/a.png" },
			"comments": { "totalCount": 4 },
			"commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "FAILURE" } } }] },
		})
	}

	fn thread(id: &str, repo: &str, kind: &str, subject_url: Option<&str>) -> RawThread {
		RawThread {
			id: id.to_owned(),
			reason: "review_requested".to_owned(),
			updated_at: "2026-11-17T10:00:00Z".to_owned(),
			subject: RawSubject {
				title: "A subject".to_owned(),
				url: subject_url.map(str::to_owned),
				kind: kind.to_owned(),
			},
			repository: RawThreadRepository {
				full_name: repo.to_owned(),
				html_url: format!("https://github.com/{repo}"),
			},
		}
	}

	#[test]
	fn pr_node_maps_state_review_and_ci() {
		let item = pr_from_node(pr_node("acme/app", 7, "2026-11-17T09:00:00Z")).expect("a pull request");
		assert_eq!(item.id, "acme/app#7");
		assert_eq!(item.state, Some(InboxItemState::Open));
		assert_eq!(item.review, Some(InboxReview::ReviewRequired));
		assert_eq!(item.ci, Some(InboxCi::Failure));
		assert_eq!(item.comment_count, Some(4));
		assert_eq!(item.author.as_deref(), Some("octocat"));

		let mut draft = pr_node("acme/app", 8, "2026-11-17T09:00:00Z");
		draft["isDraft"] = json!(true);
		draft["commits"] = json!({ "nodes": [{ "commit": { "statusCheckRollup": null } }] });
		draft["reviewDecision"] = Value::Null;
		draft["author"] = Value::Null;
		let item = pr_from_node(draft).expect("a pull request");
		assert_eq!(item.state, Some(InboxItemState::Draft));
		assert_eq!(item.ci, None);
		assert_eq!(item.review, None);
		assert_eq!(item.author, None);

		let mut merged = pr_node("acme/app", 9, "2026-11-17T09:00:00Z");
		merged["state"] = json!("MERGED");
		assert_eq!(
			pr_from_node(merged).and_then(|item| item.state),
			Some(InboxItemState::Merged)
		);
	}

	#[test]
	fn non_pr_search_nodes_are_skipped() {
		assert!(pr_from_node(json!({})).is_none());
	}

	#[test]
	fn author_qualifier_handles_users_and_bots() {
		assert_eq!(author_qualifier("octocat").as_deref(), Some("author:octocat"));
		assert_eq!(author_qualifier(" @octocat ").as_deref(), Some("author:octocat"));
		assert_eq!(
			author_qualifier("moon-bot-app[bot]").as_deref(),
			Some("author:app/moon-bot-app")
		);
		assert_eq!(
			author_qualifier("app/moon-bot-app").as_deref(),
			Some("author:app/moon-bot-app")
		);
		assert_eq!(author_qualifier(""), None);
		assert_eq!(author_qualifier("a b"), None);
		assert_eq!(author_qualifier("x repo:evil/repo"), None);
	}

	#[test]
	fn searches_are_scoped_to_the_bound_repos() {
		let authors = vec!["octocat".to_owned()];
		assert!(search_specs(&authors, &[]).is_empty());

		let repos = vec![
			InboxRepo {
				repo: "acme/app".to_owned(),
				folder_path: "/code/app".to_owned(),
			},
			InboxRepo {
				repo: "acme/lib".to_owned(),
				folder_path: "/code/lib".to_owned(),
			},
		];
		let specs = search_specs(&authors, &repos);
		assert_eq!(specs.len(), 4);
		assert_eq!(
			specs[0],
			(
				InboxSource::ReviewRequested,
				"is:pr is:open review-requested:@me repo:acme/app repo:acme/lib".to_owned()
			)
		);
		assert_eq!(
			specs[3],
			(
				InboxSource::Watched,
				"is:pr is:open author:octocat repo:acme/app repo:acme/lib".to_owned()
			)
		);
	}

	#[test]
	fn bound_repos_match_whatever_the_casing() {
		let repos = vec![InboxRepo {
			repo: "Acme/App".to_owned(),
			folder_path: "/code/app".to_owned(),
		}];
		assert!(is_bound(&repos, "acme/app"));
		assert!(!is_bound(&repos, "acme/other"));
	}

	#[test]
	fn subject_number_reads_pull_and_issue_urls() {
		assert_eq!(
			subject_number(Some("https://api.github.com/repos/acme/app/pulls/42")),
			Some(42)
		);
		assert_eq!(
			subject_number(Some("https://api.github.com/repos/acme/app/issues/7")),
			Some(7)
		);
		assert_eq!(
			subject_number(Some("https://api.github.com/repos/acme/app/releases/7")),
			None
		);
		assert_eq!(subject_number(None), None);
	}

	#[test]
	fn threads_become_items_by_subject_kind() {
		let pr = item_from_thread(thread(
			"1",
			"acme/app",
			"PullRequest",
			Some("https://api.github.com/repos/acme/app/pulls/42"),
		));
		assert_eq!(pr.id, "acme/app#42");
		assert_eq!(pr.kind, InboxItemKind::PullRequest);
		assert_eq!(pr.url, "https://github.com/acme/app/pull/42");

		let issue = item_from_thread(thread(
			"2",
			"acme/app",
			"Issue",
			Some("https://api.github.com/repos/acme/app/issues/7"),
		));
		assert_eq!(issue.kind, InboxItemKind::Issue);
		assert_eq!(issue.url, "https://github.com/acme/app/issues/7");

		let other = item_from_thread(thread("3", "acme/app", "Discussion", None));
		assert_eq!(other.id, "thread:3");
		assert_eq!(other.kind, InboxItemKind::Other);
		assert_eq!(other.number, None);
		assert_eq!(other.url, "https://github.com/acme/app");
	}

	#[test]
	fn subjects_query_aliases_by_position_and_skips_odd_repos() {
		let query = subjects_query(
			&[
				("acme/app".to_owned(), 42),
				("bad\"repo/x".to_owned(), 1),
				("acme/lib".to_owned(), 7),
			],
			"title",
		);
		assert!(query.contains("n0: repository(owner: \"acme\", name: \"app\") { pullRequest(number: 42) { title } }"));
		assert!(!query.contains("n1:"));
		assert!(query.contains("n2: repository(owner: \"acme\", name: \"lib\") { pullRequest(number: 7)"));
	}

	#[test]
	fn digest_counts_notes_and_threads_and_trims_the_description() {
		let body = format!(
			"<!-- template -->\nFixes the   cursor.\n\n{}\n\n<!-- moonbot:minor-notes:start -->\n\
			 > **Minor notes** <sub>last reviewed `abc1234`</sub>\n>\n> - `a.rs:1` — x\n> - `b.rs` — y\n\
			 <!-- moonbot:minor-notes:end -->\n",
			"word ".repeat(200)
		);
		let digest = digest_from_node(
			"acme/app#7".to_owned(),
			json!({
				"body": body,
				"reviewThreads": { "nodes": [{ "isResolved": false }, { "isResolved": true }, { "isResolved": false }] },
				"files": { "totalCount": 3, "nodes": [
					{ "path": "src/a.rs", "additions": 10, "deletions": 4 },
					{ "path": "src/b.rs", "additions": 1, "deletions": 0 },
					{ "path": "server/test/a.ts", "additions": 60, "deletions": 2 },
				] },
			}),
		)
		.expect("a digest");
		assert_eq!((digest.test_additions, digest.test_deletions), (Some(60), Some(2)));
		assert_eq!(
			(digest.minor_notes, digest.open_threads, digest.resolved_threads),
			(2, 2, 1)
		);
		assert_eq!(digest.paths, ["src/a.rs", "src/b.rs", "server/test/a.ts"]);
		assert!(
			digest.summary.starts_with("Fixes the cursor. word word"),
			"{}",
			digest.summary
		);
		assert!(digest.summary.ends_with('…') && digest.summary.chars().count() == SUMMARY_CHARS + 1);
		assert!(!digest.summary.contains("template") && !digest.summary.contains("Minor notes"));
	}

	#[test]
	fn digest_does_not_guess_test_lines_from_a_partial_file_list() {
		let digest = digest_from_node(
			"acme/app#8".to_owned(),
			json!({
				"body": "",
				"reviewThreads": { "nodes": [] },
				"files": { "totalCount": 240, "nodes": [{ "path": "tests/a.rs", "additions": 5, "deletions": 5 }] },
			}),
		)
		.expect("a digest");
		assert_eq!((digest.test_additions, digest.test_deletions), (None, None));
	}

	#[test]
	fn summarise_survives_an_unclosed_comment_and_wide_characters() {
		assert_eq!(summarise("Before <!-- never closed"), "Before");
		assert_eq!(
			summarise(&"é".repeat(SUMMARY_CHARS + 5)).chars().count(),
			SUMMARY_CHARS + 1
		);
		assert_eq!(summarise("  "), "");
	}

	#[test]
	fn merge_unions_sources_and_attaches_threads() {
		let from_search =
			|number: u32, updated_at: &str| pr_from_node(pr_node("acme/app", number, updated_at)).expect("a pull request");
		let searches = vec![
			(
				InboxSource::ReviewRequested,
				vec![from_search(1, "2026-11-17T08:00:00Z")],
			),
			(
				InboxSource::Mentioned,
				vec![
					from_search(1, "2026-11-17T08:00:00Z"),
					from_search(2, "2026-11-17T09:00:00Z"),
				],
			),
		];
		let threads = vec![
			item_from_thread(thread(
				"11",
				"acme/app",
				"PullRequest",
				Some("https://api.github.com/repos/acme/app/pulls/1"),
			)),
			item_from_thread(thread(
				"12",
				"acme/app",
				"PullRequest",
				Some("https://api.github.com/repos/acme/app/pulls/3"),
			)),
			item_from_thread(thread(
				"13",
				"acme/app",
				"PullRequest",
				Some("https://api.github.com/repos/acme/app/pulls/4"),
			)),
		];
		let mut hydrated = HashMap::new();
		hydrated.insert("acme/app#3".to_owned(), from_search(3, "2026-11-17T07:00:00Z"));

		let items = merge(searches, threads, hydrated);
		let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
		assert_eq!(ids, ["acme/app#4", "acme/app#2", "acme/app#1", "acme/app#3"]);

		let one = &items[2];
		assert_eq!(
			one.sources,
			[
				InboxSource::ReviewRequested,
				InboxSource::Mentioned,
				InboxSource::Notification
			]
		);
		assert_eq!(one.notification.as_ref().map(|n| n.thread_id.as_str()), Some("11"));

		let hydrated = &items[3];
		assert_eq!(hydrated.sources, [InboxSource::Notification]);
		assert_eq!(hydrated.ci, Some(InboxCi::Failure));
		assert_eq!(hydrated.notification.as_ref().map(|n| n.thread_id.as_str()), Some("12"));

		let bare = &items[0];
		assert_eq!(bare.state, None);
		assert_eq!(bare.sources, [InboxSource::Notification]);
	}

	#[tokio::test]
	async fn mark_read_rejects_non_numeric_ids() {
		assert!(mark_read("12; rm -rf").await.is_err());
		assert!(mark_read("").await.is_err());
	}
}
