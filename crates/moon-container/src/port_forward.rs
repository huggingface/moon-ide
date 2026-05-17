//! Workspace port forwarding via a single shared proxy sidecar.
//!
//! The user declares N forwards per workspace (see
//! [`moon_protocol::ports`]); we serve them with one
//! `alpine/socat` container — `<workspace-project>-ports-1` —
//! attached to the workspace's default network and publishing
//! `127.0.0.1:<host_port>` for each declared forward. Adding or
//! removing a port = stop the sidecar and start a new one with
//! the new set; the dev container, terminals, and any in-flight
//! `bun dev` are untouched.
//!
//! Why a sidecar (not `--publish` on `dev`)
//! ---------------------------------------
//!
//! Compose treats `ports:` as part of the dev service's spec —
//! changing it forces a recreate of the dev container, which
//! drops every terminal session, every in-progress LSP, every
//! agent process. The whole point of "exposing a port" is that
//! the user is iterating on something running inside; recreating
//! the container kills that thing on every edit. The sidecar is
//! a separate container with its own lifecycle, so port edits
//! never touch `dev`.
//!
//! Why `alpine/socat`
//! -----------------
//!
//! `alpine/socat` is small (~5 MB), already on most developers'
//! machines, and starts in <100 ms. The proxy chain is the
//! cheapest possible: kernel TCP -> socat -> kernel TCP -> dev
//! container — no userspace HTTP parsing, no protocol awareness.
//! That makes the forward transparent to anything running inside
//! (websockets, raw TCP, server-sent events, all the way down to
//! `nc -lvk`).
//!
//! Idempotency and conflict handling
//! ---------------------------------
//!
//! [`apply_forwards`] always stops the existing sidecar (best
//! effort) before starting a fresh one. Pre-flight, we probe each
//! requested host port via `TcpListener::bind("127.0.0.1:N")` and
//! refuse to include conflicting entries in the new run, returning
//! them on [`PortsApplyResult::conflicts`]. Without the probe a
//! single port collision would have `docker run` fail opaquely
//! and the *whole* forward set go offline.
//!
//! What's not here
//! ---------------
//!
//! - Auto-detection of listening ports inside `dev`. Out of scope
//!   for the first cut — the user types the port. Adding `ss
//!   -ltn` polling later is straightforward.
//! - `0.0.0.0` toggle. Loopback-only is hardcoded; AGENTS.md
//!   "hardcode first, configure later" applies.
//! - devcontainer.json `forwardPorts` interop — Phase 2.3.

use std::net::TcpListener;

use moon_protocol::ports::{ForwardedPort, ForwardedPortHealth, ForwardedPortStatus, PortsApplyResult};
use tokio::process::Command;

use crate::lifecycle::LifecycleError;
use crate::network::{dev_container_name, project_default_network};
use crate::project::ProjectName;

/// Image used for the proxy sidecar.
///
/// `alpine/socat` is a tiny dedicated socat image that ships only
/// the binary on top of Alpine. Pinning a tag here so a future
/// `:latest` flip on the registry can't silently change the
/// command-line shape we depend on; bump on intent.
const PROXY_IMAGE: &str = "alpine/socat:1.8.0.3";

/// Container name for the workspace's proxy sidecar
/// (`<workspace-project>-ports-1`).
///
/// The trailing `-1` matches compose's per-service replica index
/// even though we don't run the sidecar through compose — it
/// keeps the proxy's name stylistically consistent with `dev-1`
/// and any future helper sidecars, and a `docker ps --filter
/// name=<workspace-project>-` enumeration of "everything this
/// workspace owns" stays uniform.
pub fn proxy_container_name(workspace_project: &ProjectName) -> String {
	format!("{}-ports-1", workspace_project.as_str())
}

/// Stop the current proxy sidecar (if any), then — if `forwards`
/// is non-empty — bring up a fresh one wired to the requested
/// set.
///
/// Pre-flight per requested host port: a transient
/// `TcpListener::bind("127.0.0.1:host_port")` probe rejects ports
/// already taken on the host. Conflicting entries are returned
/// on [`PortsApplyResult::conflicts`] and *omitted* from the
/// sidecar; the non-conflicting entries still come up. Callers
/// (the IPC layer) persist the user's full requested set to
/// session.json regardless — the conflict report drives a UI
/// dot, not the on-disk truth.
///
/// On `forwards.is_empty()`, we stop the sidecar and don't start
/// a new one.
pub async fn apply_forwards(
	workspace_project: &ProjectName,
	forwards: &[ForwardedPort],
) -> Result<PortsApplyResult, LifecycleError> {
	stop_forwards(workspace_project).await?;
	let (applied, conflicts) = partition_by_host_port_availability(forwards);
	if applied.is_empty() {
		return Ok(PortsApplyResult { applied, conflicts });
	}
	run_proxy_sidecar(workspace_project, &applied).await?;
	Ok(PortsApplyResult { applied, conflicts })
}

