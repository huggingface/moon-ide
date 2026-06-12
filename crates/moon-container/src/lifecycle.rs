//! Workspace container lifecycle — the layer that actually shells
//! out to `docker compose`.
//!
//! Everything in this module is built around a single
//! [`Workspace`] handle that captures *the same three things every
//! `docker compose` invocation needs*:
//!
//! - the workspace's compose project name (`moon-ws-<id>` —
//!   see [`crate::project`]),
//! - the absolute path to the generated `compose.yaml`,
//! - the bound-folder list, which is what gets bind-mounted into
//!   the dev container and what gets scanned for project compose
//!   files.
//!
//! From those, every command is `docker compose -f <path> -p
//! <name> <subcommand>`. Both flags are always set explicitly so
//! the working-directory of the spawned process never matters,
//! and so `docker compose` doesn't accidentally pick up a
//! different default project name from a `.env` file.
//!
//! State directory layout
//! ----------------------
//!
//! `<state_dir>/compose.yaml` where `state_dir` is
//! `~/.local/share/moon-ide/workspaces/<id>/` — outside any
//! repo, decoupled from any specific folder. Sibling
//! `bound-folders.json` records the bound-folder set the
//! `compose.yaml` was generated from; the lifecycle layer treats
//! both as fully generated.
//!
//! Workspace shell vs project services
//! -----------------------------------
//!
//! This module owns the **workspace shell** — i.e. the `dev`
//! container moon-ide uses to run terminals, LSPs, etc. It
//! deliberately doesn't `include:` per-folder `docker-compose.yml`
//! files anymore: those are managed as separate compose projects
//! per folder, by [`crate::project_compose`]. That split lets the
//! user start/stop their project services independently of the
//! IDE's shell — gitaly being broken in `moon-landing` doesn't
//! prevent you from opening a terminal in the workspace.
//!
//! Why a thin shell-out, not bollard
//! ---------------------------------
//!
//! `docker compose` does substantial orchestration — include
//! resolution, network creation, dependency ordering, health-check
//! waits, x-extension pass-through. Reimplementing that on top of
//! the raw Engine API (which is what `bollard` exposes) would be
//! a meaningful project unto itself, with no upside for the IDE:
//! we *want* compose's behaviour, verbatim. Shelling out keeps us
//! one Docker upgrade away from inheriting upstream improvements.
//!
//! What's not here yet
//! -------------------
//!
//! - **Log streaming.** [`Workspace::status`] is a snapshot.
//!   Streaming `docker compose logs --follow` into a tokio
//!   channel for the UI lands with the per-service UI in 2.4.
//! - **Cancellation.** A `setup` mid-flight that the user wants
//!   to abort — also Tauri-layer, so it can be wired to the
//!   status pip's "Cancel" button.

use std::ffi::OsStr;
use std::time::Instant;

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::container::{ContainerState, ContainerStatus, ServiceStatus};
use moon_protocol::MoonError;
use thiserror::Error;
use tokio::process::Command;

use crate::compose::{
	generate_compose, BoundMount, ComposeRender, ComposeRenderOptions, GhConfigMount, HostGhToken, HostGitIdentity,
	MoonEditSocketMount, SshAgentForward, SshConfigMount,
};
use crate::network::{connect_container_to_network, dev_container_name, project_default_network};
use crate::port_forward::stop_forwards;
use crate::project::{project_name_for_id, ProjectName, ProjectNameError};
use crate::project_compose::ProjectCompose;
use crate::status_cache;

/// The image reference written into a freshly generated
/// `compose.yaml` if the caller doesn't override it.
///
/// Currently a *local* tag (`moon-base:dev`) because moon-base
/// hasn't been published yet — see ADR 0007. Once the GitHub
/// Actions workflow ships its first image to Docker Hub this
/// flips to `huggingface/moon-base:0.1` (or similar).
pub const DEFAULT_DEV_IMAGE: &str = "moon-base:dev";

/// Filename of the bound-folder sidecar inside the state dir.
pub const BOUND_FOLDERS_FILE: &str = "bound-folders.json";

/// Filename of the generated compose file inside the state dir.
pub const COMPOSE_FILE: &str = "compose.yaml";

/// Subdirectory of the state dir holding the per-workspace focus
/// socket (`instance.sock`) and nothing else. The dev container
/// bind-mounts this directory rather than the socket file so the
/// socket can be rebound across IDE restarts without the mount
/// going stale, and so Docker never auto-creates the socket path
/// as a root-owned entry. Mirrors `focus_socket::socket_dir`'s
/// `run/` segment in `src-tauri`. See [ADR 0026](../../../specs/decisions/0026-socket-dir-mount.md).
pub const SOCKET_DIR: &str = "run";

/// Errors the lifecycle layer surfaces to callers.
///
/// We keep `DockerMissing` and `DaemonUnreachable` distinct from
/// the generic `ComposeFailed` because the UI's remediation
/// differs: "install Docker", "start Docker Desktop", or "look
/// at this stderr".
#[derive(Debug, Error)]
pub enum LifecycleError {
	#[error("docker is not installed or not on PATH")]
	DockerMissing,

	#[error("docker daemon not reachable: {0}")]
	DaemonUnreachable(String),

	#[error("docker compose failed (exit {code}): {stderr}")]
	ComposeFailed { code: i32, stderr: String },

	#[error("docker {subcommand} failed (exit {code}): {stderr}")]
	DockerCommandFailed {
		subcommand: String,
		code: i32,
		stderr: String,
	},

	#[error("could not parse `docker compose ps` output: {0}")]
	ParseError(String),

