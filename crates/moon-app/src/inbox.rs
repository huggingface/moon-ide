//! GitHub inbox commands (ADR 0112).

use camino::Utf8Path;
use moon_core::forge::{detect_forge_remote, ForgeKind};
use moon_core::host::WorktreeBranch;
use moon_core::inbox;
use moon_protocol::inbox::{
	InboxGroupRequest, InboxGrouping, InboxOpenedPr, InboxPrDigest, InboxPrRef, InboxRepo, InboxSnapshot, IssueDetail,
};
use moon_protocol::review::{PrAction, PrDiff, PrThread, PullRequestDetail};
use moon_protocol::workspace::FolderOrigin;
use moon_protocol::MoonError;

use crate::AppCore;

/// GitHub repositories of the bound project folders. Worktrees are
/// left out: they share their project's remote.
async fn bound_repos(app: &AppCore) -> Vec<InboxRepo> {
	let roots: Vec<String> = app
		.workspaces
		.folders()
		.await
		.into_iter()
		.filter(|entry| matches!(entry.folder.origin, FolderOrigin::UserPicked))
		.map(|entry| entry.folder.path.clone())
		.collect();
	tokio::task::spawn_blocking(move || {
		let mut repos: Vec<InboxRepo> = Vec::new();
		for folder_path in roots {
			let Some(remote) = detect_forge_remote(Utf8Path::new(&folder_path)) else {
				continue;
			};
			if remote.kind != ForgeKind::GitHub || repos.iter().any(|known| known.repo == remote.owner_repo) {
				continue;
			}
			repos.push(InboxRepo {
				repo: remote.owner_repo,
				folder_path,
			});
		}
		repos
	})
	.await
	.unwrap_or_default()
}

pub async fn inbox_fetch(app: &AppCore, watched_authors: Vec<String>) -> Result<InboxSnapshot, MoonError> {
	let repos = bound_repos(app).await;
	Ok(inbox::fetch(&watched_authors, repos).await)
}

/// Notes, review threads, changed paths and the start of the
/// description of each pull request in `subjects`. One that can't be
/// read is missing from the answer.
pub async fn inbox_digests(subjects: Vec<InboxPrRef>) -> Result<Vec<InboxPrDigest>, MoonError> {
	Ok(inbox::digests(&subjects).await)
}

/// Loadable, short-lived URLs for images attached to a pull request
/// or an issue of the private repository `repo` (`owner/name`), which
/// GitHub serves to nobody but a signed-in browser. One answer per
/// address, `None` where there is nothing to sign.
pub async fn github_asset_urls(repo: String, urls: Vec<String>) -> Result<Vec<Option<String>>, MoonError> {
	moon_core::github_assets::sign_asset_urls(&repo, &urls).await
}

/// Sort `request.items` into groups to review together (ADR 0119):
/// one model call, through the coder's inference. Errors when the
/// coder isn't signed in or the model's answer can't be read.
pub async fn inbox_group(app: &AppCore, request: InboxGroupRequest) -> Result<InboxGrouping, MoonError> {
	app.coder.group_pull_requests(&request).await.map_err(MoonError::from)
}

pub async fn inbox_mark_read(thread_id: String) -> Result<(), MoonError> {
	inbox::mark_read(&thread_id)
		.await
		.map_err(|err| MoonError::IoError(format!("mark read: {err:?}")))
}

/// Put a pull request's head branch somewhere it can be reviewed
/// without disturbing the project's own checkout: the worktree already
/// on that branch, the project itself when it is on it, or a fresh
/// worktree under `<project>/.worktrees/` (ADR 0029).
pub async fn inbox_open_pr(app: &AppCore, folder_path: String, branch: String) -> Result<InboxOpenedPr, MoonError> {
	let project = app
		.workspaces
		.folder_for_path(&folder_path)
		.await
		.ok_or_else(|| MoonError::NotFound(format!("folder {folder_path}")))?;
	if !matches!(project.folder.origin, FolderOrigin::UserPicked) {
		return Err(MoonError::invalid(format!("{folder_path} is not a project folder")));
	}

	app.workspaces.adopt_disk_worktrees().await;
	let existing = app.workspaces.folders().await.into_iter().find(|entry| {
		matches!(
			&entry.folder.origin,
			FolderOrigin::Worktree { parent_path, branch: b } if parent_path == &folder_path && b == &branch
		)
	});
	let opened_path = match existing {
		Some(entry) => entry.folder.path.clone(),
		None if project.host.git_branch().await?.name.as_deref() == Some(branch.as_str()) => folder_path,
		None => {
			// The branch may have been pushed since the last fetch.
			if let Err(err) = project.host.git_fetch().await {
				tracing::debug!(error = %err, "inbox_open_pr: fetch failed; trying the refs we have");
			}
			let worktree_path = moon_core::worktree::free_worktree_dir(
				&Utf8Path::new(&folder_path).join(moon_core::WORKTREES_DIR_NAME),
				&branch.replace('/', "-"),
			);
			if let Some(dir) = worktree_path.parent() {
				std::fs::create_dir_all(dir.as_std_path())?;
			}
			project
				.host
				.git_worktree_add(&worktree_path, WorktreeBranch::Existing(branch.clone()))
				.await?;
			let entry = app
				.workspaces
				.add_worktree_folder(worktree_path, folder_path, branch)
				.await?;
			entry.folder.path.clone()
		}
	};
	Ok(InboxOpenedPr {
		workspace: app.workspaces.snapshot().await,
		folder_path: opened_path,
	})
}

