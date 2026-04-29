//! Per-folder compose project lifecycle.
//!
//! A bound folder's own `docker-compose.yml` runs as a **separate
//! compose project** from the workspace shell — `dev` (managed
//! by [`crate::lifecycle::Workspace`]) lives at
//! `moon-ws-<id>`, while a folder's services live at
//! `moon-ws-<id>-<folder-slug>`. Both projects share the
//! `moon-ws-<id>-` prefix so a single
//! `docker compose ls --filter name=moon-ws-default-` enumerates
//! everything the workspace owns.
//!
//! Why split them
//! --------------
//!
//! Pre-redesign moon-ide stitched everything into one project via
//! `include:`. That meant a broken project service (e.g. a
//! permission-denied `gitaly` volume) blocked the whole
//! `compose up --wait`, leaving the IDE stuck "setting up…"
//! while the user couldn't even open a terminal in the workspace.
//!
//! The split decouples two concerns the user thinks about
//! separately:
//!
//! - The **workspace shell**: "is the IDE's container alive so I
//!   can run a terminal / LSP / agent?". One per workspace,
//!   managed by moon-ide, expected to be up almost always.
//! - **Project services**: "is moon-landing's stack up so I can
//!   run e2e tests?". Per folder, started/stopped on demand by
//!   the user from the folder bar.
//!
//! Path strategy
//! -------------
//!
//! The compose file we shell out to is the **user's** file
//! (`<folder>/docker-compose.yml`), unmodified. We don't generate
//! a wrapper. `docker compose -f <user's file> -p
//! moon-ws-<id>-<slug>` lets us namespace the project on the
//! daemon without touching anything in the user's repo. Relative
//! paths inside the user's compose still resolve from the file's
//! directory — exactly what the user wired up.
//!
//! What's not covered yet
//! ----------------------
//!
//! - Cross-project networking. `dev` and a folder's services run
//!   on separate compose networks; the user reaches them via
//!   `host.docker.internal:<port>` if they expose host ports, or
//!   wires up an external network manually. Phase 2.2 will
//!   formalise the routing.
//! - Folder-local *override* files (`compose.override.yaml`,
//!   etc.). The handle uses whichever single file
//!   [`crate::discovery::discover_root_compose`] picked.

use std::ffi::OsStr;

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::container::{ContainerState, ContainerStatus};

use crate::discovery::{discover_root_compose, DiscoveredCompose};
use crate::lifecycle::{aggregate_state, parse_ps_output, run_docker_compose, LifecycleError};
use crate::project::{folder_slug, project_name_for_folder, ProjectName};

/// Handle for a single bound folder's compose project.
///
/// Cheap to construct (no I/O beyond the directory listing
/// `discover_root_compose` does). One handle per folder, rebuilt
/// on every Tauri command — there's no long-lived state to
/// preserve, the project name is derived deterministically.
#[derive(Debug, Clone)]
pub struct ProjectCompose {
	folder_root: Utf8PathBuf,
	compose_file: Utf8PathBuf,
	relative_path: Utf8PathBuf,
	project: ProjectName,
}

impl ProjectCompose {
	/// Resolve a [`ProjectCompose`] for `folder_root` under the
	/// given workspace ID.
	///
	/// Returns `Ok(None)` if the folder has no compose file at its
	/// root — that's the common case for folders the user opens
	/// just to edit code, not to run services. Returns
	/// `Err(InvalidWorkspaceId)` if the workspace ID would produce
	/// a malformed compose project name (defensive; the workspace
	/// ID has already been validated by the workspace shell
	/// constructor by the time we reach here).
	pub fn for_folder(workspace_id: &str, folder_root: &Utf8Path) -> Result<Option<Self>, LifecycleError> {
		let Some(found) = discover_root_compose(folder_root) else {
			return Ok(None);
		};
		Self::with_compose_file(workspace_id, folder_root, found).map(Some)
	}

	fn with_compose_file(
		workspace_id: &str,
		folder_root: &Utf8Path,
		found: DiscoveredCompose,
	) -> Result<Self, LifecycleError> {
		let basename = folder_root.file_name().unwrap_or("folder");
		let project = project_name_for_folder(workspace_id, basename)?;
		Ok(Self {
			folder_root: folder_root.to_path_buf(),
			compose_file: found.absolute_path,
			relative_path: found.relative_path,
			project,
		})
	}

	pub fn folder_root(&self) -> &Utf8Path {
		&self.folder_root
	}

	pub fn compose_file(&self) -> &Utf8Path {
		&self.compose_file
	}

	pub fn relative_path(&self) -> &Utf8Path {
		&self.relative_path
	}

	pub fn project(&self) -> &ProjectName {
		&self.project
	}

