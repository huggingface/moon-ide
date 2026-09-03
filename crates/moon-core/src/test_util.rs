//! Shared helpers for tests that need a real git repo (and
//! IDE-managed worktrees under it). Used by `host.rs`'s test
//! module and `workspace.rs`'s adoption tests.
//!
//! Everything here mirrors how moon-ide itself creates worktrees
//! (`git worktree add --relative-paths` + lock, ADR 0028/0029), so
//! the states the adoption sweep has to recognise are the states
//! these helpers produce.

/// `git` on PATH if runnable, else `None` (callers skip the test).
pub fn which_git() -> Option<std::path::PathBuf> {
	std::process::Command::new("git")
		.arg("--version")
		.output()
		.ok()
		.filter(|o| o.status.success())
		.map(|_| std::path::PathBuf::from("git"))
}

/// True when the installed git supports `--relative-paths`
/// worktree links (>= 2.48) — the gate for every real-worktree
/// test, matching the runtime gate in `host.rs`.
pub fn relative_worktrees_supported() -> bool {
	crate::host::git_major_minor(&crate::shell::ShellTarget::Host, camino::Utf8Path::new("."))
		.is_some_and(|v| v >= crate::host::MIN_GIT_FOR_RELATIVE_WORKTREES)
}

/// Run `git <args>` in `cwd` for test setup; panics on failure so
/// a broken fixture fails loudly at the setup line, not deep in the
/// assertion. Scrubs the ambient committer identity so CI's
/// exported `GIT_AUTHOR_NAME` etc. can't skew results.
pub fn run_git(git: &std::path::Path, cwd: &std::path::Path, args: &[&str]) {
	let out = std::process::Command::new(git)
		.arg("-C")
		.arg(cwd)
		.args(args)
		.env_remove("GIT_AUTHOR_NAME")
		.env_remove("GIT_AUTHOR_EMAIL")
		.env_remove("GIT_AUTHOR_DATE")
		.env_remove("GIT_COMMITTER_NAME")
		.env_remove("GIT_COMMITTER_EMAIL")
		.env_remove("GIT_COMMITTER_DATE")
		.output()
		.expect("git should run");
	assert!(
		out.status.success(),
		"git {:?} failed: {}",
		args,
		String::from_utf8_lossy(&out.stderr)
	);
}

/// A committed repo at `dir` with one commit on `main` — the
/// minimal parent project. Configures a local identity so commits
/// work in sandboxes without global git config.
pub fn init_committed_repo(git: &std::path::Path, dir: &std::path::Path) {
	run_git(git, dir, &["init", "-q", "-b", "main"]);
	run_git(git, dir, &["config", "user.email", "a@example.com"]);
	run_git(git, dir, &["config", "user.name", "A"]);
	std::fs::write(dir.join("README.md"), "hi\n").unwrap();
	run_git(git, dir, &["add", "."]);
	run_git(git, dir, &["commit", "-q", "-m", "initial"]);
}