/// The open pull request for the active folder's branch (ADR 0113),
/// or `None` when there is nothing to show.
pub async fn pr_current(app: &AppCore) -> Result<Option<PullRequestDetail>, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	Ok(moon_core::pull_request::fetch_current(Utf8Path::new(&entry.folder.path)).await)
}

/// Pull request `number` of `folder_path`'s repository, without its
/// review threads or diff: the one fast call a PR tab paints from.
pub async fn pr_detail(app: &AppCore, folder_path: String, number: u32) -> Result<PullRequestDetail, MoonError> {
	let entry = crate::fs::folder_or_active(app, Some(folder_path)).await?;
	moon_core::pull_request::fetch_detail(Utf8Path::new(&entry.folder.path), Some(number)).await
}

/// Review threads of the pull request at `url`.
pub async fn pr_threads(url: String) -> Result<Vec<PrThread>, MoonError> {
	moon_core::pull_request::fetch_threads_at(&url).await
}

/// The diff of pull request `number` of `folder_path`'s repository,
/// without a checkout: worktrees share one object store, so any
/// folder of the repository will do.
pub async fn pr_diff(
	app: &AppCore,
	folder_path: String,
	number: u32,
	base_ref: String,
	head_sha: String,
) -> Result<PrDiff, MoonError> {
	let entry = crate::fs::folder_or_active(app, Some(folder_path)).await?;
	moon_core::pull_request::diff(entry.host.as_ref(), number, &base_ref, &head_sha).await
}

/// Change the state of the pull request at `url`: ready, draft, merge,
/// close or reopen (ADR 0120). Only ever called for a click.
pub async fn pr_act(url: String, action: PrAction) -> Result<(), MoonError> {
	moon_core::pull_request::act(&url, &action).await
}

/// Issue `number` of `folder_path`'s repository.
pub async fn issue_open(app: &AppCore, folder_path: String, number: u32) -> Result<IssueDetail, MoonError> {
	let entry = crate::fs::folder_or_active(app, Some(folder_path)).await?;
	moon_core::issue::fetch(Utf8Path::new(&entry.folder.path), number).await
}

crate::commands! {
	pr_current([app]),
	pr_detail([app] folder_path: String, number: u32),
	pr_threads(url: String),
	pr_diff([app] folder_path: String, number: u32, base_ref: String, head_sha: String),
	pr_act(url: String, action: PrAction),
	issue_open([app] folder_path: String, number: u32),
	inbox_fetch([app] watched_authors: Vec<String>),
	inbox_digests(subjects: Vec<InboxPrRef>),
	github_asset_urls(repo: String, urls: Vec<String>),
	inbox_group([app] request: InboxGroupRequest),
	inbox_mark_read(thread_id: String),
	inbox_open_pr([app] folder_path: String, branch: String),
}

#[cfg(test)]
mod tests {
	use super::*;

	fn git(cwd: &std::path::Path, args: &[&str]) {
		let out = std::process::Command::new("git")
			.arg("-C")
			.arg(cwd)
			.args(args)
			.output()
			.expect("git should run");
		assert!(
			out.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
	}

	#[tokio::test]
	async fn open_pr_reuses_the_checkout_that_has_the_branch() {
		let repo = tempfile::tempdir().unwrap();
		let root = repo.path().canonicalize().unwrap();
		git(&root, &["init", "-q", "-b", "main"]);
		git(&root, &["config", "user.email", "a@example.com"]);
		git(&root, &["config", "user.name", "A"]);
		std::fs::write(root.join("README.md"), "hi\n").unwrap();
		git(&root, &["add", "."]);
		git(&root, &["commit", "-q", "-m", "initial"]);
		git(&root, &["branch", "feature/login"]);
		let project = root.to_str().unwrap().to_owned();

		let (app, _data) = crate::tests::app_with_events(std::sync::Arc::new(NoEvents));
		app.workspaces.add_folder(project.clone().into()).await.unwrap();

		// A branch nobody has checked out gets its own worktree.
		let opened = inbox_open_pr(&app, project.clone(), "feature/login".into())
			.await
			.unwrap();
		assert!(opened
			.folder_path
			.starts_with(&format!("{project}/.worktrees/feature-login")));
		let bound = opened
			.workspace
			.folders
			.iter()
			.find(|folder| folder.path == opened.folder_path)
			.expect("the worktree is bound");
		assert_eq!(
			bound.origin,
			FolderOrigin::Worktree {
				parent_path: project.clone(),
				branch: "feature/login".into()
			}
		);
		assert_eq!(opened.workspace.active_folder.as_deref(), Some(project.as_str()));

		// Asking again returns that worktree instead of failing on
		// "already checked out".
		let again = inbox_open_pr(&app, project.clone(), "feature/login".into())
			.await
			.unwrap();
		assert_eq!(again.folder_path, opened.folder_path);
		assert_eq!(again.workspace.folders.len(), 2);

		// The project's own branch needs no worktree.
		let own = inbox_open_pr(&app, project.clone(), "main".into()).await.unwrap();
		assert_eq!(own.folder_path, project);
		assert_eq!(own.workspace.folders.len(), 2);

		// A worktree is not a project to branch from.
		assert!(inbox_open_pr(&app, opened.folder_path, "main".into()).await.is_err());
	}

	struct NoEvents;

	impl crate::EventSink for NoEvents {
		fn emit(&self, _event: &str, _payload: serde_json::Value) {}
	}
}