	#[error("invalid workspace id: {0}")]
	InvalidWorkspaceId(#[from] ProjectNameError),

	#[error("io error: {0}")]
	Io(#[from] std::io::Error),
}

/// Map lifecycle errors onto the protocol-level `MoonError` the
/// Tauri command boundary expects.
///
/// `DockerMissing` and `DaemonUnreachable` both flatten to
/// `HostUnavailable` — that's the same variant fs / search
/// commands use when the workspace host can't be reached, so
/// the UI's existing "host disconnected" affordance covers
/// "Docker isn't running" without a new code path. The genuine
/// "compose itself failed" and "ps stdout was malformed" cases
/// stay as `Internal`, carrying the daemon's stderr so the user
/// can copy-paste it into a support thread.
impl From<LifecycleError> for MoonError {
	fn from(err: LifecycleError) -> Self {
		match err {
			LifecycleError::DockerMissing => MoonError::HostUnavailable("docker is not installed or not on PATH".into()),
			LifecycleError::DaemonUnreachable(msg) => MoonError::HostUnavailable(msg),
			LifecycleError::ComposeFailed { code, stderr } => {
				MoonError::internal(format!("docker compose failed (exit {code}): {stderr}"))
			}
			LifecycleError::DockerCommandFailed {
				subcommand,
				code,
				stderr,
			} => MoonError::internal(format!("docker {subcommand} failed (exit {code}): {stderr}")),
			LifecycleError::ParseError(msg) => MoonError::internal(format!("could not parse docker compose output: {msg}")),
			LifecycleError::InvalidWorkspaceId(err) => MoonError::invalid(err.to_string()),
			LifecycleError::Io(err) => MoonError::from(err),
		}
	}
}

/// Inputs the caller resolves before constructing a [`Workspace`].
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
	/// Stable, validated workspace identifier — the slug from
	/// the running process's `--workspace <slug>` CLI arg.
	/// Becomes the suffix on the compose project name and the
	/// directory name under `<workspaces_dir>/`.
	pub workspace_id: String,
	/// Where the workspace's `compose.yaml` and
	/// `bound-folders.json` live. Created on first write.
	pub state_dir: Utf8PathBuf,
	/// Absolute paths to every folder the workspace has bound,
	/// in user-chosen order. Each is bind-mounted at
	/// `/workspace/<basename>` inside the dev container.
	pub bound_folders: Vec<Utf8PathBuf>,
}

/// Handle on a workspace's compose project. Cheap to construct
/// (no I/O).
#[derive(Debug, Clone)]
pub struct Workspace {
	workspace_id: String,
	state_dir: Utf8PathBuf,
	bound_folders: Vec<Utf8PathBuf>,
	project: ProjectName,
	compose_path: Utf8PathBuf,
	bound_folders_path: Utf8PathBuf,
}

impl Workspace {
	/// Construct a handle from validated inputs.
	pub fn new(config: WorkspaceConfig) -> Result<Self, LifecycleError> {
		let project = project_name_for_id(&config.workspace_id)?;
		let compose_path = config.state_dir.join(COMPOSE_FILE);
		let bound_folders_path = config.state_dir.join(BOUND_FOLDERS_FILE);
		Ok(Self {
			workspace_id: config.workspace_id,
			state_dir: config.state_dir,
			bound_folders: config.bound_folders,
			project,
			compose_path,
			bound_folders_path,
		})
	}

	pub fn project(&self) -> &ProjectName {
		&self.project
	}

	pub fn compose_path(&self) -> &Utf8Path {
		&self.compose_path
	}

	pub fn bound_folders_path(&self) -> &Utf8Path {
		&self.bound_folders_path
	}

	pub fn bound_folders(&self) -> &[Utf8PathBuf] {
		&self.bound_folders
	}

	/// True iff `<state_dir>/compose.yaml` exists. Doesn't say
	/// anything about whether the containers are up.
	pub fn is_initialized(&self) -> bool {
		self.compose_path.is_file()
	}

	/// Render what `compose.yaml` *would* look like if we
	/// generated it right now from the current bound-folder set.
	/// Useful for an "Inspect" affordance before the user clicks
	/// "Set up".
	///
	/// SSH agent forwarding is resolved per call from the host
	/// environment (see [`detect_ssh_agent_forward`]), so the
	/// rendered file always reflects the agent the IDE could
	/// reach at this moment. If the host has no agent the
	/// dev-service still renders correctly, just without the
	/// volume + environment block.
	pub fn render_compose(&self, dev_image: &str) -> ComposeRender {
		let mounts = self.bound_mounts();
		let agent = detect_ssh_agent_forward();
		let ssh_config = detect_host_ssh_config();
		let identity = detect_host_git_identity();
		let gh_config = detect_host_gh_config();
		let gh_token = detect_host_gh_token();
		// The IDE's per-workspace focus socket lives in the
		// `run/` subdir of `state_dir`. moon-ide creates that
		// directory before any compose call runs (pre-Tauri in
		// `src-tauri/src/focus_socket.rs::try_bind`, and again in
		// `write_state` below) so it's guaranteed to exist,
		// user-owned, by the time compose renders. We mount the
		// directory (ADR 0026), not the socket file. Forwards
		// `$GIT_EDITOR` into the host IDE from container
		// terminals; see ADR 0021.
		let moon_edit = MoonEditSocketMount {
			host_dir: self.state_dir.join(SOCKET_DIR),
		};
		generate_compose(ComposeRenderOptions {
			project: &self.project,
			dev_image,
			bound_mounts: &mounts,
			ssh_agent: agent.as_ref(),
			ssh_config: ssh_config.as_ref(),
			git_identity: identity.as_ref(),
			gh_config: gh_config.as_ref(),
			gh_token: gh_token.as_ref(),
			moon_edit_socket: Some(&moon_edit),
		})
	}

	/// Compute the volume-mount entries from the bound-folder
	/// list. The basename becomes the `/workspace/<name>` segment
	/// — duplicates across the workspace are the registry layer's
	/// problem to refuse, this layer trusts the input.
	fn bound_mounts(&self) -> Vec<BoundMount> {
		self
			.bound_folders
			.iter()
			.map(|path| BoundMount {
				host_path: path.clone(),
				mount_name: mount_name_for(path),
			})
			.collect()
	}

	/// Persist the current bound-folder set + regenerated compose
	/// file to disk. Idempotent — writes the same bytes every
	/// time the input is the same. Returns `Ok(true)` if either
	/// file's contents changed (so callers can decide whether to
	/// re-apply via `docker compose up`).
	pub async fn write_state(&self, dev_image: &str) -> Result<bool, LifecycleError> {
		tokio::fs::create_dir_all(self.state_dir.as_std_path()).await?;
		// Make sure the socket directory exists, user-owned,
		// before the next `docker compose up` mounts it — Docker
		// would otherwise auto-create a missing bind-mount source
		// as a root-owned directory we can't bind into (ADR 0026).
		tokio::fs::create_dir_all(self.state_dir.join(SOCKET_DIR).as_std_path()).await?;
		let render = self.render_compose(dev_image);
		let bound_json = render_bound_folders_json(&self.bound_folders);

		let compose_changed = write_if_changed(&self.compose_path, render.yaml.as_bytes()).await?;
		let bound_changed = write_if_changed(&self.bound_folders_path, bound_json.as_bytes()).await?;
		Ok(compose_changed || bound_changed)
	}

	/// Snapshot the compose project's state.
	///
	/// `Absent` is returned without invoking docker if there's no
	/// `compose.yaml` yet — opening a fresh workspace is the
	/// common case and we don't want to pay a `docker compose`
	/// invocation per open just to confirm "no, still nothing".
	///
	/// Reads are TTL-cached (see [`crate::status_cache`]). Every
	/// mutating method below invalidates the cache on success so
	/// `pause()` followed immediately by `status()` reports the
	/// fresh `Paused` rather than a stale `Running`.
	pub async fn status(&self) -> Result<ContainerStatus, LifecycleError> {
		if !self.compose_path.is_file() {
			return Ok(ContainerStatus {
				state: ContainerState::Absent,
				services: Vec::new(),
			});
		}
		status_cache::get_or_fetch(&self.project, &self.compose_path, Instant::now(), || async {
			let output = self.docker_compose(["ps", "--all", "--format", "json"]).await?;
			let mut services = parse_ps_output(&output.stdout).map_err(LifecycleError::ParseError)?;
			flag_networkless_services(&self.project, &mut services).await;
			let state = aggregate_state(&services);
			Ok(ContainerStatus { state, services })
		})
		.await
	}

