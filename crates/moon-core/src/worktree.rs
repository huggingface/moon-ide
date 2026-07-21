//! Worktree-backed coder sessions (ADR 0029): host↔container path
//! mapping.
//!
//! A worktree-backed session checks its branch out into a directory
//! **inside the parent repo** at `<parent>/.worktrees/<branch-slug>`,
//! with `--relative-paths` git links (git >= 2.48). Because it rides
//! inside the parent repo's existing bind mount, the same checkout is
//! reachable inside the dev container at the parent's container mount
//! plus the same relative tail — no separate mount, no `git worktree
//! repair`, and host git keeps working when the container is down.
//! This module maps a worktree's host path to its in-container path.
//!
//! See [`specs/coder.md` § Worktree sessions](../../../specs/coder.md).

use camino::{Utf8Path, Utf8PathBuf};

/// Directory name, under the parent repo, that holds its worktrees.
/// Added to the parent's `.git/info/exclude` so it never shows up in
/// the parent's `git status`.
pub const WORKTREES_DIR_NAME: &str = ".worktrees";

/// Map a worktree's absolute **host** path to its in-container path.
/// The worktree lives at `<parent>/.worktrees/<rel>`; the parent repo
/// is bind-mounted at `/workspace/<parent-basename>`, so the worktree
/// is at `/workspace/<parent-basename>/<tail>` where `<tail>` is the
/// worktree's path relative to the parent. Returns `None` when
/// `worktree_host` isn't under `parent_host` (caller falls back to
/// host execution) or the parent has no basename.
pub fn worktree_container_path(parent_host: &Utf8Path, worktree_host: &Utf8Path) -> Option<Utf8PathBuf> {
	let tail = worktree_host.strip_prefix(parent_host).ok()?;
	let parent_basename = parent_host.file_name()?;
	Some(Utf8Path::new("/workspace").join(parent_basename).join(tail))
}

/// The host path whose bind mount a folder rides in the dev
/// container: a worktree folder rides its **parent's** mount
/// (ADR 0029), everything else rides its own. This is the path to
/// check against the container's mounted-folder set when deciding
/// host-vs-container routing for a folder's subprocesses.
pub fn effective_mount_root(folder: &moon_protocol::workspace::WorkspaceFolder) -> &str {
	match &folder.origin {
		moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } => parent_path,
		_ => &folder.path,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_worktree_under_parent_mount() {
		let parent = Utf8Path::new("/home/me/code/moon-landing");
		let wt = Utf8Path::new("/home/me/code/moon-landing/.worktrees/moon-agent-1");
		assert_eq!(
			worktree_container_path(parent, wt).as_deref().map(Utf8Path::as_str),
			Some("/workspace/moon-landing/.worktrees/moon-agent-1")
		);
	}

	#[test]
	fn rejects_paths_outside_the_parent() {
		let parent = Utf8Path::new("/home/me/code/moon-landing");
		assert_eq!(
			worktree_container_path(parent, Utf8Path::new("/home/me/code/other/.worktrees/x")),
			None
		);
	}
}