/// Force-stop the current proxy sidecar (if any). Idempotent —
/// "no such container" is treated as success so the IPC layer
/// can call this on workspace teardown without first checking
/// existence.
///
/// Used by:
///
/// - [`apply_forwards`] before recreating with a new set.
/// - Workspace `teardown` so we don't leak the sidecar across
///   `compose down` (the dev container is going away; the
///   proxy following it out is the right behaviour).
pub async fn stop_forwards(workspace_project: &ProjectName) -> Result<(), LifecycleError> {
	let name = proxy_container_name(workspace_project);
	let mut cmd = Command::new("docker");
	cmd.args(["rm", "-f", &name]);
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
	if stderr.contains("No such container") {
		return Ok(());
	}
	if stderr.contains("Cannot connect to the Docker daemon") {
		return Err(LifecycleError::DaemonUnreachable(stderr));
	}
	Err(LifecycleError::DockerCommandFailed {
		subcommand: format!("rm -f {name}"),
		code: output.status.code().unwrap_or(-1),
		stderr: stderr.trim().to_owned(),
	})
}

/// Cheap status read for the picker's per-row dot.
///
/// Order of evaluation per forward, picked so the most useful
/// signal wins:
///
/// 1. If the host port can be `bind`-probed (i.e. it's free on
///    the host) and the sidecar isn't bound on it, we're not
///    serving the forward — distinguishes between
///    [`ForwardedPortHealth::ProxyDown`] (sidecar absent) and
///    [`ForwardedPortHealth::HostPortBusy`] (something else
///    holds the port).
/// 2. Otherwise the proxy is bound and the user's connections
///    succeed — `Live`.
///
/// Doesn't probe the dev container itself: a forward that
/// reaches a stopped dev process is the *expected* behaviour
/// when the user spins their server up and down inside the
/// container ("the proxy is fine, your server is what's not
/// listening"). Folding that into health here would flap on
/// every restart of `bun dev`.
pub async fn list_status(
	workspace_project: &ProjectName,
	forwards: &[ForwardedPort],
) -> Result<Vec<ForwardedPortStatus>, LifecycleError> {
	let proxy_running = is_proxy_running(workspace_project).await?;
	let mut out = Vec::with_capacity(forwards.len());
	for forward in forwards {
		let health = if !proxy_running {
			if host_port_free(forward.host_port) {
				ForwardedPortHealth::ProxyDown
			} else {
				ForwardedPortHealth::HostPortBusy
			}
		} else {
			ForwardedPortHealth::Live
		};
		out.push(ForwardedPortStatus {
			forward: forward.clone(),
			health,
		});
	}
	Ok(out)
}

/// `docker inspect <name>` on the proxy container.
///
/// We don't filter by state — a paused or restarting proxy
/// counts as "running" for the picker's purposes. The richer
/// breakdown isn't useful here; the user just needs to know
/// whether the sidecar exists.
async fn is_proxy_running(workspace_project: &ProjectName) -> Result<bool, LifecycleError> {
	let name = proxy_container_name(workspace_project);
	let output = Command::new("docker")
		.args(["inspect", "-f", "{{.State.Running}}", &name])
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
		let stderr = String::from_utf8_lossy(&output.stderr);
		if stderr.contains("No such object") || stderr.contains("Error: No such object") {
			return Ok(false);
		}
		if stderr.contains("Cannot connect to the Docker daemon") {
			return Err(LifecycleError::DaemonUnreachable(stderr.into_owned()));
		}
		return Err(LifecycleError::DockerCommandFailed {
			subcommand: format!("inspect {name}"),
			code: output.status.code().unwrap_or(-1),
			stderr: stderr.trim().to_owned(),
		});
	}
	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(stdout.trim() == "true")
}

/// Partition the requested set into "host port available" and
/// "host port busy". Pre-flight only — between this probe and
/// `docker run` something else could grab the port, in which case
/// we surface the docker-run error to the caller as
/// `LifecycleError::DockerCommandFailed` and leave the conflict
/// list as it was.
fn partition_by_host_port_availability(forwards: &[ForwardedPort]) -> (Vec<ForwardedPort>, Vec<ForwardedPort>) {
	let mut applied = Vec::with_capacity(forwards.len());
	let mut conflicts = Vec::new();
	for forward in forwards {
		if host_port_free(forward.host_port) {
			applied.push(forward.clone());
		} else {
			conflicts.push(forward.clone());
		}
	}
	(applied, conflicts)
}

