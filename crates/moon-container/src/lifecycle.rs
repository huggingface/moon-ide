//! Workspace container lifecycle — the layer that actually shells
//! out to `docker compose`.
//!
//! Everything in this module is built around a single
//! [`Workspace`] handle that captures *the same three things every
//! `docker compose` invocation needs*:
//!
//! - the workspace root (so we can discover sibling compose files
//!   the first time we generate `.moon/compose.yaml`),
//! - the compose project name (`moon-ws-<hash>` — see
//!   [`crate::project`]),
//! - the absolute path to `<root>/.moon/compose.yaml`.
//!
//! From those, every command is `docker compose -f <path> -p
//! <name> <subcommand>`. Both flags are always set explicitly so
//! the working-directory of the spawned process never matters,
//! and so `docker compose` doesn't accidentally pick up a
//! different default project name from a `.env` file.
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
//!   channel for the UI lands with the Tauri-events commit.
//! - **Cancellation.** A `setup` mid-flight that the user wants
//!   to abort — also Tauri-layer, so it can be wired to the
//!   status pip's "Cancel" button.
//! - **Per-service operations.** 2.4's per-service start/stop/
//!   restart UI surface; not relevant until the UI exists.

use std::ffi::OsStr;

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::container::{ContainerState, ContainerStatus, ServiceStatus};
use moon_protocol::MoonError;
use thiserror::Error;
use tokio::process::Command;

use crate::compose::{generate_compose, ComposeRender, ComposeRenderOptions};
use crate::discovery::discover_compose_files;
use crate::project::{project_name_for, ProjectName};

/// The image reference written into a freshly generated
/// `.moon/compose.yaml` if the caller doesn't override it.
///
/// Currently a *local* tag (`moon-base:dev`) because moon-base
/// hasn't been published yet — see ADR 0007. Once the GitHub
/// Actions workflow ships its first image to Docker Hub this
/// flips to `huggingface/moon-base:0.1` (or similar). The default
/// is intentionally a single-source constant rather than a config
/// knob: per ADR 0005, "what image does a fresh moon-ide
/// workspace build against?" is a release-time decision, not a
/// per-user preference.
pub const DEFAULT_DEV_IMAGE: &str = "moon-base:dev";

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

	#[error("could not parse `docker compose ps` output: {0}")]
	ParseError(String),

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
			LifecycleError::ParseError(msg) => MoonError::internal(format!("could not parse docker compose output: {msg}")),
			LifecycleError::Io(err) => MoonError::from(err),
		}
	}
}

/// Handle on a workspace's compose project. Cheap to construct
/// (no I/O), cheap to clone (`Arc`-able strings under the hood).
#[derive(Debug, Clone)]
pub struct Workspace {
	root: Utf8PathBuf,
	project: ProjectName,
	compose_path: Utf8PathBuf,
}

impl Workspace {
	/// Construct a handle for the workspace rooted at `root`.
	/// Doesn't touch disk; safe to call before the workspace has
	/// been "set up".
	pub fn for_root(root: Utf8PathBuf) -> Self {
		let project = project_name_for(&root);
		let compose_path = root.join(".moon").join("compose.yaml");
		Self {
			root,
			project,
			compose_path,
		}
	}

	pub fn root(&self) -> &Utf8Path {
		&self.root
	}

	pub fn project(&self) -> &ProjectName {
		&self.project
	}

	pub fn compose_path(&self) -> &Utf8Path {
		&self.compose_path
	}

	/// True iff `<root>/.moon/compose.yaml` exists. Doesn't say
	/// anything about whether the containers are up.
	pub fn is_initialized(&self) -> bool {
		self.compose_path.is_file()
	}

	/// Render what `<root>/.moon/compose.yaml` *would* look like
	/// if we generated it right now. Useful for an "Inspect"
	/// affordance before the user clicks "Set up".
	pub fn render_compose(&self, dev_image: &str) -> ComposeRender {
		let discovery = discover_compose_files(&self.root);
		let includes: Vec<&Utf8Path> = discovery.files.iter().map(|f| f.relative_path.as_path()).collect();
		generate_compose(ComposeRenderOptions {
			project: &self.project,
			dev_image,
			include_files: &includes,
		})
	}

	/// Generate `.moon/compose.yaml` if it doesn't already exist.
	/// Returns `Ok(true)` if the file was written this call,
	/// `Ok(false)` if it was already present.
	pub async fn write_compose_if_missing(&self, dev_image: &str) -> Result<bool, LifecycleError> {
		if self.compose_path.is_file() {
			return Ok(false);
		}
		let render = self.render_compose(dev_image);
		if let Some(parent) = self.compose_path.parent() {
			tokio::fs::create_dir_all(parent.as_std_path()).await?;
		}
		tokio::fs::write(self.compose_path.as_std_path(), render.yaml).await?;
		Ok(true)
	}

