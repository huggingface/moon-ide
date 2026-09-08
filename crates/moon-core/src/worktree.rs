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

/// Root every bound folder's bind mount lives under in the dev
/// container (`/workspace/<folder-basename>`).
const CONTAINER_MOUNT_ROOT: &str = "/workspace";

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
	Some(Utf8Path::new(CONTAINER_MOUNT_ROOT).join(parent_basename).join(tail))
}

/// Inverse of [`worktree_container_path`]: a **container-shaped**
/// absolute path (`/workspace/<parent-basename>/<tail>`) back to its
/// host equivalent under `parent_host`. Identity when `path` is
/// already host-shaped (anything not under `/workspace/<basename>`).
///
/// Needed when a process *inside* the dev container wrote an absolute
/// path git later reports to the host — concretely, an agent running
/// `git worktree add` from the `bash` tool without
/// `--relative-paths`, which burns `/workspace/…` into the worktree's
/// git links (see [`repair_absolute_worktree_links`]).
pub fn container_path_to_host(path: &Utf8Path, parent_host: &Utf8Path) -> Utf8PathBuf {
	let Some(basename) = parent_host.file_name() else {
		return path.to_path_buf();
	};
	let mount = Utf8Path::new(CONTAINER_MOUNT_ROOT).join(basename);
	match path.strip_prefix(&mount) {
		Ok(tail) => parent_host.join(tail),
		Err(_) => path.to_path_buf(),
	}
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

/// Rewrite a worktree's two git link files when they carry an
/// **absolute path under the dev-container mount** (`/workspace/…`) —
/// the signature of an agent running `git worktree add` from inside
/// the container without `--relative-paths` (the `bash` tool can't
/// intercept a raw git call, so the host-side creation path in
/// `host.rs` is bypassed). Both links are rewritten to the relative
/// form `--relative-paths` would have written, after which host git
/// resolves the worktree again:
///
/// - `<worktree>/.git` — the `gitdir:` pointer to the metadata dir
///   (`../../.git/worktrees/<name>`),
/// - `<parent>/.git/worktrees/<name>/gitdir` — the back-pointer to
///   the checkout (`../../../.worktrees/<name>/.git`).
///
/// Either link is rewritten only when it is absolute **and** points
/// inside the parent's container mount — a link naming anything else
/// (a real host path, a foreign mount) is left alone: it may be
/// valid, and rewriting a link we can't place would corrupt it.
/// Missing files (mid-removal) are skipped, matching the liveness
/// checks callers already do. Returns `true` when something was
/// repaired.
pub fn repair_absolute_worktree_links(worktree_host: &Utf8Path, parent_host: &Utf8Path) -> MoonResult<bool> {
	let mount_prefix = match parent_host.file_name() {
		Some(b) => format!("{CONTAINER_MOUNT_ROOT}/{b}"),
		None => return Ok(false),
	};
	let mut repaired = false;

	// Forward link: `<worktree>/.git` → metadata dir.
	let link = worktree_host.join(".git");
	if let Ok(content) = std::fs::read_to_string(&link) {
		if let Some(target) = content.trim().strip_prefix("gitdir:").map(str::trim) {
			if let Some(gitdir) = target.strip_prefix(mount_prefix.as_str()) {
				// `/workspace/<base>/.git/worktrees/<name>` → relative from
				// the checkout: up past the worktree name and `.worktrees/`.
				let rel = format!("gitdir: ../..{gitdir}\n");
				std::fs::write(&link, rel)?;
				repaired = true;
			}
		}
	}

	// Back link: `<parent>/.git/worktrees/<name>/gitdir` → checkout's
	// `.git` file. Resolve the (possibly already-repaired) forward
	// link to find the metadata dir rather than re-deriving the name.
	let link = worktree_host.join(".git");
	let Ok(content) = std::fs::read_to_string(&link) else {
		return Ok(repaired);
	};
	let Some(target) = content.trim().strip_prefix("gitdir:").map(str::trim) else {
		return Ok(repaired);
	};
	let metadata_dir = if Utf8Path::new(target).is_absolute() {
		Utf8PathBuf::from(target)
	} else {
		let Some(abs) = path_absolutize(worktree_host, target) else {
			return Ok(repaired);
		};
		abs
	};
	if let Ok(back) = std::fs::read_to_string(metadata_dir.join("gitdir")) {
		let back_target = back.trim();
		if let Some(checkout) = back_target.strip_prefix(mount_prefix.as_str()) {
			// `/workspace/<base>/.worktrees/<name>/.git` → relative from the
			// metadata dir: up past `<name>`, `worktrees/`, `.git/`.
			let rel = format!("../../..{checkout}\n");
			std::fs::write(metadata_dir.join("gitdir"), rel)?;
			repaired = true;
		}
	}
	Ok(repaired)
}

/// Join a relative path onto `base`, collapsing `..` / `.`
/// components textually. The git links git writes are pure relative
/// (`../..` chains), but after our rewrite the forward link is read
/// back to locate the metadata dir — resolve it without hitting the
/// filesystem (the targets may not exist mid-repair).
fn path_absolutize(base: &Utf8Path, rel: &str) -> Option<Utf8PathBuf> {
	let mut out = base.to_path_buf();
	for comp in Utf8Path::new(rel).components() {
		match comp {
			camino::Utf8Component::CurDir => {}
			camino::Utf8Component::ParentDir => {
				if !out.pop() {
					return None;
				}
			}
			camino::Utf8Component::Normal(s) => out.push(s),
			// Root / prefix inside a `gitdir:` target means it was
			// absolute all along — caller handles that case.
			_ => return None,
		}
	}
	Some(out)
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

	#[test]
	fn container_path_round_trips_through_host() {
		let parent = Utf8Path::new("/home/me/code/moon-landing");
		let host = Utf8Path::new("/home/me/code/moon-landing/.worktrees/agent-1");
		let container = worktree_container_path(parent, host).unwrap();
		assert_eq!(container, Utf8Path::new("/workspace/moon-landing/.worktrees/agent-1"));
		assert_eq!(container_path_to_host(&container, parent), host);
		// Already-host paths are identity (the repair runs on every
		// adoption candidate, healthy ones included).
		assert_eq!(container_path_to_host(host, parent), host);
		// A `/workspace/<other>` path is not this parent's mount.
		assert_eq!(
			container_path_to_host(Utf8Path::new("/workspace/other/.worktrees/x"), parent),
			Utf8PathBuf::from("/workspace/other/.worktrees/x")
		);
	}

	/// Build the on-disk shape a container-side `git worktree add`
	/// without `--relative-paths` produces: checkout `.git` file and
	/// metadata `gitdir` both pointing at absolute `/workspace/…`
	/// paths.
	fn write_absolute_container_links(root: &Utf8Path, parent_base: &str, name: &str) -> Utf8PathBuf {
		let wt = root.join(".worktrees").join(name);
		std::fs::create_dir_all(&wt).unwrap();
		let metadata = root.join(".git").join("worktrees").join(name);
		std::fs::create_dir_all(&metadata).unwrap();
		std::fs::write(
			wt.join(".git"),
			format!("gitdir: /workspace/{parent_base}/.git/worktrees/{name}\n"),
		)
		.unwrap();
		std::fs::write(
			metadata.join("gitdir"),
			format!("/workspace/{parent_base}/.worktrees/{name}/.git\n"),
		)
		.unwrap();
		std::fs::write(metadata.join("commondir"), "../..\n").unwrap();
		wt
	}

	#[test]
	fn repair_rewrites_absolute_container_links_to_relative() {
		let dir = tempfile::TempDir::new().unwrap();
		// The container mount is keyed on the parent folder's
		// basename (`/workspace/<basename>`), so name the root to
		// match the fixture links.
		let root = Utf8Path::from_path(dir.path()).unwrap().join("moon-landing");
		std::fs::create_dir_all(&root).unwrap();
		let wt = write_absolute_container_links(&root, "moon-landing", "agent-x");

		let repaired = repair_absolute_worktree_links(&wt, &root).unwrap();
		assert!(repaired);

		assert_eq!(
			std::fs::read_to_string(wt.join(".git")).unwrap(),
			"gitdir: ../../.git/worktrees/agent-x\n"
		);
		assert_eq!(
			std::fs::read_to_string(root.join(".git/worktrees/agent-x/gitdir")).unwrap(),
			"../../../.worktrees/agent-x/.git\n"
		);
		// A second pass is a no-op.
		assert!(!repair_absolute_worktree_links(&wt, &root).unwrap());
	}

	#[test]
	fn repair_leaves_foreign_or_relative_links_alone() {
		let dir = tempfile::TempDir::new().unwrap();
		let root = Utf8Path::from_path(dir.path()).unwrap();

		// Absolute but outside this parent's container mount — a real
		// host path or a foreign mount; not ours to rewrite.
		let foreign = root.join(".worktrees").join("foreign");
		std::fs::create_dir_all(&foreign).unwrap();
		std::fs::write(foreign.join(".git"), "gitdir: /elsewhere/.git/worktrees/foreign\n").unwrap();
		assert!(!repair_absolute_worktree_links(&foreign, root).unwrap());
		assert_eq!(
			std::fs::read_to_string(foreign.join(".git")).unwrap(),
			"gitdir: /elsewhere/.git/worktrees/foreign\n"
		);

		// Healthy relative links (what `--relative-paths` writes).
		let ok = root.join(".worktrees").join("ok");
		std::fs::create_dir_all(&ok).unwrap();
		std::fs::write(ok.join(".git"), "gitdir: ../../.git/worktrees/ok\n").unwrap();
		let metadata = root.join(".git").join("worktrees").join("ok");
		std::fs::create_dir_all(&metadata).unwrap();
		std::fs::write(metadata.join("gitdir"), "../../../.worktrees/ok/.git\n").unwrap();
		assert!(!repair_absolute_worktree_links(&ok, root).unwrap());
	}
}