	/// Drop any cached `status()` reading so the next call
	/// re-probes `docker compose ps`. Called by every mutating
	/// method after the underlying `docker compose` succeeds.
	async fn invalidate_status_cache(&self) {
		status_cache::invalidate(&self.project, &self.compose_path).await;
	}

	/// First-time opt-in: regenerate the workspace's compose
	/// state from the current bound-folder set, then `docker
	/// compose up -d --wait` so we don't return until everything
	/// is healthy (or has failed).
	///
	/// After the dev container is up, reconciles network
	/// attachments to every bound folder whose project services
	/// are already running — covers the case where the user
	/// started a project's services first (cold-start) and only
	/// then opted into the workspace shell.
	pub async fn setup(&self, dev_image: &str) -> Result<(), LifecycleError> {
		self.write_state(dev_image).await?;
		self.docker_compose(["up", "-d", "--wait"]).await?;
		self.invalidate_status_cache().await;
		self.reattach_running_projects().await;
		Ok(())
	}

	/// Refresh `compose.yaml` + `bound-folders.json` from the
	/// current bound-folder set, then — if the project is
	/// already running — apply the diff with `up -d --wait`.
	///
	/// Used by the IDE after the user adds or removes a folder.
	/// If the project isn't running yet (status `Absent`,
	/// `Paused`, `Stopped`, or `Failed`), we only persist the
	/// new files and let the next explicit lifecycle action
	/// (`setup` / `rebuild`) bring the change in. This avoids
	/// surprise-recreating containers while the user has them
	/// paused on purpose.
	pub async fn apply_bound_folders(&self, dev_image: &str) -> Result<(), LifecycleError> {
		let changed = self.write_state(dev_image).await?;
		if !changed {
			return Ok(());
		}
		let status = self.status().await?;
		if matches!(status.state, ContainerState::Running) {
			// `up -d --wait` recreates the dev container when its
			// mount set changes — that drops every prior project-
			// network attachment, same as `rebuild`. Reattach.
			self.docker_compose(["up", "-d", "--wait"]).await?;
			self.invalidate_status_cache().await;
			self.reattach_running_projects().await;
		}
		Ok(())
	}

