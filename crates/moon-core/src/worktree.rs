//! Worktree-backed coder sessions (ADR 0028): path mapping for the
//! shared in-container worktrees mount.
//!
//! A worktree-backed session checks its branch out into a directory
//! under the per-workspace state dir at
//! `<state_dir>/worktrees/<parent-slug>/<branch-slug>`. That tree is
//! bind-mounted into the dev container **once**, at
//! [`WORKTREE_CONTAINER_ROOT`], so new worktrees appear inside the
//! running container without recreating it (docker can't hot-add a
//! mount). This module is the single source of truth for the
//! host↔container path mapping every layer needs — the shell
//! resolver (git + format-on-save routing), the coder's `bash` cwd,
//! and the create / repair / discard orchestration.
//!
//! See [`specs/coder.md` § Worktree sessions](../../../specs/coder.md).

use camino::{Utf8Path, Utf8PathBuf};

/// In-container mount point for the per-workspace worktrees tree —
/// re-exported from `moon-protocol` (the single source of truth
/// shared with `moon-container`'s compose generation). A host
/// worktree at `<state_dir>/worktrees/<rel>` is visible at
/// `/workspace/.worktrees/<rel>` inside the container. The leading
/// dot keeps it clear of real bound folders (which mount at
/// `/workspace/<name>`).
pub use moon_protocol::container::WORKTREE_CONTAINER_ROOT;

/// Host-side root holding every worktree for one workspace:
/// `<state_dir>/worktrees`. `state_dir` is
/// `<workspaces_dir>/<workspace_id>`.
pub fn worktrees_host_root(state_dir: &Utf8Path) -> Utf8PathBuf {
	state_dir.join("worktrees")
}

/// Map a worktree's absolute **host** path to its in-container path
/// under [`WORKTREE_CONTAINER_ROOT`]. Returns `None` when `host_path`
/// isn't under the worktrees root (i.e. it isn't an IDE worktree) —
/// callers fall back to host execution in that case.
pub fn worktree_container_path(state_dir: &Utf8Path, host_path: &Utf8Path) -> Option<Utf8PathBuf> {
	let rel = host_path.strip_prefix(worktrees_host_root(state_dir)).ok()?;
	if rel.as_str().is_empty() {
		return Some(Utf8PathBuf::from(WORKTREE_CONTAINER_ROOT));
	}
	Some(Utf8Path::new(WORKTREE_CONTAINER_ROOT).join(rel))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_worktree_host_path_under_root() {
		let state = Utf8Path::new("/data/moon-ide/workspaces/default");
		let host = Utf8Path::new("/data/moon-ide/workspaces/default/worktrees/repo-ab12/moon-agent-1");
		assert_eq!(
			worktree_container_path(state, host).as_deref().map(Utf8Path::as_str),
			Some("/workspace/.worktrees/repo-ab12/moon-agent-1")
		);
	}

	#[test]
	fn rejects_paths_outside_the_worktrees_root() {
		let state = Utf8Path::new("/data/moon-ide/workspaces/default");
		assert_eq!(
			worktree_container_path(state, Utf8Path::new("/home/me/code/repo")),
			None
		);
	}

	#[test]
	fn maps_the_root_itself() {
		let state = Utf8Path::new("/data/ws/default");
		let host = Utf8Path::new("/data/ws/default/worktrees");
		assert_eq!(
			worktree_container_path(state, host).as_deref().map(Utf8Path::as_str),
			Some("/workspace/.worktrees")
		);
	}
}
