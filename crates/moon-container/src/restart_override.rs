//! Per-folder compose **restart-policy override**.
//!
//! Project compose files we discover in bound folders typically
//! ship with `restart: always` (moon-landing, infra repos, etc.) —
//! correct for the original project's own developers and CI, wrong
//! when moon-ide is the lifecycle owner. The IDE issues `docker
//! compose stop` on its own quit and persists an auto-resume
//! snapshot; if the daemon then respawns the containers behind
//! us, the snapshot lies and the user sees ghost services they
//! didn't ask for.
//!
//! We don't want to edit the user's compose file. Instead, we
//! generate a tiny sibling override file (`<state-dir>/project-
//! overrides/<slug>.yaml`) that sets `restart: "no"` for every
//! service declared by the user's compose, and pass it as a
//! second `-f` to every `docker compose` invocation for that
//! folder. Compose merges the two so the override's
//! `restart: "no"` *replaces* the base policy on each service —
//! same as if the user had run with `-f base.yml -f override.yml`
//! by hand.
//!
//! Why the runtime `docker compose config --services` round-trip
//! ----------------------------------------------------------
//!
//! Hand-parsing YAML to enumerate services is fragile: the user's
//! compose can use `extends:`, `include:`, anchors, env
//! interpolation, multiple compose files merged via the
//! `COMPOSE_FILE` env, etc. `docker compose config --services`
//! is the same resolver compose itself runs at `up` time, so we
//! get the exact service list compose would create — no
//! reimplementation drift. The call is local to dockerd and
//! finishes in ~100 ms; we run it once per `up`/`rebuild` /
//! `start_service` invocation, not on every mouse-over.
//!
//! Why we render the override by hand
//! ----------------------------------
//!
//! Same reasoning as [`crate::compose`]: the file is small +
//! fixed-shape, we never read it back ourselves, and `serde_yaml`
//! has been unmaintained since 2023. Two-line entry per service,
//! double-quoted `"no"` so YAML doesn't coerce it to the boolean
//! `false`.

use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use tokio::process::Command;

use crate::lifecycle::LifecycleError;

/// Subdirectory under a workspace's state dir where per-folder
/// restart overrides are cached.
pub const PROJECT_OVERRIDES_DIR: &str = "project-overrides";

/// Resolve the override file path for `slug` under `state_dir`,
/// without touching disk.
///
/// Pure path math so callers (including tests) can predict where
/// the override will land before any I/O happens.
pub fn override_path(state_dir: &Utf8Path, slug: &str) -> Utf8PathBuf {
	state_dir.join(PROJECT_OVERRIDES_DIR).join(format!("{slug}.yaml"))
}

/// Generate (or refresh) the restart-policy override for a
/// per-folder compose project.
///
/// 1. Asks compose itself for the service list of
///    `user_compose_file` via `docker compose config --services`.
/// 2. Renders a sibling YAML file at
///    `<state_dir>/project-overrides/<slug>.yaml` setting
///    `restart: "no"` for each of those services.
///
/// Returns the absolute path to the override file. Callers pass
/// it as a second `-f` after the user's compose file so compose
/// merges the two — the override's `restart: "no"` wins, anything
/// else stays untouched.
///
/// Idempotent: rewriting the same content is a no-op from the
/// daemon's perspective, and the file ends up byte-identical if
/// the service set hasn't changed.
pub async fn ensure_restart_override(
	state_dir: &Utf8Path,
	slug: &str,
	user_compose_file: &Utf8Path,
) -> Result<Utf8PathBuf, LifecycleError> {
	let services = list_services(user_compose_file).await?;
	let path = override_path(state_dir, slug);
	let body = render_override(&services);

	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent.as_std_path()).await?;
	}
	tokio::fs::write(path.as_std_path(), body.as_bytes()).await?;
	Ok(path)
}