/// True iff `127.0.0.1:port` is free *right now*, on the host.
///
/// Bind/drop is a standard pre-flight idiom: bind reserves the
/// port for the lifetime of the listener, dropping it
/// immediately frees it for the very next `docker run -p`. The
/// race window is on the order of microseconds and lands us a
/// `DockerCommandFailed` if it does fire — which is the same
/// outcome we'd get without the probe, just with the conflict
/// invisible in the report.
fn host_port_free(port: u16) -> bool {
	TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// `docker run -d --rm --name <proxy> --network <net> -p ... alpine/socat sh -c '...'`
///
/// We use `sh -c` rather than `alpine/socat`'s default entrypoint
/// because we need to fan multiple `socat` listeners off a single
/// container — one per forward. The trailing `wait` blocks on
/// every backgrounded socat so the container exits when any of
/// them die (which would otherwise just orphan a half-broken
/// proxy).
async fn run_proxy_sidecar(workspace_project: &ProjectName, forwards: &[ForwardedPort]) -> Result<(), LifecycleError> {
	let name = proxy_container_name(workspace_project);
	let network = project_default_network(workspace_project);
	let dev = dev_container_name(workspace_project);

	let mut cmd = Command::new("docker");
	cmd.args(["run", "-d", "--rm"]);
	cmd.args(["--name", &name]);
	cmd.args(["--network", &network]);
	for forward in forwards {
		cmd.arg("-p");
		cmd.arg(format!("127.0.0.1:{}:{}", forward.host_port, forward.container_port));
	}
	cmd.args(["--entrypoint", "/bin/sh"]);
	cmd.arg(PROXY_IMAGE);
	cmd.arg("-c");
	cmd.arg(build_socat_command(&dev, forwards));

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
	if stderr.contains("Cannot connect to the Docker daemon") {
		return Err(LifecycleError::DaemonUnreachable(stderr));
	}
	Err(LifecycleError::DockerCommandFailed {
		subcommand: format!("run {name}"),
		code: output.status.code().unwrap_or(-1),
		stderr: stderr.trim().to_owned(),
	})
}

/// Build the `sh -c '...'` body that fans N `socat` listeners
/// inside the sidecar, all targeting `<dev>:<container_port>`.
///
/// `tcp-listen:<port>,fork,reuseaddr` — `fork` so each accepted
/// connection gets its own child (concurrent clients work),
/// `reuseaddr` so a quick stop / start cycle doesn't trip
/// `TIME_WAIT`. Targets use the dev container's compose name
/// directly; both containers are on the same network, so the
/// daemon's embedded DNS resolves it.
fn build_socat_command(dev: &str, forwards: &[ForwardedPort]) -> String {
	let mut parts = Vec::with_capacity(forwards.len());
	for forward in forwards {
		parts.push(format!(
			"socat tcp-listen:{port},fork,reuseaddr tcp:{dev}:{port}",
			port = forward.container_port,
			dev = dev,
		));
	}
	parts.push("wait".to_owned());
	parts.join(" & ")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::project::project_name_for_id;

	#[test]
	fn proxy_container_name_uses_project_with_ports_1_suffix() {
		let project = project_name_for_id("default").unwrap();
		assert_eq!(proxy_container_name(&project), "moon-ws-default-ports-1");
		let project = project_name_for_id("foo-bar").unwrap();
		assert_eq!(proxy_container_name(&project), "moon-ws-foo-bar-ports-1");
	}

	#[test]
	fn build_socat_command_chains_listeners_with_amp_and_wait() {
		let forwards = vec![
			ForwardedPort {
				container_port: 3000,
				host_port: 3000,
				label: "vite".into(),
			},
			ForwardedPort {
				container_port: 8080,
				host_port: 8080,
				label: "api".into(),
			},
		];
		let cmd = build_socat_command("moon-ws-default-dev-1", &forwards);
		assert!(cmd.contains("socat tcp-listen:3000,fork,reuseaddr tcp:moon-ws-default-dev-1:3000"));
		assert!(cmd.contains("socat tcp-listen:8080,fork,reuseaddr tcp:moon-ws-default-dev-1:8080"));
		assert!(cmd.ends_with("& wait"));
	}

	#[test]
	fn build_socat_command_with_one_forward_still_has_wait() {
		let forwards = vec![ForwardedPort {
			container_port: 3000,
			host_port: 4000,
			label: String::new(),
		}];
		let cmd = build_socat_command("dev", &forwards);
		assert_eq!(cmd, "socat tcp-listen:3000,fork,reuseaddr tcp:dev:3000 & wait");
	}

	#[test]
	fn host_port_free_detects_bound_port() {
		// Bind the listener on an OS-picked free port; while it's
		// alive the probe must report "busy". (We deliberately
		// don't drop-then-reprobe — Linux holds the address in
		// `TIME_WAIT` for a beat, which would flake the probe;
		// `partition_separates_busy_and_free_ports` covers the
		// "free" half via a fresh OS-picked port instead.)
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		assert!(!host_port_free(port));
	}

	#[test]
	fn partition_separates_busy_and_free_ports() {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let busy = listener.local_addr().unwrap().port();
		// Pick a free port by binding-and-dropping a second
		// listener; the kernel won't immediately reuse it.
		let probe = TcpListener::bind("127.0.0.1:0").unwrap();
		let free = probe.local_addr().unwrap().port();
		drop(probe);

		let forwards = vec![
			ForwardedPort {
				container_port: 3000,
				host_port: free,
				label: "ok".into(),
			},
			ForwardedPort {
				container_port: 3001,
				host_port: busy,
				label: "conflict".into(),
			},
		];
		let (applied, conflicts) = partition_by_host_port_availability(&forwards);
		assert_eq!(applied.len(), 1);
		assert_eq!(applied[0].host_port, free);
		assert_eq!(conflicts.len(), 1);
		assert_eq!(conflicts[0].host_port, busy);
	}
}
