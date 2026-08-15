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
use moon_protocol::{MoonError, MoonResult};

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

/// Liveness of a worktree checkout on disk. Host-side test — valid
/// under either shell target, since worktrees live under
/// `<parent>/.worktrees/<slug>` and ride the parent's bind mount
/// (ADR 0029).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutState {
	/// `<path>/.git` exists — a valid linked-worktree checkout.
	Live,
	/// Nothing meaningful left on disk: the directory is missing or
	/// empty. Safe to forget silently (ADR 0044).
	Gone,
	/// The directory still has files but no `.git` link — an
	/// out-of-band removal left ignored/untracked leftovers behind
	/// (`node_modules`, build output). git refuses `worktree remove`
	/// on these with "is not a working tree" whatever the flags, so
	/// discarding has to delete the leftovers itself (ADR 0068).
	StaleLeftovers,
}

/// Classify a worktree checkout's on-disk state. See [`CheckoutState`].
pub fn checkout_state(path: &Utf8Path) -> CheckoutState {
	if path.join(".git").exists() {
		return CheckoutState::Live;
	}
	let has_leftovers = std::fs::read_dir(path)
		.map(|mut entries| entries.next().is_some())
		.unwrap_or(false);
	if has_leftovers {
		return CheckoutState::StaleLeftovers;
	}
	CheckoutState::Gone
}

/// Discard a worktree checkout idempotently (ADR 0044 / ADR 0068).
/// Shared by the UI's discard command and the coordinator's
/// `discard_worker_worktree` tool so both sides agree on what
/// "gone" means:
///
/// - [`CheckoutState::Live`] → `git worktree remove [--force]`; a
///   genuine refusal (dirty tree without `force`) propagates so the
///   caller can re-confirm and force.
/// - [`CheckoutState::Gone`] → forget the stale git metadata
///   (best-effort — it would refuse a later `git worktree add` at
///   the same deterministic path, ADR 0042) and reap an empty husk
///   directory if one remains. Never errors.
/// - [`CheckoutState::StaleLeftovers`] → refused without `force`
///   (the leftovers may be files the user wants); with `force` the
///   metadata is forgotten and the leftover directory deleted.
///
/// `remove_path` is the path handed to git (the caller may have
/// translated it for a container target); `host_path` is the host
/// path used for the on-disk liveness test and leftover deletion.
pub async fn discard_checkout(
	host: &dyn crate::host::WorkspaceHost,
	host_path: &Utf8Path,
	remove_path: &Utf8Path,
	force: bool,
) -> MoonResult<()> {
	match checkout_state(host_path) {
		CheckoutState::Live => return host.git_worktree_remove(remove_path, force).await,
		CheckoutState::StaleLeftovers if !force => {
			return Err(MoonError::invalid(format!(
				"{host_path} is no longer a git worktree, but leftover files (ignored build output, node_modules, …) remain — discarding will delete them"
			)));
		}
		CheckoutState::StaleLeftovers | CheckoutState::Gone => {}
	}
	if let Err(err) = host.git_worktree_forget(remove_path).await {
		// Housekeeping, not a gate: the caller still unbinds the folder.
		tracing::warn!(error = %err, worktree = %host_path, "git worktree prune failed for an already-removed checkout");
	}
	// Delete what's left of the checkout — an empty husk directory,
	// or (force-gated above) the leftover files. Blocking fs work off
	// the async thread: node_modules leftovers can be large.
	let dir = host_path.to_owned();
	tokio::task::spawn_blocking(move || {
		if !dir.is_dir() {
			return Ok(());
		}
		std::fs::remove_dir_all(&dir)
			.map_err(|e| MoonError::IoError(format!("could not delete leftover files at {dir}: {e}")))
	})
	.await
	.map_err(|e| MoonError::Internal(format!("discard_checkout join error: {e}")))?
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

	#[test]
	fn checkout_state_classifies_live_gone_and_stale() {
		let dir = tempfile::TempDir::new().unwrap();
		let root = Utf8Path::from_path(dir.path()).unwrap();

		assert_eq!(checkout_state(&root.join("missing")), CheckoutState::Gone);

		let empty = root.join("empty");
		std::fs::create_dir(&empty).unwrap();
		assert_eq!(checkout_state(&empty), CheckoutState::Gone);

		let live = root.join("live");
		std::fs::create_dir(&live).unwrap();
		std::fs::write(live.join(".git"), "gitdir: ../.git/worktrees/live\n").unwrap();
		assert_eq!(checkout_state(&live), CheckoutState::Live);

		// git already forgot the checkout but ignored files survived —
		// the half-removed state ADR 0068 makes discardable.
		let stale = root.join("stale");
		std::fs::create_dir_all(stale.join("node_modules")).unwrap();
		assert_eq!(checkout_state(&stale), CheckoutState::StaleLeftovers);
	}
}