/// `docker compose -f <user_compose_file> config --services`.
///
/// No `-p` here: we don't want the project-name-on-the-daemon
/// machinery (no containers are created), we just want compose to
/// resolve the service list it would create at `up` time.
async fn list_services(user_compose_file: &Utf8Path) -> Result<Vec<String>, LifecycleError> {
	let mut cmd = Command::new("docker");
	cmd.arg("compose");
	cmd.arg("-f").arg(user_compose_file.as_std_path());
	cmd.arg("config");
	cmd.arg("--services");

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

	let stdout = String::from_utf8_lossy(&output.stdout);
	Ok(parse_services_output(&stdout))
}

/// One service name per line; blank lines and leading/trailing
/// whitespace are tolerated. `docker compose config --services`
/// emits in compose-declared order, which we preserve.
fn parse_services_output(stdout: &str) -> Vec<String> {
	stdout
		.lines()
		.map(str::trim)
		.filter(|s| !s.is_empty())
		.map(str::to_owned)
		.collect()
}

/// Render the override YAML body for a service list.
///
/// Empty list -> `services: {}`. We still write the file so the
/// caller's "always pass `-f override`" rule stays uniform; an
/// empty `services:` block merges as a no-op.
fn render_override(services: &[String]) -> String {
	let mut body = String::new();
	let _ = writeln!(body, "# Generated by moon-ide. Do not edit.");
	let _ = writeln!(
		body,
		"# Neutralises `restart:` policies on the user's project compose so the"
	);
	let _ = writeln!(
		body,
		"# IDE's `docker compose stop` (on quit) doesn't fight the daemon."
	);
	let _ = writeln!(body, "# See specs/decisions/0017-project-compose-restart-override.md.");
	if services.is_empty() {
		let _ = writeln!(body, "services: {{}}");
		return body;
	}
	let _ = writeln!(body, "services:");
	for name in services {
		let _ = writeln!(body, "  {name}:");
		let _ = writeln!(body, "    restart: \"no\"");
	}
	body
}

#[cfg(test)]
mod tests {
	use std::fs;

	use camino::Utf8PathBuf;
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn override_path_lands_under_state_dir_subdir() {
		let state = Utf8PathBuf::from("/var/lib/moon-ide/workspaces/default");
		let p = override_path(&state, "moon-landing");
		assert_eq!(
			p,
			Utf8PathBuf::from("/var/lib/moon-ide/workspaces/default/project-overrides/moon-landing.yaml")
		);
	}

	#[test]
	fn parse_services_strips_whitespace_and_blanks() {
		let raw = "mongo\nredis\n\n  gitaly  \n\n";
		assert_eq!(parse_services_output(raw), vec!["mongo", "redis", "gitaly"]);
	}

	#[test]
	fn render_override_pins_each_service_to_no() {
		let body = render_override(&["mongo".into(), "redis".into()]);
		// Header comment present, each service gets a quoted "no"
		// so YAML doesn't coerce the value to the boolean `false`.
		assert!(body.contains("Generated by moon-ide"));
		assert!(body.contains("services:"));
		assert!(body.contains("  mongo:\n    restart: \"no\"\n"));
		assert!(body.contains("  redis:\n    restart: \"no\"\n"));
	}

	#[test]
	fn render_override_handles_empty_service_list() {
		let body = render_override(&[]);
		assert!(body.contains("services: {}"));
	}

	#[tokio::test]
	async fn ensure_creates_parent_dir_and_writes_file() {
		// `ensure_restart_override` shells out to `docker compose
		// config --services`, which we can't run in unit tests
		// (no daemon). Drive the I/O half via the public helpers
		// directly to prove the path math + write semantics.
		let tmp = tempdir().unwrap();
		let state = Utf8PathBuf::from_path_buf(tmp.path().join("workspace")).unwrap();
		let path = override_path(&state, "moon-landing");
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(&path, render_override(&["mongo".into()])).unwrap();

		assert!(path.is_file());
		let body = fs::read_to_string(&path).unwrap();
		assert!(body.contains("restart: \"no\""));
	}
}
