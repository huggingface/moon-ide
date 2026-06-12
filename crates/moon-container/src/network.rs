//! Cross-project network attachment for the workspace shell.
//!
//! Per [ADR 0008] the workspace shell ([`crate::lifecycle::Workspace`])
//! and per-folder project services ([`crate::project_compose`])
//! run as **separate compose projects**, each with their own
//! default network. To let `mongosh mongodb://mongo:27017` work
//! from a workspace terminal — i.e. for the dev container to
//! resolve a project service by its compose service name — we
//! attach the dev container to the project's default network on
//! `up` (and re-attach on workspace setup / rebuild) and detach
//! on `down`.
//!
//! Why dynamic attach, not a shared external network
//! -------------------------------------------------
//!
//! A shared external network (with `external: true` declared in
//! both compose files) would require either modifying the user's
//! project compose on disk or layering a generated override file
//! on top — both break the design rule that the user's
//! `docker-compose.yml` runs unmodified (see
//! [`crate::project_compose`] § "Path strategy"). Attaching the
//! dev container post-`up` is a pure daemon-side operation; the
//! file system stays clean, and a project that's never been
//! brought up never has a network for us to attach to anyway.
//!
//! What we attach to
//! -----------------
//!
//! Only the project's **default** network (`<project>_default`),
//! the one compose creates implicitly when a `docker-compose.yml`
//! doesn't declare a top-level `networks:` block of its own. A
//! project that opts into multi-network topologies (e.g. one
//! network per service tier) won't get every service reachable
//! by name from the dev container; the user explicitly designed
//! that segmentation, and we surface the limitation in
//! `specs/containers.md` rather than silently fan out across
//! every project network. Single-network is the common case for
//! the kind of side-services compose stacks teams ship (a
//! database + a cache + maybe a worker) and that's what this
//! affordance is for.
//!
//! Idempotency
//! -----------
//!
//! `connect_container_to_network` and
//! `disconnect_container_from_network` both treat the
//! "already attached" / "not attached" stderr replies as
//! success, so callers don't have to predicate on the current
//! attachment state. Two callers racing to attach the same
//! container at the same time both succeed.
//!
//! [ADR 0008]: ../../specs/decisions/0008-host-shared-daemon.md

use std::collections::BTreeSet;

use tokio::process::Command;

use crate::lifecycle::LifecycleError;
use crate::project::ProjectName;

/// Default network name compose creates for `project`
/// (`<project>_default`).
pub fn project_default_network(project: &ProjectName) -> String {
	format!("{}_default", project.as_str())
}

/// Container name compose creates for the workspace's `dev`
/// service: `<workspace-project>-dev-1`. The trailing `1` is
/// compose's per-service replica index; `dev` is single-replica
/// today (`x-moon.shell-service`), so the index is always `1`.
///
/// Mirrors `moon_terminal::container_name_for_workspace`'s
/// format. We deliberately don't share the helper across crates
/// — `moon-container` is the lower layer and shouldn't depend on
/// `moon-terminal`. The format is one line; the test suite pins
/// it on both sides so a divergent rename can't slip through
/// silently.
pub fn dev_container_name(workspace_project: &ProjectName) -> String {
	format!("{}-dev-1", workspace_project.as_str())
}

/// `docker network connect <network> <container>`.
///
/// Idempotent: a stderr reply that signals "already attached"
/// resolves to `Ok(())`. Other failures (network missing,
/// container missing, daemon unreachable) bubble up so the caller
/// can decide whether to log + continue or surface to the user.
pub async fn connect_container_to_network(network: &str, container: &str) -> Result<(), LifecycleError> {
	run_docker_network(["connect", network, container]).await
}

/// `docker network disconnect <network> <container>`.
///
/// Idempotent: "not connected" / "no such network" / "no such
/// container" resolve to `Ok(())`. Used before `down` so compose
/// can remove the project network without "has active endpoints"
/// errors, and during workspace-shell teardown so leftover
/// attachments don't pin networks for projects that have already
/// gone away.
pub async fn disconnect_container_from_network(network: &str, container: &str) -> Result<(), LifecycleError> {
	run_docker_network(["disconnect", network, container]).await
}

/// Internal: spawn `docker network <op> <network> <container>`
/// and tolerate the idempotency-relevant stderr patterns. Single
/// path so connect / disconnect parsing rules stay consistent.
async fn run_docker_network<'a, I>(args: I) -> Result<(), LifecycleError>
where
	I: IntoIterator<Item = &'a str>,
{
	let mut cmd = Command::new("docker");
	cmd.arg("network");
	let mut argv: Vec<String> = vec!["network".to_owned()];
	for arg in args {
		cmd.arg(arg);
		argv.push(arg.to_owned());
	}
	let output = cmd.output().await.map_err(|err| {
		if err.kind() == std::io::ErrorKind::NotFound {
			LifecycleError::DockerMissing
		} else {
			LifecycleError::Io(err)
		}
	})?;
	if output.status.success() {
		return Ok(());
	}
	let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
	if is_already_attached(&stderr) || is_not_attached(&stderr) {
		tracing::debug!(args = ?argv, %stderr, "docker network op tolerated as no-op");
		return Ok(());
	}
	if stderr.contains("Cannot connect to the Docker daemon") {
		return Err(LifecycleError::DaemonUnreachable(stderr));
	}
	Err(LifecycleError::DockerCommandFailed {
		subcommand: argv.join(" "),
		code: output.status.code().unwrap_or(-1),
		stderr: stderr.trim().to_owned(),
	})
}