	/// Snapshot the compose project's state.
	///
	/// `Absent` is returned without invoking docker if there's no
	/// `.moon/compose.yaml` yet — opening a fresh workspace is
	/// the common case and we don't want to pay a `docker compose`
	/// invocation per open just to confirm "no, still nothing".
	pub async fn status(&self) -> Result<ContainerStatus, LifecycleError> {
		if !self.compose_path.is_file() {
			return Ok(ContainerStatus {
				state: ContainerState::Absent,
				services: Vec::new(),
			});
		}
		let output = self.docker_compose(["ps", "--all", "--format", "json"]).await?;
		let services = parse_ps_output(&output.stdout).map_err(LifecycleError::ParseError)?;
		let state = aggregate_state(&services);
		Ok(ContainerStatus { state, services })
	}

	/// First-time opt-in: generate `.moon/compose.yaml` if
	/// missing, then `docker compose up -d --wait` so we don't
	/// return until everything is healthy (or has failed).
	pub async fn setup(&self, dev_image: &str) -> Result<(), LifecycleError> {
		self.write_compose_if_missing(dev_image).await?;
		self.docker_compose(["up", "-d", "--wait"]).await?;
		Ok(())
	}

	/// Pause every container in the project. Idempotency: if
	/// some are already paused, `docker compose pause` errors —
	/// callers should check [`Workspace::status`] first.
	pub async fn pause(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["pause"]).await?;
		Ok(())
	}

	/// Inverse of [`Workspace::pause`].
	pub async fn resume(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["unpause"]).await?;
		Ok(())
	}

	/// Force-recreate every container, pulling fresh images
	/// first. The hammer: use this when the moon-base reference
	/// changed, when an included compose changed in a way `up`
	/// didn't pick up, or when the user just wants to start
	/// over.
	pub async fn rebuild(&self) -> Result<(), LifecycleError> {
		self
			.docker_compose(["up", "-d", "--force-recreate", "--pull", "always", "--wait"])
			.await?;
		Ok(())
	}

	/// `docker compose down` — stop and remove containers,
	/// networks, and the project entry. The compose file itself
	/// stays on disk; the next `setup` resurrects from there.
	pub async fn teardown(&self) -> Result<(), LifecycleError> {
		self.docker_compose(["down"]).await?;
		Ok(())
	}

	async fn docker_compose<I, S>(&self, args: I) -> Result<DockerOutput, LifecycleError>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		let mut cmd = Command::new("docker");
		cmd.arg("compose");
		cmd.arg("-f").arg(self.compose_path.as_std_path());
		cmd.arg("-p").arg(self.project.as_str());
		for arg in args {
			cmd.arg(arg);
		}

		tracing::debug!(
			project = %self.project,
			compose = %self.compose_path,
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
			// Surfaced verbatim from the engine; the substring is
			// stable across recent Docker versions and lets the
			// UI distinguish "Docker is installed but the daemon
			// isn't running" from a genuine compose error.
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
}

struct DockerOutput {
	stdout: Vec<u8>,
}

// Pure helpers — extracted so the parsing + aggregation logic is
// testable without a Docker daemon.

/// Parse the stdout of `docker compose ps --format json`.
///
/// Compose's output format has shifted between versions: some
/// emit JSON-Lines (one container object per line), others emit
/// a single JSON array. We accept both.
fn parse_ps_output(stdout: &[u8]) -> Result<Vec<ServiceStatus>, String> {
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
}

impl From<PsEntry> for ServiceStatus {
	fn from(entry: PsEntry) -> Self {
		ServiceStatus {
			name: entry.service,
			raw_state: entry.state,
		}
	}
}

/// Roll the per-service state list up into a single
/// [`ContainerState`] for the status pip.
///
/// Precedence (highest wins):
///
/// 1. `Paused` — any container paused.
/// 2. `Creating` — any container in `created`/`restarting`.
/// 3. `Failed` — any container `dead` or in an unrecognised
///    state.
/// 4. `Running` — at least one running and none of the above.
/// 5. `Stopped` — every container `exited`.
/// 6. `Absent` — no containers reported.
fn aggregate_state(services: &[ServiceStatus]) -> ContainerState {
	if services.is_empty() {
		return ContainerState::Absent;
	}
	let mut any_paused = false;
	let mut any_running = false;
	let mut any_creating = false;
	let mut any_failed = false;
	let mut any_exited = false;
	for svc in services {
		match svc.raw_state.as_str() {
			"paused" => any_paused = true,
			"running" => any_running = true,
			"created" | "restarting" => any_creating = true,
			"dead" => any_failed = true,
			"exited" => any_exited = true,
			// Anything we don't recognise is treated as
			// "something's wrong" rather than silently dropped.
			_ => any_failed = true,
		}
	}
	if any_paused {
		return ContainerState::Paused;
	}
	if any_creating {
		return ContainerState::Creating;
	}
	if any_failed {
		return ContainerState::Failed;
	}
	if any_running {
		return ContainerState::Running;
	}
	if any_exited {
		return ContainerState::Stopped;
	}
	// Unreachable in practice: the loop covers every observed
	// state and all unknowns flip `any_failed`. Defaulting to
	// `Failed` here is the safe choice if a future Docker
	// version adds a new status string.
	ContainerState::Failed
}

