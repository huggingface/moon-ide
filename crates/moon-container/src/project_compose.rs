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
//! Cross-project networking
//! ------------------------
//!
//! `dev` and a folder's services start as separate compose
//! projects on separate networks, but [`Self::up`] /
//! [`Self::rebuild`] / [`Self::start_service`] /
//! [`Self::restart_service`] follow up with a `docker network
//! connect <project>_default <dev-container>` (best-effort, even
//! when the lifecycle command itself errored on an unhealthy
//! service) so the workspace shell can reach project services by
//! compose service name —
//! `mongosh mongodb://mongo:27017`, `psql -h db -U postgres`,
//! `curl http://api:3000/health`. [`Self::down`] disconnects
//! before tearing the network down. See [`crate::network`] for
//! the helper, idempotency rules, and the limitation around
//! projects that override the default network.
//!
//! The same lifecycle actions also repair containers whose
//! network endpoints were wiped by a failed start — see
//! [`Self::heal_networkless_services`].
//!
//! What's not covered yet
//! ----------------------
//!
//! - Folder-local *override* files (`compose.override.yaml`,
//!   etc.). The handle uses whichever single file
//!   [`crate::discovery::discover_root_compose`] picked.

use std::ffi::OsStr;
use std::time::Instant;

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::container::{ContainerState, ContainerStatus};

use crate::discovery::{discover_root_compose, DiscoveredCompose};
use crate::lifecycle::{
	aggregate_state, flag_networkless_services, parse_ps_output, run_docker_compose_with_overrides, LifecycleError,
};
use crate::network::{
	connect_container_to_network, dev_container_name, disconnect_container_from_network, networkless_running_services,
	project_default_network,
};
use crate::project::{folder_slug, project_name_for_folder, project_name_for_id, ProjectName};
use crate::restart_override::ensure_restart_override;
use crate::status_cache;

/// Handle for a single bound folder's compose project.
///
/// Cheap to construct (no I/O beyond the directory listing
/// `discover_root_compose` does). One handle per folder, rebuilt
/// on every Tauri command — there's no long-lived state to
/// preserve, the project name is derived deterministically.
///
/// The `workspace_project` carried on the handle is the parent
/// workspace shell's project name (`moon-ws-<id>`); we need it to
/// resolve the dev container we attach to this project's network
/// for service-name DNS to work — see [`crate::network`].
#[derive(Debug, Clone)]
pub struct ProjectCompose {
	folder_root: Utf8PathBuf,
	compose_file: Utf8PathBuf,
	relative_path: Utf8PathBuf,
	project: ProjectName,
	workspace_project: ProjectName,
	/// Workspace state dir (`<workspaces_dir>/<id>/`). The
	/// restart-policy override file we layer over the user's
	/// compose lives at `<state_dir>/project-overrides/<slug>.yaml`
	/// — see [`crate::restart_override`].
	state_dir: Utf8PathBuf,
	/// Folder-slug component of the compose project name. Cached
	/// here so the override path resolver doesn't have to re-parse
	/// the [`ProjectName`].
	slug: String,
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
	///
	/// `state_dir` is the workspace's state directory
	/// (`<workspaces_dir>/<id>/`); the restart-policy override
	/// file we layer over the user's compose lives under there.
	pub fn for_folder(
		workspace_id: &str,
		state_dir: &Utf8Path,
		folder_root: &Utf8Path,
	) -> Result<Option<Self>, LifecycleError> {
		let Some(found) = discover_root_compose(folder_root) else {
			return Ok(None);
		};
		Self::with_compose_file(workspace_id, state_dir, folder_root, found).map(Some)
	}