/// Compose services of `project` whose container is up but
/// attached to **no** network at all.
///
/// That state is never legitimate — even `network_mode: host` /
/// `none` report `host` / `none` in `docker ps`'s `Networks`
/// column. It's the residue of a failed `docker start`: when the
/// daemon aborts startup mid-way (the classic trigger is a
/// host-port conflict, "Bind for 0.0.0.0:27017 failed: port is
/// already allocated"), its rollback wipes the container's
/// stored endpoint config. Every later plain `start` then
/// "succeeds" with only a loopback interface, no service-name
/// DNS, and no published ports — a zombie that looks healthy as
/// long as its healthcheck talks to `127.0.0.1`. Only a
/// recreate rebuilds the endpoints; see
/// `ProjectCompose::heal_networkless_services`.
///
/// One `docker ps` round-trip (running containers only — a
/// stopped container holds no endpoints, so the question is
/// meaningless for it). Returns a sorted, deduplicated set of
/// compose service names.
pub(crate) async fn networkless_running_services(project: &ProjectName) -> Result<BTreeSet<String>, LifecycleError> {
	let filter = format!("label=com.docker.compose.project={}", project.as_str());
	let output = Command::new("docker")
		.args([
			"ps",
			"--filter",
			&filter,
			"--format",
			"{{.Label \"com.docker.compose.service\"}}\t{{.Networks}}",
		])
		.output()
		.await
		.map_err(|err| {
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
		return Err(LifecycleError::DockerCommandFailed {
			subcommand: "ps".to_owned(),
			code: output.status.code().unwrap_or(-1),
			stderr: stderr.trim().to_owned(),
		});
	}
	Ok(parse_networkless_services(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse the `<service>\t<networks>` lines emitted by the
/// `networkless_running_services` probe. Split out for testing.
fn parse_networkless_services(stdout: &str) -> BTreeSet<String> {
	let mut out = BTreeSet::new();
	for line in stdout.lines() {
		let Some((service, networks)) = line.split_once('\t') else {
			continue;
		};
		let service = service.trim();
		if !service.is_empty() && networks.trim().is_empty() {
			out.insert(service.to_owned());
		}
	}
	out
}

/// "container is already attached to this network" — the wording
/// the daemon emits varies a bit across Docker / Podman versions
/// (Docker 24+: "endpoint with name … already exists in network …";
/// older: "Endpoint already exists"). Match both rather than pin
/// to one phrasing.
fn is_already_attached(stderr: &str) -> bool {
	stderr.contains("already exists in network") || stderr.contains("Endpoint already exists")
}

/// "container is not attached to this network", or one of its
/// parts went away first. We include the "no such" paths because
/// the ProjectCompose down flow runs disconnect *before* compose
/// removes the network, but a previous failed run could have
/// left things in any order — treating all four as success keeps
/// the "best-effort cleanup" promise.
fn is_not_attached(stderr: &str) -> bool {
	stderr.contains("is not connected to network")
		|| stderr.contains("is not connected to the network")
		|| stderr.contains("No such network")
		|| stderr.contains("No such container")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::project::project_name_for_id;

	#[test]
	fn default_network_uses_project_name_with_default_suffix() {
		let project = project_name_for_id("default").unwrap();
		assert_eq!(project_default_network(&project), "moon-ws-default_default");
	}

	#[test]
	fn dev_container_name_uses_project_with_dev_1_suffix() {
		// Pin both formats here and in `moon_terminal::target` so
		// a rename in either crate trips a test rather than
		// silently desyncing. See doc comment on
		// `dev_container_name` for why we duplicate the format.
		let project = project_name_for_id("default").unwrap();
		assert_eq!(dev_container_name(&project), "moon-ws-default-dev-1");
		let project = project_name_for_id("foo-bar").unwrap();
		assert_eq!(dev_container_name(&project), "moon-ws-foo-bar-dev-1");
	}

	#[test]
	fn already_attached_phrasings_accepted() {
		assert!(is_already_attached(
			"Error response from daemon: endpoint with name moon-ws-default-dev-1 already exists in network mynet"
		));
		assert!(is_already_attached("Endpoint already exists"));
		assert!(!is_already_attached("Error: no such network: mynet"));
	}

	#[test]
	fn networkless_probe_flags_only_empty_network_columns() {
		let stdout = "mongo\t\nredis\tmoon-ws-default-app_default\nproxy\thost\nbatch\tnone\nmulti\tneta,netb\n";
		let got = parse_networkless_services(stdout);
		assert_eq!(got.into_iter().collect::<Vec<_>>(), vec!["mongo".to_owned()]);
	}

	#[test]
	fn networkless_probe_tolerates_blank_and_malformed_lines() {
		// No tab at all (docker changed its format?) and fully
		// blank lines must not panic or produce phantom services.
		let got = parse_networkless_services("\nmongo\n\t\nweird line without tab\n");
		assert!(got.is_empty());
	}

	#[test]
	fn networkless_probe_dedupes_replicas() {
		// Two replicas of the same compose service, both broken,
		// heal with a single `up --force-recreate <svc>`.
		let got = parse_networkless_services("worker\t\nworker\t\n");
		assert_eq!(got.len(), 1);
	}

	#[test]
	fn not_attached_phrasings_accepted() {
		assert!(is_not_attached("Error: container foo is not connected to network bar"));
		assert!(is_not_attached(
			"Error: container foo is not connected to the network bar"
		));
		assert!(is_not_attached("Error: No such network: mynet"));
		assert!(is_not_attached("Error: No such container: foo"));
		assert!(!is_not_attached("Error: random failure"));
	}
}