#[cfg(test)]
mod tests {
	use super::*;

	fn svc(name: &str, raw: &str) -> ServiceStatus {
		ServiceStatus {
			name: name.into(),
			raw_state: raw.into(),
		}
	}

	#[test]
	fn parse_ps_empty_stdout_is_empty_list() {
		assert!(parse_ps_output(b"").unwrap().is_empty());
		assert!(parse_ps_output(b"   \n  \n").unwrap().is_empty());
	}

	#[test]
	fn parse_ps_jsonl_one_per_line() {
		let stdout = br#"{"Service":"dev","State":"running"}
{"Service":"mongo","State":"paused"}
"#;
		let parsed = parse_ps_output(stdout).unwrap();
		assert_eq!(parsed, vec![svc("dev", "running"), svc("mongo", "paused")]);
	}

	#[test]
	fn parse_ps_array_form() {
		// Some docker compose versions emit a single JSON array
		// (`--format json` on older 2.x).
		let stdout = br#"[{"Service":"dev","State":"running"},{"Service":"mongo","State":"exited"}]"#;
		let parsed = parse_ps_output(stdout).unwrap();
		assert_eq!(parsed, vec![svc("dev", "running"), svc("mongo", "exited")]);
	}

	#[test]
	fn parse_ps_tolerates_blank_lines() {
		let stdout = b"\n{\"Service\":\"dev\",\"State\":\"running\"}\n\n";
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
		// A single paused service flips the whole project to
		// paused — matches the UX spec ("the project is paused
		// when any container is paused").
		let services = [svc("dev", "running"), svc("mongo", "paused")];
		assert_eq!(aggregate_state(&services), ContainerState::Paused);
	}

	#[test]
	fn aggregate_creating_beats_running_but_not_paused() {
		assert_eq!(
			aggregate_state(&[svc("dev", "created"), svc("mongo", "running")]),
			ContainerState::Creating,
		);
		assert_eq!(
			aggregate_state(&[svc("dev", "created"), svc("mongo", "paused")]),
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
		// Forward-compat: a future Docker version inventing a
		// new status shouldn't show up as a happy "running".
		assert_eq!(aggregate_state(&[svc("dev", "wat")]), ContainerState::Failed,);
	}

	#[test]
	fn aggregate_all_exited_is_stopped() {
		assert_eq!(
			aggregate_state(&[svc("dev", "exited"), svc("mongo", "exited")]),
			ContainerState::Stopped,
		);
	}

	#[test]
	fn workspace_handle_derives_paths_without_io() {
		let root = Utf8PathBuf::from("/tmp/non-existent-moon-x");
		let ws = Workspace::for_root(root.clone());
		assert_eq!(ws.root(), &root);
		assert_eq!(ws.compose_path(), root.join(".moon/compose.yaml"));
		assert!(ws.project().as_str().starts_with("moon-ws-"));
		assert!(!ws.is_initialized());
	}

	#[tokio::test]
	async fn status_on_uninitialised_workspace_is_absent() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let ws = Workspace::for_root(root);
		let status = ws.status().await.unwrap();
		assert_eq!(status.state, ContainerState::Absent);
		assert!(status.services.is_empty());
	}

	#[tokio::test]
	async fn write_compose_if_missing_is_idempotent() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let ws = Workspace::for_root(root);

		assert!(ws.write_compose_if_missing(DEFAULT_DEV_IMAGE).await.unwrap());
		assert!(ws.is_initialized());

		// Second call doesn't rewrite — caller-driven state, not
		// "regenerate on every status check".
		assert!(!ws.write_compose_if_missing(DEFAULT_DEV_IMAGE).await.unwrap());
	}

	// Real-Docker integration smoke. `--ignored` so `cargo test`
	// stays green on machines without a daemon. Run locally with
	// `cargo test -p moon-container -- --ignored container_smoke`.
	#[tokio::test]
	#[ignore = "requires a running Docker daemon and pulls alpine:latest"]
	async fn container_smoke_setup_pause_resume_teardown() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let ws = Workspace::for_root(root);

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