	fn with_compose_file(
		workspace_id: &str,
		state_dir: &Utf8Path,
		folder_root: &Utf8Path,
		found: DiscoveredCompose,
	) -> Result<Self, LifecycleError> {
		let basename = folder_root.file_name().unwrap_or("folder");
		let project = project_name_for_folder(workspace_id, basename)?;
		let workspace_project = project_name_for_id(workspace_id)?;
		let slug = folder_slug(basename);
		Ok(Self {
			folder_root: folder_root.to_path_buf(),
			compose_file: found.absolute_path,
			relative_path: found.relative_path,
			project,
			workspace_project,
			state_dir: state_dir.to_path_buf(),
			slug,
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
	///
	/// Reads are TTL-cached (see [`crate::status_cache`]). Every
	/// mutating method below invalidates the cache on success.
	pub async fn status(&self) -> Result<ContainerStatus, LifecycleError> {
		status_cache::get_or_fetch(&self.project, &self.compose_file, Instant::now(), || async {
			let output = self.docker_compose(["ps", "--all", "--format", "json"]).await?;
			let mut services = parse_ps_output(&output.stdout).map_err(LifecycleError::ParseError)?;
			flag_networkless_services(&self.project, &mut services).await;
			let state = aggregate_state(&services);
			Ok(ContainerStatus { state, services })
		})
		.await
	}

	/// Drop the cached `status()` reading so the next call
	/// re-probes `docker compose ps`. Called by every mutating
	/// method after `docker compose` succeeds.
	async fn invalidate_status_cache(&self) {
		status_cache::invalidate(&self.project, &self.compose_file).await;
	}

	/// `docker compose up -d --wait` — start all of the folder's
	/// services and block until each is healthy (or has failed).
	///
	/// Used by the folder-bar "Start services" affordance. The
	/// IDE never auto-invokes this; it's always a user click.
	///
	/// Attaches the workspace's `dev` container to this project's
	/// default network (best effort) so a container terminal can
	/// reach the project's services by compose service name
	/// (`mongosh mongodb://mongo:27017`, `psql -h db`). See
	/// [`crate::network`].
	///
	/// The attach runs even when `up --wait` returns an error.
	/// `--wait` fails the whole command if *any* service is
	/// unhealthy, but the project network and the services that
	/// did come up are real — and the dev-side attach is
	/// idempotent and harmless against a half-up project. Gating
	/// it behind whole-project health would leave the dev
	/// container un-attached whenever a single unrelated service
	/// flakes, so we wire the dev side regardless and re-surface
	/// the original `up` error.
	pub async fn up(&self) -> Result<(), LifecycleError> {
		let result = self.docker_compose(["up", "-d", "--wait"]).await;
		self.invalidate_status_cache().await;
		let healed = self.heal_networkless_services().await;
		self.attach_workspace_dev().await;
		result.map(|_| ()).and(healed)
	}

	pub async fn pause(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["pause"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	pub async fn resume(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["unpause"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// Hammer: `up -d --force-recreate --pull always --wait`.
	///
	/// Re-attaches the workspace dev container after the recreate
	/// — `--force-recreate` tears down the project network too, so
	/// any pre-existing attachment is gone by the time we get
	/// here.
	pub async fn rebuild(&self) -> Result<(), LifecycleError> {
		let result = self
			.docker_compose(["up", "-d", "--force-recreate", "--pull", "always", "--wait"])
			.await;
		self.invalidate_status_cache().await;
		self.attach_workspace_dev().await;
		result.map(|_| ())
	}

	/// `docker compose stop` — SIGTERM all of the folder's
	/// service containers but **leave the records on the
	/// daemon**. Cheaper to undo than [`Self::down`]: a follow-up
	/// `up` resumes from the same containers without rebuilding
	/// or re-pulling images. This is the right "I'm done for
	/// now, I'll come back to this project soon" knob.
	///
	/// We deliberately leave the dev container's attachment to
	/// the project network in place: the network survives `stop`,
	/// and a follow-up [`Self::up`] / [`Self::start_service`]
	/// re-uses the same network, so the attachment is still valid.
	/// Detaching here would force an extra `connect` round-trip on
	/// every start/stop cycle for no benefit.
	pub async fn stop(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["stop"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// `docker compose down` — stop and remove containers,
	/// networks, and the project entry. Named volumes (and any
	/// host bind mounts) are *preserved*: this isn't a data
	/// nuke, just a daemon-side teardown of the runtime
	/// resources. The user's compose file stays put on disk.
	///
	/// Detaches the workspace dev container from the project
	/// network *before* `down` runs — Docker refuses to remove
	/// a network with active endpoints, and our dev container
	/// is exactly that. Detach failures are tolerated (network
	/// might already be gone from a partially-failed prior down)
	/// so the cleanup remains best-effort.
	pub async fn down(&self) -> Result<(), LifecycleError> {
		self.detach_workspace_dev().await;
		self.docker_compose(["down"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// `docker compose up -d --no-deps <service>` — bring a single
	/// service to `running`, creating and network-joining its
	/// container if needed.
	///
	/// Used by the per-service "▶" affordance in the popover. We
	/// deliberately use `up` rather than bare `start`: `start`
	/// only handles the pure `exited`/`stopped` → `running`
	/// transition and assumes the container is already correctly
	/// joined to the project network. A container left in
	/// `created` by a partially-failed `up` (e.g. a host-port
	/// conflict aborted the project before the network was fully
	/// established) is exactly the case where `start` runs the
	/// container but leaves it un-resolvable — `up` (re)creates
	/// and (re)joins it idempotently, which is what the user
	/// wants when recovering from such a failure. `--no-deps`
	/// keeps the click scoped to the one service the user asked
	/// for instead of dragging its `depends_on` graph up.
	///
	/// Re-attaches the workspace dev container after the start —
	/// the project network survives across `stop`/`start`, but a
	/// fresh workspace shell created since the last `up` won't be
	/// on it yet. The attach runs even on the `up` error path
	/// (see [`Self::up`] for why). Idempotent on the
	/// already-attached path.
	pub async fn start_service(&self, service: &str) -> Result<(), LifecycleError> {
		let result = self.docker_compose(["up", "-d", "--no-deps", service]).await;
		self.invalidate_status_cache().await;
		let healed = self.heal_networkless_services().await;
		self.attach_workspace_dev().await;
		result.map(|_| ()).and(healed)
	}

	/// `docker compose stop <service>` — send SIGTERM to a single
	/// service's container, leaving the container record so the
	/// user can `start_service` it again without losing state.
	pub async fn stop_service(&self, service: &str) -> Result<(), LifecycleError> {
		self.docker_compose(["stop", service]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// `docker compose restart <service>` — stop + start one
	/// service's container, preserving its image and volumes.
	/// This is the cheap "did the config flake out, try again"
	/// affordance. Use [`Self::rebuild`] for the heavier
	/// "recreate from a fresh image" workflow.
	pub async fn restart_service(&self, service: &str) -> Result<(), LifecycleError> {
		let result = self.docker_compose(["restart", service]).await;
		self.invalidate_status_cache().await;
		let healed = self.heal_networkless_services().await;
		self.attach_workspace_dev().await;
		result.map(|_| ()).and(healed)
	}

	/// Detect running-but-networkless service containers and
	/// force-recreate just those services.
	///
	/// A failed `docker start` (the classic trigger: a host-port
	/// conflict against another project publishing the same
	/// port) rolls back by wiping the container's stored network
	/// endpoints. From then on every plain start — compose
	/// `start`, `restart`, even `up` when the config hash hasn't
	/// changed — brings the container up with only a loopback
	/// interface: no service-name DNS, no published ports, yet
	/// "running (healthy)" if its healthcheck talks to
	/// `127.0.0.1`. The state never self-resolves; recreating
	/// the container is the only cure, so we do exactly that,
	/// scoped to the broken services (`--no-deps
	/// --force-recreate <svc>...`).
	///
	/// Called after every lifecycle action that (re)starts
	/// containers, and from the workspace shell's
	/// setup/rebuild reconcile. Detection failures are logged
	/// and swallowed (the probe must not make lifecycle ops less
	/// reliable); a failed *recreate* is returned to the caller
	/// — its stderr carries the real, actionable error (e.g.
	/// "Bind for 0.0.0.0:27017 failed: port is already
	/// allocated") that the silent zombie was hiding.
	pub(crate) async fn heal_networkless_services(&self) -> Result<(), LifecycleError> {
		let broken = match networkless_running_services(&self.project).await {
			Ok(set) => set,
			Err(err) => {
				tracing::debug!(%err, project = %self.project, "network probe failed; skipping networkless heal");
				return Ok(());
			}
		};
		if broken.is_empty() {
			return Ok(());
		}
		tracing::warn!(
			project = %self.project,
			services = ?broken,
			"running containers hold no network endpoints (failed-start residue); force-recreating",
		);
		let mut args: Vec<&str> = vec!["up", "-d", "--no-deps", "--force-recreate"];
		args.extend(broken.iter().map(String::as_str));
		let result = self.docker_compose(args).await;
		self.invalidate_status_cache().await;
		result.map(|_| ())
	}

	/// Best-effort attach of the workspace `dev` container to
	/// this project's default network.
	///
	/// Failures are logged at `warn` and swallowed: the user's
	/// project services are up, which is the success the caller
	/// was after; the cross-project DNS perk is a quality-of-life
	/// add. Common failure paths include:
	///
	/// - The workspace shell isn't up yet (cold-start order: user
	///   started a project before opting into the workspace
	///   shell). The next `Workspace::setup` reconciles by
	///   re-attaching across all running projects.
	/// - The user's project compose declares an explicit
	///   top-level `networks:` block that doesn't include
	///   `default`, so `<project>_default` doesn't exist. The
	///   user picked that segmentation deliberately; we surface
	///   the limitation in `specs/containers.md`.
	async fn attach_workspace_dev(&self) {
		let network = project_default_network(&self.project);
		let dev = dev_container_name(&self.workspace_project);
		if let Err(err) = connect_container_to_network(&network, &dev).await {
			tracing::warn!(
				%err,
				project = %self.project,
				network,
				dev,
				"failed to attach workspace dev container to project network",
			);
		}
	}

	/// Best-effort detach. Mirror of `attach_workspace_dev` —
	/// errors are logged and swallowed so a `down` doesn't
	/// fail because the network already disappeared.
	async fn detach_workspace_dev(&self) {
		let network = project_default_network(&self.project);
		let dev = dev_container_name(&self.workspace_project);
		if let Err(err) = disconnect_container_from_network(&network, &dev).await {
			tracing::warn!(
				%err,
				project = %self.project,
				network,
				dev,
				"failed to detach workspace dev container from project network",
			);
		}
	}

	/// Run `docker compose -f <user-file> -f <restart-override> -p
	/// <project> <args...>` against this folder.
	///
	/// The override file is (re)generated on every call so a user
	/// who adds a new service to their compose doesn't have to
	/// remember to nuke a stale cache. It's cheap — one
	/// `docker compose config --services` round-trip plus a few
	/// hundred bytes written to disk — and runs entirely against
	/// the local daemon. If the override step fails (daemon down,
	/// compose file broken), the error bubbles up before we touch
	/// any containers: the user sees the *real* reason, not a
	/// confusing later failure from `up`.
	///
	/// See [`crate::restart_override`] for the rationale.
	async fn docker_compose<I, S>(&self, args: I) -> Result<crate::lifecycle::DockerOutput, LifecycleError>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		let override_file = ensure_restart_override(&self.state_dir, &self.slug, &self.compose_file).await?;
		run_docker_compose_with_overrides(&self.compose_file, &[override_file], &self.project, args).await
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

	fn state_dir() -> Utf8PathBuf {
		Utf8PathBuf::from("/tmp/moon-ide-test-state")
	}

	#[test]
	fn for_folder_returns_none_without_compose() {
		let tmp = tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		assert!(ProjectCompose::for_folder("default", &state_dir(), &root)
			.unwrap()
			.is_none());
	}

	#[test]
	fn for_folder_finds_root_compose_and_derives_project_name() {
		let tmp = tempdir().unwrap();
		let parent = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let root = parent.join("moon-landing");
		touch(&root.join("docker-compose.yml"));

		let pc = ProjectCompose::for_folder("default", &state_dir(), &root)
			.unwrap()
			.unwrap();
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

		let pc = ProjectCompose::for_folder("default", &state_dir(), &root)
			.unwrap()
			.unwrap();
		assert_eq!(pc.project().as_str(), "moon-ws-default-my-stuff");
	}

	#[test]
	fn for_folder_propagates_invalid_workspace_id() {
		let tmp = tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		touch(&root.join("docker-compose.yml"));

		let err = ProjectCompose::for_folder("Bad ID", &state_dir(), &root).unwrap_err();
		assert!(matches!(err, LifecycleError::InvalidWorkspaceId(_)));
	}

	#[test]
	fn slug_helper_re_export_matches_internal_helper() {
		assert_eq!(slug_for_folder_basename("Moon Landing"), "moon-landing");
	}
}