	/// Snapshot the per-folder compose project's state.
	///
	/// Mirrors [`crate::lifecycle::Workspace::status`]: same JSON
	/// parser, same aggregation rules. `Absent` is what we return
	/// when the project simply hasn't been brought up yet — the
	/// daemon has nothing for `-p moon-ws-default-<slug>` and
	/// reports an empty container list.
	pub async fn status(&self) -> Result<ContainerStatus, LifecycleError> {
		let output = self.docker_compose(["ps", "--all", "--format", "json"]).await?;
		let services = parse_ps_output(&output.stdout).map_err(LifecycleError::ParseError)?;
		let state = aggregate_state(&services);
		Ok(ContainerStatus { state, services })
	}

	/// `docker compose up -d --wait` — start all of the folder's
	/// services and block until each is healthy (or has failed).
	///
	/// Used by the folder-bar "Start services" affordance. The
	/// IDE never auto-invokes this; it's always a user click.
	pub async fn up(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["up", "-d", "--wait"]).await?;
		Ok(())
	}

	pub async fn pause(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["pause"]).await?;
		Ok(())
	}

	pub async fn resume(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["unpause"]).await?;
		Ok(())
	}

	/// Hammer: `up -d --force-recreate --pull always --wait`.
	pub async fn rebuild(&self) -> Result<(), LifecycleError> {
		self
			.docker_compose(["up", "-d", "--force-recreate", "--pull", "always", "--wait"])
			.await?;
		Ok(())
	}

	/// `docker compose down` — stop and remove containers,
	/// networks, and the project entry. The user's compose file
	/// stays put on disk; this is purely a daemon-side teardown.
	pub async fn down(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["down"]).await?;
		Ok(())
	}

	async fn docker_compose<I, S>(&self, args: I) -> Result<crate::lifecycle::DockerOutput, LifecycleError>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		run_docker_compose(&self.compose_file, &self.project, args).await
	}
}

/// Slug a folder basename into the suffix used for its compose
/// project name.
///
/// Re-exported here so the IPC layer can derive the slug without
/// pulling in [`crate::project`] directly — keeps the surface
/// area "everything per-folder lives in one module" tight.
pub fn slug_for_folder_basename(basename: &str) -> String {
	folder_slug(basename)
}

/// Convenience snapshot the IPC layer hands to the frontend.
///
/// Distinct from [`ContainerStatus`] because the frontend wants to
/// know whether a folder *has* a compose file at all (so it can
/// show or hide the indicator on the folder bar) without first
/// asking "what's its status?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectComposeSnapshot {
	/// Bound folder this snapshot belongs to (absolute path).
	pub folder_path: Utf8PathBuf,
	/// Path to the user-owned compose file, or `None` if the
	/// folder has none — in which case the rest of the snapshot
	/// is meaningless and the UI should hide the indicator.
	pub compose_file: Option<Utf8PathBuf>,
	/// Compose project name on the daemon
	/// (`moon-ws-<id>-<slug>`). `None` mirrors `compose_file`.
	pub project_name: Option<String>,
	/// Whatever `docker compose ps` reported for that project.
	/// `Absent` for "compose file present but never brought up".
	pub status: ContainerStatus,
}

impl ProjectComposeSnapshot {
	pub fn absent_for(folder: &Utf8Path) -> Self {
		Self {
			folder_path: folder.to_path_buf(),
			compose_file: None,
			project_name: None,
			status: ContainerStatus {
				state: ContainerState::Absent,
				services: Vec::new(),
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use std::fs;

	use camino::Utf8PathBuf;
	use tempfile::tempdir;

	use super::*;

	fn touch(path: &Utf8Path) {
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).unwrap();
		}
		fs::write(path, b"# placeholder\n").unwrap();
	}

	#[test]
	fn for_folder_returns_none_without_compose() {
		let tmp = tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		assert!(ProjectCompose::for_folder("default", &root).unwrap().is_none());
	}

	#[test]
	fn for_folder_finds_root_compose_and_derives_project_name() {
		let tmp = tempdir().unwrap();
		let parent = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let root = parent.join("moon-landing");
		touch(&root.join("docker-compose.yml"));

		let pc = ProjectCompose::for_folder("default", &root).unwrap().unwrap();
		assert_eq!(pc.project().as_str(), "moon-ws-default-moon-landing");
		assert_eq!(pc.compose_file(), root.join("docker-compose.yml"));
		assert_eq!(pc.relative_path(), Utf8Path::new("docker-compose.yml"));
	}

	#[test]
	fn for_folder_slugifies_messy_basename() {
		let tmp = tempdir().unwrap();
		let parent = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let root = parent.join("My Stuff");
		touch(&root.join("compose.yaml"));

		let pc = ProjectCompose::for_folder("default", &root).unwrap().unwrap();
		assert_eq!(pc.project().as_str(), "moon-ws-default-my-stuff");
	}

	#[test]
	fn for_folder_propagates_invalid_workspace_id() {
		let tmp = tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		touch(&root.join("docker-compose.yml"));

		let err = ProjectCompose::for_folder("Bad ID", &root).unwrap_err();
		assert!(matches!(err, LifecycleError::InvalidWorkspaceId(_)));
	}

	#[test]
	fn slug_helper_re_export_matches_internal_helper() {
		assert_eq!(slug_for_folder_basename("Moon Landing"), "moon-landing");
	}
}
