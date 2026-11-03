//! Home paths kept across `dev` container recreation.
//!
//! Each workspace has one named compose volume mounted at
//! [`PERSIST_CONTAINER_DIR`]. The user lists home-relative paths in
//! `<state_dir>/persist-paths.json`; after every `up` (and when the
//! list changes) each listed path becomes a symlink into the volume.
//! A path that already exists in the container the first time is
//! moved into the volume. Toolchains under `/home/dev` stay on the
//! image, so image upgrades still reach them.

use camino::Utf8Path;
use tokio::process::Command;

use crate::lifecycle::LifecycleError;

/// Where the workspace's persist volume is mounted in `dev`.
pub const PERSIST_CONTAINER_DIR: &str = "/home/dev/.moon-persist";

/// The list, in the workspace state dir.
pub const PERSIST_PATHS_FILE: &str = "persist-paths.json";

const HOME: &str = "/home/dev";

/// Normalise a user-entered path to one relative to the container
/// home: `~/x`, `/home/dev/x` and `x` all become `x`. Paths outside
/// home, `..` components and the volume itself are rejected.
pub fn normalize_persist_path(raw: &str) -> Result<String, String> {
	let trimmed = raw.trim();
	let rel = if let Some(rest) = trimmed.strip_prefix("~/") {
		rest
	} else if let Some(rest) = trimmed.strip_prefix(&format!("{HOME}/")) {
		rest
	} else if trimmed.starts_with('/') || trimmed.starts_with('~') {
		return Err(format!("{trimmed}: only paths under {HOME} can be persisted"));
	} else {
		trimmed
	};
	let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
	if parts.is_empty() {
		return Err("empty path".to_owned());
	}
	if parts.contains(&"..") {
		return Err(format!("{trimmed}: `..` is not allowed"));
	}
	if parts[0] == ".moon-persist" {
		return Err(format!("{trimmed}: that's the persist volume itself"));
	}
	Ok(parts.join("/"))
}

/// The workspace's persisted paths; empty when the file is missing
/// or unreadable.
pub async fn read_persist_paths(state_dir: &Utf8Path) -> Vec<String> {
	let path = state_dir.join(PERSIST_PATHS_FILE);
	let Ok(text) = tokio::fs::read_to_string(path.as_std_path()).await else {
		return Vec::new();
	};
	match serde_json::from_str::<Vec<String>>(&text) {
		Ok(paths) => paths,
		Err(err) => {
			tracing::warn!(%err, %path, "could not parse persisted paths; ignoring");
			Vec::new()
		}
	}
}

/// Normalise, dedupe and store the list. Returns what was stored.
pub async fn write_persist_paths(state_dir: &Utf8Path, raw: &[String]) -> Result<Vec<String>, String> {
	let mut paths: Vec<String> = Vec::new();
	for entry in raw {
		if entry.trim().is_empty() {
			continue;
		}
		let path = normalize_persist_path(entry)?;
		if !paths.contains(&path) {
			paths.push(path);
		}
	}
	let json = serde_json::to_string_pretty(&paths).map_err(|err| err.to_string())?;
	tokio::fs::create_dir_all(state_dir.as_std_path())
		.await
		.map_err(|err| err.to_string())?;
	tokio::fs::write(state_dir.join(PERSIST_PATHS_FILE).as_std_path(), json)
		.await
		.map_err(|err| err.to_string())?;
	Ok(paths)
}

// Per path: already linked → skip; in the volume → link (an
// image-provided copy is kept aside as `.moon-image`); only in the
// container → move into the volume and link; nowhere → wait until it
// exists (a dangling link would break tools that `mkdir` it).
const LINK_SCRIPT: &str = r#"
persist="$1"
shift
for rel in "$@"; do
	src="$HOME/$rel"
	dst="$persist/home/$rel"
	if [ -L "$src" ] && [ "$(readlink "$src")" = "$dst" ]; then
		continue
	fi
	if [ -e "$dst" ] || [ -L "$dst" ]; then
		if [ -e "$src" ] || [ -L "$src" ]; then
			rm -rf "$src.moon-image"
			mv "$src" "$src.moon-image"
		fi
	elif [ -e "$src" ]; then
		mkdir -p "$(dirname "$dst")"
		mv "$src" "$dst"
	else
		continue
	fi
	mkdir -p "$(dirname "$src")"
	ln -s "$dst" "$src"
done
"#;