	/// Pause every container in the project. Idempotency: if
	/// some are already paused, `docker compose pause` errors —
	/// callers should check [`Workspace::status`] first.
	pub async fn pause(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["pause"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// Inverse of [`Workspace::pause`].
	pub async fn resume(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["unpause"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// Force-recreate every container, pulling fresh images
	/// first. The hammer: use this when the moon-base reference
	/// changed, when an included compose changed in a way `up`
	/// didn't pick up, or when the user just wants to start
	/// over.
	///
	/// Force-recreate replaces the dev container, so any prior
	/// project-network attachments are lost; we reconcile them
	/// after the recreate.
	pub async fn rebuild(&self, dev_image: &str) -> Result<(), LifecycleError> {
		// Make sure the file on disk reflects the current
		// bound-folder set before forcing a recreate — otherwise
		// "Rebuild" could end up resurrecting the previous
		// generation's mounts.
		self.write_state(dev_image).await?;
		self
			.docker_compose(["up", "-d", "--force-recreate", "--pull", "always", "--wait"])
			.await?;
		self.invalidate_status_cache().await;
		self.reattach_running_projects().await;
		Ok(())
	}

	/// `docker compose stop` — SIGTERM every container in the
	/// project but leave the records on the daemon. Cheaper to
	/// undo than [`Self::teardown`]: a follow-up `setup` resumes
	/// from the same containers without re-pulling images. The
	/// in-container process state is lost (LSPs restart, shells
	/// re-init) — users who need that to survive can reach for
	/// `docker compose pause` from a terminal.
	pub async fn stop(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["stop"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// `docker compose down` — stop and remove containers,
	/// networks, and the project entry. The compose file itself
	/// stays on disk; the next `setup` resurrects from there.
	///
	/// `down` removes the dev container, which also drops any
	/// project-network attachments it carried — no explicit
	/// detach is needed. The proxy sidecar from
	/// [`crate::port_forward`] *is* attached to the workspace's
	/// default network, though, and Docker refuses to remove a
	/// network with active endpoints — so stop the sidecar first
	/// (best-effort: a missing sidecar is success).
	pub async fn teardown(&self) -> Result<(), LifecycleError> {
		if let Err(err) = stop_forwards(&self.project).await {
			tracing::warn!(%err, project = %self.project, "best-effort stop of port-forward sidecar before teardown failed");
		}
		self.docker_compose(["down"]).await?;
		self.invalidate_status_cache().await;
		Ok(())
	}

	/// Walk the bound-folder set and, for each folder whose
	/// project compose is currently running, re-attach this
	/// workspace's dev container to that project's default
	/// network. Best-effort throughout: a folder without a
	/// compose file, a project that's down, an attach failure
	/// — all logged at `debug` / `warn` and skipped. The dev
	/// container being up is a hard prerequisite (the caller
	/// drives that).
	///
	/// Two situations rely on this:
	///
	/// 1. Cold-start where the user brings up project services
	///    *before* opting into the workspace shell. The
	///    project's `up` couldn't attach the dev container
	///    (didn't exist yet); the workspace's `setup` reconciles.
	/// 2. Workspace `rebuild` recreates the dev container, which
	///    drops every prior attachment. Reconcile here is the
	///    only way to re-establish them without touching every
	///    project's lifecycle.
	///
	/// Each project is also healed of running-but-networkless
	/// containers first (see
	/// [`ProjectCompose::heal_networkless_services`]) — this is
	/// the path that repairs them on IDE relaunch, and it must
	/// run before the `Running` gate below because a networkless
	/// service drags the project's aggregate to `Failed`.
	async fn reattach_running_projects(&self) {
		let dev = dev_container_name(&self.project);
		for folder in &self.bound_folders {
			let pc = match ProjectCompose::for_folder(&self.workspace_id, &self.state_dir, folder) {
				Ok(Some(pc)) => pc,
				Ok(None) => continue,
				Err(err) => {
					tracing::debug!(%err, %folder, "skipping reattach: invalid project handle");
					continue;
				}
			};
			if let Err(err) = pc.heal_networkless_services().await {
				// Keep going: the recreate's error is also visible
				// through the project's Failed status + per-service
				// detail, and one broken folder must not block the
				// other folders' reattach.
				tracing::warn!(%err, %folder, project = %pc.project(), "networkless heal failed during reattach");
			}
			let status = match pc.status().await {
				Ok(s) => s,
				Err(err) => {
					tracing::debug!(%err, %folder, "skipping reattach: status query failed");
					continue;
				}
			};
			if !matches!(status.state, ContainerState::Running) {
				continue;
			}
			let network = project_default_network(pc.project());
			if let Err(err) = connect_container_to_network(&network, &dev).await {
				tracing::warn!(
					%err,
					%folder,
					project = %pc.project(),
					network,
					dev,
					"failed to reattach dev container to project network",
				);
			}
		}
	}

	async fn docker_compose<I, S>(&self, args: I) -> Result<DockerOutput, LifecycleError>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		run_docker_compose(&self.compose_path, &self.project, args).await
	}
}

pub(crate) struct DockerOutput {
	pub(crate) stdout: Vec<u8>,
}

/// Run `docker compose -f <compose> -p <project> <args...>`.
///
/// Used by the workspace shell ([`Workspace`]). Per-folder
/// project services go through [`run_docker_compose_with_overrides`]
/// so they can layer the restart-policy override on top.
pub(crate) async fn run_docker_compose<I, S>(
	compose_path: &Utf8Path,
	project: &ProjectName,
	args: I,
) -> Result<DockerOutput, LifecycleError>
where
	I: IntoIterator<Item = S>,
	S: AsRef<OsStr>,
{
	run_docker_compose_with_overrides::<_, _, &Utf8Path>(compose_path, &[], project, args).await
}

/// Run `docker compose -f <compose> [-f <override>]... -p
/// <project> <args...>`.
///
/// Used by both the workspace shell (via [`run_docker_compose`],
/// no overrides) and the per-folder project runner (with the
/// generated restart-policy override layered on top). Both pin
/// `-f` and `-p` explicitly so the spawned process's CWD or any
/// ambient `.env` never colours the result.
///
/// Compose merge semantics: later `-f` files override earlier
/// ones key by key — exactly the behaviour the restart override
/// needs (the override's `restart: "no"` wins, anything else in
/// the base file stays).
pub(crate) async fn run_docker_compose_with_overrides<I, S, P>(
	compose_path: &Utf8Path,
	override_paths: &[P],
	project: &ProjectName,
	args: I,
) -> Result<DockerOutput, LifecycleError>
where
	I: IntoIterator<Item = S>,
	S: AsRef<OsStr>,
	P: AsRef<Utf8Path>,
{
	let mut cmd = Command::new("docker");
	cmd.arg("compose");
	cmd.arg("-f").arg(compose_path.as_std_path());
	for over in override_paths {
		cmd.arg("-f").arg(over.as_ref().as_std_path());
	}
	cmd.arg("-p").arg(project.as_str());
	for arg in args {
		cmd.arg(arg);
	}

	tracing::debug!(
		%project,
		compose = %compose_path,
		"running docker compose",
	);

	let output = cmd.output().await.map_err(|err| {
		if err.kind() == std::io::ErrorKind::NotFound {
			LifecycleError::DockerMissing
		} else {
			LifecycleError::Io(err)
		}
	})?;

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
		if stderr.contains("Cannot connect to the Docker daemon") {
			return Err(LifecycleError::DaemonUnreachable(stderr));
		}
		return Err(LifecycleError::ComposeFailed {
			code: output.status.code().unwrap_or(-1),
			stderr,
		});
	}

	Ok(DockerOutput { stdout: output.stdout })
}

/// Pick the container-side directory name for a host-side folder.
///
/// Always the basename. Callers (i.e. the workspace registry) are
/// responsible for refusing to bind two folders that would
/// collide on the same name; this layer falls back to a generic
/// `root` name if the basename is missing (e.g. binding `/` —
/// nonsensical in practice, but we don't want to emit an empty
/// mount segment).
fn mount_name_for(path: &Utf8Path) -> String {
	path
		.file_name()
		.filter(|s| !s.is_empty())
		.map(str::to_owned)
		.unwrap_or_else(|| "root".to_owned())
}

/// Resolve the host-side SSH agent socket to bind into the dev
/// container, if one is reachable.
///
/// Two paths:
///
/// - **macOS**: Docker Desktop special-cases
///   `/run/host-services/ssh-auth.sock` and forwards reads to the
///   host's running agent. We always emit that mount on macOS;
///   the path's existence inside the VM is Docker Desktop's
///   responsibility, not ours.
/// - **Linux**: read `$SSH_AUTH_SOCK` and bind it directly. If
///   the env var is unset or the socket file doesn't exist we
///   skip the forward (the user gets a working container, just
///   without ssh-agent — `tracing::warn!` flags it once on
///   compose write).
///
/// Re-evaluated every time we render or write `compose.yaml`,
/// so a contributor who starts an agent after the IDE is open
/// just needs to recreate the dev container (palette → "Rebuild
/// container") to pick it up.
pub(crate) fn detect_ssh_agent_forward() -> Option<SshAgentForward> {
	if cfg!(target_os = "macos") {
		return Some(SshAgentForward {
			host_socket: Utf8PathBuf::from("/run/host-services/ssh-auth.sock"),
		});
	}
	let raw = match std::env::var("SSH_AUTH_SOCK") {
		Ok(s) if !s.is_empty() => s,
		_ => {
			tracing::debug!("SSH_AUTH_SOCK not set; skipping ssh agent forwarding");
			return None;
		}
	};
	let path = Utf8PathBuf::from(raw);
	if !path.exists() {
		tracing::warn!(
			%path,
			"SSH_AUTH_SOCK is set but path doesn't exist; skipping ssh agent forwarding",
		);
		return None;
	}
	Some(SshAgentForward { host_socket: path })
}

/// Read the host's `git config --global user.{name,email}` so we
/// can project them into the dev container as `GIT_AUTHOR_*` /
/// `GIT_COMMITTER_*`. Returns `None` when **either** field is
/// missing — a half-configured identity (name without email, or
/// vice versa) is exactly the case where git refuses to commit
/// anyway, so promoting it into the container would just defer
/// the same error to commit time.
///
/// `--global` (not `--get`) is deliberate: we want the user-level
/// identity, not whatever per-repo override the IDE happens to be
/// running inside. The container has no notion of "current repo"
/// at startup, and per-repo overrides will still be respected
/// when the user `cd`s into a folder with one (env vars take
/// precedence over `~/.gitconfig`, but per-repo `.git/config`
/// takes precedence over env vars).
///
/// Resolved fresh per `render_compose` call (cheap shell-out;
/// runs maybe twice per workspace lifetime). If the user updates
/// their host gitconfig after the container is up, regenerating
/// the workspace compose file picks up the new identity — the
/// IDE's "Rebuild container" affordance is the user-facing path.
pub(crate) fn detect_host_git_identity() -> Option<HostGitIdentity> {
	let name = read_git_global_config("user.name")?;
	let email = read_git_global_config("user.email")?;
	Some(HostGitIdentity { name, email })
}

/// Resolve the host's `~/.ssh/config` and bind-mount it into the
/// dev container so an in-container `ssh` knows the user's
/// `Host` aliases, `ProxyJump` chains, and per-host options.
/// Without this, a command like `ssh europe` inside a container
/// terminal — perfectly fine on the host — falls through to DNS
/// resolution of the literal alias and hangs.
///
/// Resolves to `$HOME/.ssh/config` (or `$USERPROFILE/.ssh/config`
/// on Windows-ish hosts, included for symmetry with
/// [`detect_host_gh_config`]). Returns `None` when the file isn't
/// there — mounting a non-existent path would have Docker
/// auto-create the source as an empty directory and shadow any
/// later host config until the container is recreated.
///
/// Private key material is deliberately **not** mounted. The
/// agent forward ([`detect_ssh_agent_forward`]) is the intended
/// auth path; `IdentityFile` directives in the config silently
/// fall through to the agent when the referenced key path
/// doesn't resolve inside the container.
///
/// Re-evaluated every time we render or write `compose.yaml`,
/// matching the other host-bridge detectors' "rebuild container
/// to pick it up" cadence.
pub(crate) fn detect_host_ssh_config() -> Option<SshConfigMount> {
	let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
	let path = Utf8PathBuf::from(home).join(".ssh").join("config");
	if !path.is_file() {
		tracing::debug!(
			%path,
			"ssh config not found; skipping ssh config pass-through into the container",
		);
		return None;
	}
	Some(SshConfigMount { host_path: path })
}

/// Resolve the host's `gh` config directory and bind-mount it
/// into the dev container so an in-container `gh` shares the
/// host's per-host preferences. Returns `None` when:
///
/// - `$GH_CONFIG_DIR` isn't set and the platform default
///   (`$XDG_CONFIG_HOME/gh` or `~/.config/gh`) doesn't exist —
///   the user has never run `gh auth login` on the host.
///   Mounting a non-existent path would have Docker auto-create
///   it as an empty directory and shadow any later host login
///   until the container is recreated.
/// - The home directory itself can't be resolved (no `$HOME`,
///   no `$USERPROFILE`) — extremely rare, but we'd rather skip
///   than guess.
///
/// Re-evaluated every time we render or write `compose.yaml`,
/// matching `detect_ssh_agent_forward`'s "rebuild container to
/// pick it up" cadence.
///
/// Note that this mount on its own does **not** authenticate
/// the in-container `gh` when the host uses the system keyring
/// (the modern default — `hosts.yml` carries no `oauth_token:`
/// in that mode). [`detect_host_gh_token`] is the companion that
/// forwards the active token as `GH_TOKEN` and covers both
/// storage shapes.
pub(crate) fn detect_host_gh_config() -> Option<GhConfigMount> {
	let raw = std::env::var("GH_CONFIG_DIR").ok().filter(|s| !s.is_empty());
	let path = if let Some(raw) = raw {
		Utf8PathBuf::from(raw)
	} else {
		let xdg = std::env::var("XDG_CONFIG_HOME").ok().filter(|s| !s.is_empty());
		let base = match xdg {
			Some(s) => Utf8PathBuf::from(s),
			None => {
				let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
				Utf8PathBuf::from(home).join(".config")
			}
		};
		base.join("gh")
	};
	if !path.is_dir() {
		tracing::debug!(
			%path,
			"gh config dir not found; skipping gh auth pass-through into the container",
		);
		return None;
	}
	Some(GhConfigMount { host_path: path })
}

/// Resolve the host's active `gh` OAuth token so we can pass it
/// into the dev container as `GH_TOKEN`. `gh auth token` is the
/// canonical extraction path — it reads from whichever storage
/// the host happens to be using (keyring or `hosts.yml`),
/// avoiding a fork in our own logic.
///
/// Returns `None` when:
///
/// - The `gh` binary isn't on the host's `$PATH`. The user may
///   not have installed it, or may have logged in via a custom
///   mechanism. Either way, no token, no env var.
/// - `gh auth token` exits non-zero (the typical signal for
///   "you're not logged in").
/// - Stdout is empty or non-UTF-8.
///
/// Re-evaluated every time we render or write `compose.yaml`, so
/// a host-side `gh auth refresh` or `gh auth login` is picked up
/// by the next "Rebuild container". The token ends up in
/// plaintext in the generated `compose.yaml` under the
/// per-workspace state dir — that's a deliberate trade-off (see
/// [`HostGhToken`] for the rationale).
pub(crate) fn detect_host_gh_token() -> Option<HostGhToken> {
	let output = std::process::Command::new("gh").args(["auth", "token"]).output().ok()?;
	if !output.status.success() {
		tracing::debug!(
			status = ?output.status,
			"gh auth token returned non-zero; skipping GH_TOKEN forward",
		);
		return None;
	}
	let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
	if value.is_empty() {
		return None;
	}
	Some(HostGhToken { token: value })
}

fn read_git_global_config(key: &str) -> Option<String> {
	let output = std::process::Command::new("git")
		.args(["config", "--global", "--get", key])
		.output()
		.ok()?;
	if !output.status.success() {
		return None;
	}
	let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
	if value.is_empty() {
		return None;
	}
	Some(value)
}

fn render_bound_folders_json(folders: &[Utf8PathBuf]) -> String {
	// Hand-rolled JSON — the structure is one array of strings,
	// stable ordering, and we'd rather not pull `serde_json` into
	// the rendering path just for that.
	let mut out = String::from("{\n  \"folders\": [");
	for (i, folder) in folders.iter().enumerate() {
		if i > 0 {
			out.push(',');
		}
		out.push_str("\n    ");
		// Re-use serde_json for escaping the path string itself
		// (serde is already a dep of this crate via moon-protocol).
		out.push_str(&serde_json::to_string(folder.as_str()).expect("UTF-8 path serializes"));
	}
	if folders.is_empty() {
		out.push_str("]\n}\n");
	} else {
		out.push_str("\n  ]\n}\n");
	}
	out
}

async fn write_if_changed(path: &Utf8Path, contents: &[u8]) -> Result<bool, std::io::Error> {
	match tokio::fs::read(path.as_std_path()).await {
		Ok(existing) if existing == contents => return Ok(false),
		Ok(_) | Err(_) => {}
	}
	tokio::fs::write(path.as_std_path(), contents).await?;
	Ok(true)
}

// Pure helpers — extracted so the parsing + aggregation logic is
// testable without a Docker daemon. Shared with the per-folder
// runner in `project_compose`, which goes through the same JSON
// shape.

/// Parse the stdout of `docker compose ps --format json`.
///
/// Compose's output format has shifted between versions: some
/// emit JSON-Lines (one container object per line), others emit
/// a single JSON array. We accept both.
pub(crate) fn parse_ps_output(stdout: &[u8]) -> Result<Vec<ServiceStatus>, String> {
	let s = std::str::from_utf8(stdout).map_err(|e| e.to_string())?;
	let trimmed = s.trim_start();
	if trimmed.is_empty() {
		return Ok(Vec::new());
	}
	if trimmed.starts_with('[') {
		let entries: Vec<PsEntry> = serde_json::from_str(trimmed.trim()).map_err(|e| e.to_string())?;
		return Ok(entries.into_iter().map(ServiceStatus::from).collect());
	}
	let mut out = Vec::new();
	for line in s.lines() {
		let line = line.trim();
		if line.is_empty() {
			continue;
		}
		let entry: PsEntry = serde_json::from_str(line).map_err(|e| e.to_string())?;
		out.push(ServiceStatus::from(entry));
	}
	Ok(out)
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsEntry {
	service: String,
	state: String,
	#[serde(default)]
	exit_code: i32,
	#[serde(default)]
	health: String,
}

impl From<PsEntry> for ServiceStatus {
	fn from(entry: PsEntry) -> Self {
		ServiceStatus {
			name: entry.service,
			raw_state: entry.state,
			exit_code: entry.exit_code,
			health: entry.health,
			// `compose ps` doesn't report network attachments;
			// the flag is filled in by `flag_networkless_services`
			// from a separate `docker ps` probe.
			networkless: false,
		}
	}
}

/// Cross-check `compose ps`'s view against the network probe and
/// flag running services whose container holds **no** network
/// endpoints (see [`crate::network::networkless_running_services`]
/// for how containers get into that state and why it never
/// self-resolves).
///
/// Best-effort: a probe failure leaves every flag `false` and
/// logs at `debug` — status reporting must not become less
/// available than `compose ps` itself just because the extra
/// probe flaked. Skips the probe entirely when nothing is
/// running, which keeps the common `Stopped`/`Absent` polls at
/// one docker invocation.
pub(crate) async fn flag_networkless_services(project: &ProjectName, services: &mut [ServiceStatus]) {
	let any_up = services
		.iter()
		.any(|svc| matches!(svc.raw_state.as_str(), "running" | "paused" | "restarting"));
	if !any_up {
		return;
	}
	let networkless = match crate::network::networkless_running_services(project).await {
		Ok(set) => set,
		Err(err) => {
			tracing::debug!(%err, %project, "network probe failed; skipping networkless detection");
			return;
		}
	};
	if networkless.is_empty() {
		return;
	}
	for svc in services.iter_mut() {
		if networkless.contains(&svc.name) {
			svc.networkless = true;
		}
	}
}

/// True for the conventional "process was terminated by a
/// stop signal" exit codes:
///
/// - `130` = 128 + SIGINT (Ctrl+C)
/// - `137` = 128 + SIGKILL (`docker kill`, OOM-killer, or
///   `compose stop` after the SIGTERM grace period elapsed)
/// - `143` = 128 + SIGTERM (`docker stop` / `compose stop`)
///
/// These show up the moment the user clicks "Stop" in moon-ide
/// (or quits the IDE, which runs `compose stop` against
/// everything it knows). They aren't application failures, so
/// we treat them as a clean stop in [`aggregate_state`] —
/// otherwise a JVM that exits 143 on SIGTERM would leave the
/// project pinned to `Failed` after the user explicitly asked
/// for it to stop, and break the `auto_resume_shell` path on
/// next IDE launch.
///
/// SIGSEGV (139), SIGABRT (134), SIGBUS (135), and friends are
/// deliberately *not* on this list: those are real crashes the
/// user should see surfaced.
pub(crate) fn is_stop_signal(exit_code: i32) -> bool {
	matches!(exit_code, 130 | 137 | 143)
}

/// Roll the per-service state list up into a single
/// [`ContainerState`] for the status pip.
///
/// Precedence (highest wins):
///
/// 1. `Paused` — any container paused.
/// 2. `Failed` —
///    - any container `dead` or in an unrecognised state, OR
///    - any container exited with a non-zero, non-signal code
///      (including a previously-failed init container blocking
///      `cas` or similar
///      `depends_on: service_completed_successfully` consumers
///      from ever starting). Signal-termination codes
///      (130/137/143; see [`is_stop_signal`]) are treated as a
///      clean stop, not a failure, OR
///    - any running container holds no network endpoints
///      (`ServiceStatus::networkless` — wiped by a failed
///      start; the service is unreachable and needs a
///      recreate).
/// 3. `Creating` — any container `restarting`, or a mix of
///    `running` + `created` (depends_on chains take time to
///    resolve during `compose up`; absent any non-zero exit
///    we lean toward "still coming up" rather than failed).
/// 4. `Running` — at least one container running and the rest
///    are healthy zero-exit init containers.
/// 5. `Stopped` — everything is `created`, zero-exit `exited`,
///    or signal-exited (project never came up, or the user
///    stopped it manually, or the IDE quit and ran
///    `compose stop` against everything). Init-only projects
///    that completed land here.
/// 6. `Absent` — no containers reported.
pub(crate) fn aggregate_state(services: &[ServiceStatus]) -> ContainerState {
	if services.is_empty() {
		return ContainerState::Absent;
	}
	let mut any_paused = false;
	let mut any_networkless = false;
	let mut any_running = false;
	let mut any_restarting = false;
	let mut any_dead_or_unknown = false;
	let mut any_exited_clean = false;
	let mut any_exited_failed = false;
	let mut any_created = false;
	for svc in services {
		if svc.networkless {
			any_networkless = true;
		}
		match svc.raw_state.as_str() {
			"paused" => any_paused = true,
			"running" => any_running = true,
			"restarting" => any_restarting = true,
			"created" => any_created = true,
			"exited" => {
				if svc.exit_code == 0 || is_stop_signal(svc.exit_code) {
					any_exited_clean = true;
				} else {
					any_exited_failed = true;
				}
			}
			"dead" => any_dead_or_unknown = true,
			// Anything we don't recognise is treated as
			// "something's wrong" rather than silently dropped.
			_ => any_dead_or_unknown = true,
		}
	}

	if any_paused {
		return ContainerState::Paused;
	}
	// A running container with no network endpoints is broken no
	// matter what its healthcheck says — unreachable by name,
	// nothing published. Surface it as Failed so the folder-bar
	// glyph flips instead of showing a green zombie.
	if any_dead_or_unknown || any_exited_failed || any_networkless {
		return ContainerState::Failed;
	}
	// `running` + `created` is the normal in-progress state
	// during `compose up`: long-running upstreams are up while
	// downstreams sit in `created` waiting on their `depends_on`
	// chain to resolve (e.g. `cas` waits on `cas-deps` to exit
	// 0). Absent any non-zero-exit signal we treat that as
	// Creating, not Failed — a genuinely stuck consumer surfaces
	// via the per-service indicator and via the upstream's
	// non-zero exit when it eventually fails.
	if any_restarting || (any_running && any_created) {
		return ContainerState::Creating;
	}
	if any_running {
		return ContainerState::Running;
	}
	// At this point the only remaining states are `created`
	// and clean-exited (zero or stop-signal). Both are inert:
	// project was either stopped or never started fully.
	// Either way the status pip's "Stopped" affordance is the
	// right next step.
	if any_created || any_exited_clean {
		return ContainerState::Stopped;
	}
	// Unreachable in practice: the loop covers every observed
	// state and all unknowns flip `any_dead_or_unknown`.
	// Defaulting to `Failed` here is the safe choice if a
	// future Docker version adds a new status string.
	ContainerState::Failed
}

#[cfg(test)]
mod tests {
	use super::*;

	fn svc(name: &str, raw: &str) -> ServiceStatus {
		ServiceStatus {
			name: name.into(),
			raw_state: raw.into(),
			exit_code: 0,
			health: String::new(),
			networkless: false,
		}
	}

	fn exited(name: &str, code: i32) -> ServiceStatus {
		ServiceStatus {
			name: name.into(),
			raw_state: "exited".into(),
			exit_code: code,
			health: String::new(),
			networkless: false,
		}
	}

	fn networkless(name: &str) -> ServiceStatus {
		ServiceStatus {
			networkless: true,
			..svc(name, "running")
		}
	}

	fn workspace_in(state_dir: Utf8PathBuf, folders: Vec<Utf8PathBuf>) -> Workspace {
		Workspace::new(WorkspaceConfig {
			workspace_id: "default".into(),
			state_dir,
			bound_folders: folders,
		})
		.expect("default is a valid id")
	}

	#[test]
	fn parse_ps_empty_stdout_is_empty_list() {
		assert!(parse_ps_output(b"").unwrap().is_empty());
		assert!(parse_ps_output(b"   \n  \n").unwrap().is_empty());
	}

	#[test]
	fn parse_ps_jsonl_one_per_line() {
		let stdout = br#"{"Service":"dev","State":"running","ExitCode":0,"Health":""}
{"Service":"mongo","State":"paused","ExitCode":0,"Health":""}
"#;
		let parsed = parse_ps_output(stdout).unwrap();
		assert_eq!(parsed, vec![svc("dev", "running"), svc("mongo", "paused")]);
	}

	#[test]
	fn parse_ps_array_form() {
		// Some docker compose versions emit a single JSON array
		// (`--format json` on older 2.x).
		let stdout = br#"[{"Service":"dev","State":"running","ExitCode":0,"Health":""},{"Service":"mongo","State":"exited","ExitCode":0,"Health":""}]"#;
		let parsed = parse_ps_output(stdout).unwrap();
		assert_eq!(parsed, vec![svc("dev", "running"), exited("mongo", 0)]);
	}

	#[test]
	fn parse_ps_captures_exit_code_and_health() {
		let stdout = br#"{"Service":"gitaly","State":"exited","ExitCode":1,"Health":""}
{"Service":"meilisearch","State":"running","ExitCode":0,"Health":"healthy"}
"#;
		let parsed = parse_ps_output(stdout).unwrap();
		assert_eq!(parsed[0].exit_code, 1);
		assert_eq!(parsed[1].health, "healthy");
	}

	#[test]
	fn parse_ps_tolerates_missing_optional_fields() {
		// Older compose versions may omit ExitCode / Health. The
		// `#[serde(default)]` on the struct should make us tolerate
		// that without erroring.
		let stdout = br#"{"Service":"dev","State":"running"}"#;
		let parsed = parse_ps_output(stdout).unwrap();
		assert_eq!(parsed, vec![svc("dev", "running")]);
	}

	#[test]
	fn parse_ps_tolerates_blank_lines() {
		let stdout = b"\n{\"Service\":\"dev\",\"State\":\"running\",\"ExitCode\":0,\"Health\":\"\"}\n\n";
		assert_eq!(parse_ps_output(stdout).unwrap(), vec![svc("dev", "running")]);
	}

	#[test]
	fn parse_ps_propagates_json_error() {
		let stdout = b"not json";
		assert!(parse_ps_output(stdout).is_err());
	}

	#[test]
	fn aggregate_no_services_is_absent() {
		assert_eq!(aggregate_state(&[]), ContainerState::Absent);
	}

	#[test]
	fn aggregate_all_running_is_running() {
		let services = [svc("dev", "running"), svc("mongo", "running")];
		assert_eq!(aggregate_state(&services), ContainerState::Running);
	}

	#[test]
	fn aggregate_any_paused_dominates() {
		let services = [svc("dev", "running"), svc("mongo", "paused")];
		assert_eq!(aggregate_state(&services), ContainerState::Paused);
	}

	#[test]
	fn aggregate_restarting_is_creating() {
		assert_eq!(
			aggregate_state(&[svc("dev", "running"), svc("mongo", "restarting")]),
			ContainerState::Creating,
		);
	}

	#[test]
	fn aggregate_paused_dominates_restarting() {
		assert_eq!(
			aggregate_state(&[svc("dev", "restarting"), svc("mongo", "paused")]),
			ContainerState::Paused,
		);
	}

	#[test]
	fn aggregate_dead_is_failed() {
		assert_eq!(
			aggregate_state(&[svc("dev", "running"), svc("mongo", "dead")]),
			ContainerState::Failed,
		);
	}

	#[test]
	fn aggregate_unknown_state_falls_to_failed() {
		assert_eq!(aggregate_state(&[svc("dev", "wat")]), ContainerState::Failed,);
	}

	#[test]
	fn aggregate_running_with_created_is_creating() {
		// Mid-startup snapshot: long-running upstreams (`dev`,
		// `redis`) are already up, downstreams (`cas`, `mongo`)
		// are still in `created` because their `depends_on`
		// chain hasn't resolved yet. Lean toward Creating —
		// flagging this as Failed would misfire on every
		// healthy compose up. Genuinely stuck downstreams
		// surface as `exited(non-zero)` upstream of them, which
		// the rule above already maps to Failed.
		let services = [
			svc("dev", "running"),
			svc("redis", "running"),
			svc("cas", "created"),
			svc("mongo", "created"),
		];
		assert_eq!(aggregate_state(&services), ContainerState::Creating);
	}

	#[test]
	fn aggregate_running_with_created_and_failed_init_is_failed() {
		// The diagnostic version: the same `running + created`
		// shape but with a failed init container upstream
		// (e.g. `cas-deps` exited 255, blocking `cas` from
		// ever starting). The non-zero exit is the unambiguous
		// "this project needs attention" signal.
		let services = [
			svc("dev", "running"),
			svc("redis", "running"),
			exited("cas-deps", 255),
			svc("cas", "created"),
		];
		assert_eq!(aggregate_state(&services), ContainerState::Failed);
	}

	#[test]
	fn aggregate_init_container_zero_exit_does_not_failbreak_running() {
		// One-shot init containers (e.g. `keyfile-generator`) are
		// expected to exit 0 alongside running long-running
		// services. That's a healthy `Running` state, not Failed.
		let services = [
			svc("dev", "running"),
			svc("redis", "running"),
			exited("keyfile-generator", 0),
		];
		assert_eq!(aggregate_state(&services), ContainerState::Running);
	}

	#[test]
	fn aggregate_nonzero_exit_is_failed() {
		// `gitaly` exited 1 → project is broken even if other
		// services are running.
		let services = [svc("dev", "running"), exited("gitaly", 1)];
		assert_eq!(aggregate_state(&services), ContainerState::Failed);
	}

	#[test]
	fn aggregate_all_exited_zero_is_stopped() {
		assert_eq!(
			aggregate_state(&[exited("dev", 0), exited("mongo", 0)]),
			ContainerState::Stopped,
		);
	}

	#[test]
	fn aggregate_signal_termination_exits_are_stopped_not_failed() {
		// After moon-ide's shutdown hook (or the user clicking
		// "Stop" in the popover) sends `compose stop`, JVMs and
		// long-running services typically exit 143 (SIGTERM)
		// or 137 (SIGKILL after the grace period). Treating
		// those as Failed leaves the project pinned to red
		// and breaks `auto_resume_shell`'s
		// `state == Stopped` precondition on next launch.
		let services = [
			exited("dev", 143),
			exited("mongo", 143),
			exited("gitaly", 137),
			exited("init-container", 0),
		];
		assert_eq!(aggregate_state(&services), ContainerState::Stopped);
	}

	#[test]
	fn aggregate_signal_termination_does_not_mask_real_failure() {
		// One service exited via SIGTERM (clean stop), another
		// crashed with code 1 (real application failure). The
		// real failure must still flip the project to Failed —
		// the signal-exit allowance only applies to "everyone
		// got cleanly stopped together".
		let services = [exited("dev", 143), exited("gitaly", 1)];
		assert_eq!(aggregate_state(&services), ContainerState::Failed);
	}

	#[test]
	fn aggregate_segfault_stays_failed() {
		// SIGSEGV (139) and SIGABRT (134) are crashes, not stop
		// signals — keep surfacing them as Failed.
		assert_eq!(
			aggregate_state(&[svc("dev", "running"), exited("worker", 139)]),
			ContainerState::Failed,
		);
		assert_eq!(
			aggregate_state(&[svc("dev", "running"), exited("worker", 134)]),
			ContainerState::Failed,
		);
	}

	#[test]
	fn aggregate_all_created_is_stopped() {
		// `compose create` (without `up`) puts every service in
		// `created` — the project exists but never started. Map
		// to Stopped so the user sees a clean "press Set up"
		// affordance rather than a forever-spinning "setting up".
		assert_eq!(
			aggregate_state(&[svc("dev", "created"), svc("mongo", "created")]),
			ContainerState::Stopped,
		);
	}

	#[test]
	fn aggregate_networkless_running_is_failed() {
		// A running container with no network endpoints is a
		// zombie — healthy-looking but unreachable. The glyph
		// must flip even though every raw state says "running".
		assert_eq!(
			aggregate_state(&[networkless("mongo"), svc("redis", "running")]),
			ContainerState::Failed,
		);
	}

	#[test]
	fn aggregate_paused_still_wins_over_networkless() {
		// Precedence: paused is the user's explicit choice and
		// stays the headline; the per-service flag carries the
		// detail.
		assert_eq!(
			aggregate_state(&[networkless("mongo"), svc("redis", "paused")]),
			ContainerState::Paused,
		);
	}

	#[test]
	fn workspace_handle_derives_paths_without_io() {
		let state_dir = Utf8PathBuf::from("/tmp/non-existent-moon-x");
		let ws = workspace_in(state_dir.clone(), vec![Utf8PathBuf::from("/tmp/folder")]);
		assert_eq!(ws.compose_path(), state_dir.join("compose.yaml"));
		assert_eq!(ws.bound_folders_path(), state_dir.join("bound-folders.json"));
		assert_eq!(ws.project().as_str(), "moon-ws-default");
		assert!(!ws.is_initialized());
	}

	#[tokio::test]
	async fn status_on_uninitialised_workspace_is_absent() {
		let tmp = tempfile::tempdir().unwrap();
		let state_dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let ws = workspace_in(state_dir, vec![]);
		let status = ws.status().await.unwrap();
		assert_eq!(status.state, ContainerState::Absent);
		assert!(status.services.is_empty());
	}

	#[tokio::test]
	async fn write_state_creates_state_dir_and_files() {
		let tmp = tempfile::tempdir().unwrap();
		// Pick a deeper state_dir to confirm `create_dir_all`
		// applies — the parent doesn't exist yet.
		let state_dir = Utf8PathBuf::from_path_buf(tmp.path().join("a").join("b")).unwrap();
		let folder = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let ws = workspace_in(state_dir.clone(), vec![folder.clone()]);

		let changed = ws.write_state(DEFAULT_DEV_IMAGE).await.unwrap();
		assert!(changed);
		assert!(ws.is_initialized());
		// Bound-folders sidecar lands alongside.
		let bound_json = tokio::fs::read_to_string(ws.bound_folders_path().as_std_path())
			.await
			.unwrap();
		assert!(bound_json.contains(folder.as_str()));
	}

	#[tokio::test]
	async fn write_state_is_idempotent_when_inputs_unchanged() {
		let tmp = tempfile::tempdir().unwrap();
		let state_dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let folder = Utf8PathBuf::from("/tmp/some/folder");
		let ws = workspace_in(state_dir, vec![folder]);

		assert!(ws.write_state(DEFAULT_DEV_IMAGE).await.unwrap());
		// Second call: same inputs, no change to bytes on disk
		// → returns `false` so callers can skip a `compose up`.
		assert!(!ws.write_state(DEFAULT_DEV_IMAGE).await.unwrap());
	}

	#[test]
	fn render_bound_folders_json_emits_minimal_structure() {
		let json = render_bound_folders_json(&[]);
		assert_eq!(json, "{\n  \"folders\": []\n}\n");

		let json = render_bound_folders_json(&[
			Utf8PathBuf::from("/home/me/code/moon-landing"),
			Utf8PathBuf::from("/home/me/code/moon-ide"),
		]);
		assert!(json.contains("\"/home/me/code/moon-landing\""));
		assert!(json.contains("\"/home/me/code/moon-ide\""));
		// Comma between the two entries, no trailing comma.
		let body_only = json.split("[").nth(1).unwrap();
		assert_eq!(body_only.matches(',').count(), 1);
	}

	#[test]
	fn mount_name_falls_back_to_dashed_path_for_root_input() {
		// `/` has no basename — we should still produce a
		// directory-name-shaped string rather than an empty mount
		// segment.
		let path = Utf8Path::new("/");
		assert!(!mount_name_for(path).is_empty());
	}

	#[test]
	fn invalid_workspace_id_is_a_typed_error() {
		let err = Workspace::new(WorkspaceConfig {
			workspace_id: "Invalid Id".into(),
			state_dir: Utf8PathBuf::from("/tmp/x"),
			bound_folders: vec![],
		})
		.unwrap_err();
		assert!(matches!(err, LifecycleError::InvalidWorkspaceId(_)));
	}

	// Real-Docker integration smoke. `--ignored` so `cargo test`
	// stays green on machines without a daemon. Run locally with
	// `cargo test -p moon-container -- --ignored container_smoke`.
	#[tokio::test]
	#[ignore = "requires a running Docker daemon and pulls alpine:latest"]
	async fn container_smoke_setup_pause_resume_teardown() {
		let tmp = tempfile::tempdir().unwrap();
		let state_dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let folder = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let ws = workspace_in(state_dir, vec![folder]);

		// alpine is 3 MB and on every developer's machine
		// already; `sleep infinity` keeps it up.
		ws.setup("alpine:latest").await.expect("setup");

		let st = ws.status().await.expect("status after setup");
		assert_eq!(st.state, ContainerState::Running, "{st:?}");

		ws.pause().await.expect("pause");
		assert_eq!(ws.status().await.unwrap().state, ContainerState::Paused);

		ws.resume().await.expect("resume");
		assert_eq!(ws.status().await.unwrap().state, ContainerState::Running);

		ws.teardown().await.expect("teardown");
		assert_eq!(ws.status().await.unwrap().state, ContainerState::Absent);
	}
}