/// Link every persisted path in the running `dev` container. The
/// volume starts root-owned, so it's handed to the home's owner
/// first. A container created before the volume existed has no mount
/// there; linking into it would lose the files on the next recreate.
pub async fn link_persisted_paths(container: &str, paths: &[String]) -> Result<(), LifecycleError> {
	let prepare = format!(
		"d=$(stat -c %d {PERSIST_CONTAINER_DIR} 2>/dev/null) || exit 3; \
		 [ \"$d\" != \"$(stat -c %d {HOME})\" ] || exit 3; \
		 chown \"$(stat -c %u:%g {HOME})\" {PERSIST_CONTAINER_DIR}"
	);
	match docker_exec(container, &["-u", "root"], &["sh", "-c", &prepare]).await {
		Err(LifecycleError::DockerCommandFailed { code: 3, .. }) => {
			return Err(LifecycleError::InvalidPersistPath(
				"the persist volume isn't mounted in this container yet; Recreate it first (files only in the container are lost on Recreate, so copy them out)".to_owned(),
			));
		}
		other => other?,
	}
	if paths.is_empty() {
		return Ok(());
	}
	let mut argv: Vec<&str> = vec!["sh", "-c", LINK_SCRIPT, "sh", PERSIST_CONTAINER_DIR];
	argv.extend(paths.iter().map(String::as_str));
	docker_exec(container, &[], &argv).await
}

async fn docker_exec(container: &str, flags: &[&str], argv: &[&str]) -> Result<(), LifecycleError> {
	let output = Command::new("docker")
		.arg("exec")
		.args(flags)
		.arg(container)
		.args(argv)
		.output()
		.await
		.map_err(|err| {
			if err.kind() == std::io::ErrorKind::NotFound {
				LifecycleError::DockerMissing
			} else {
				LifecycleError::Io(err)
			}
		})?;
	if output.status.success() {
		return Ok(());
	}
	Err(LifecycleError::DockerCommandFailed {
		subcommand: "exec (persisted paths)".to_owned(),
		code: output.status.code().unwrap_or(-1),
		stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn paths_normalise_to_home_relative() {
		assert_eq!(normalize_persist_path("~/.bgs").unwrap(), ".bgs");
		assert_eq!(normalize_persist_path("/home/dev/.config/foo/").unwrap(), ".config/foo");
		assert_eq!(normalize_persist_path(" .bgs ").unwrap(), ".bgs");
		assert!(normalize_persist_path("/etc/passwd").is_err());
		assert!(normalize_persist_path("~other/x").is_err());
		assert!(normalize_persist_path("a/../../x").is_err());
		assert!(normalize_persist_path("~/").is_err());
		assert!(normalize_persist_path(".moon-persist/x").is_err());
	}

	#[tokio::test]
	async fn the_list_round_trips_deduped() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let stored = write_persist_paths(&dir, &["~/.bgs".into(), ".bgs".into(), "".into(), "~/.x".into()])
			.await
			.unwrap();
		assert_eq!(stored, vec![".bgs", ".x"]);
		assert_eq!(read_persist_paths(&dir).await, stored);
	}

	#[tokio::test]
	async fn the_link_script_moves_links_and_waits() {
		let tmp = tempfile::tempdir().unwrap();
		let home = tmp.path().join("home");
		let persist = tmp.path().join("persist");
		std::fs::create_dir_all(&home).unwrap();
		std::fs::write(home.join(".bgs"), "token").unwrap();
		let run = || {
			let status = std::process::Command::new("sh")
				.env("HOME", &home)
				.args(["-c", LINK_SCRIPT, "sh"])
				.arg(&persist)
				.args([".bgs", ".later"])
				.status()
				.unwrap();
			assert!(status.success());
		};
		run();
		let link = home.join(".bgs");
		assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
		assert_eq!(std::fs::read_to_string(&link).unwrap(), "token");
		assert!(!home.join(".later").exists());
		// A recreated container: home is fresh, the volume isn't.
		std::fs::remove_file(&link).unwrap();
		std::fs::write(&link, "image default").unwrap();
		run();
		assert_eq!(std::fs::read_to_string(&link).unwrap(), "token");
		assert_eq!(
			std::fs::read_to_string(home.join(".bgs.moon-image")).unwrap(),
			"image default"
		);
		// Idempotent.
		run();
		assert_eq!(std::fs::read_to_string(&link).unwrap(), "token");
	}
}
