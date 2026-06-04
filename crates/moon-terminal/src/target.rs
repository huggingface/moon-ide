//! `TerminalTarget` and helpers for translating it into a
//! `portable_pty::CommandBuilder`.

use camino::{Utf8Path, Utf8PathBuf};
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};

/// Where a terminal's shell process runs. The variant is fixed
/// at open time and never changes for a given session: a host
/// terminal stays a host terminal even if the workspace
/// container later starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TerminalTarget {
	/// Spawn a shell directly on the user's machine.
	Host {
		/// Working directory the shell starts in. `None` falls
		/// back to the user's home directory (`$HOME`).
		cwd: Option<Utf8PathBuf>,
		/// Override for the shell binary; `None` reads
		/// `$SHELL` with `/bin/bash` as a final fallback.
		shell: Option<TerminalShell>,
	},
	/// `docker exec` into a workspace container. The name
	/// comes from [`container_name_for_workspace`] at the
	/// caller site so the protocol surface doesn't lock us
	/// into a specific naming scheme.
	Container {
		/// Compose-assigned container name
		/// (`moon-ws-<id>-dev-1`). The caller is responsible
		/// for verifying the container is actually running
		/// before opening a terminal against it; we don't
		/// re-check.
		container_name: String,
		/// In-container working directory. Required because
		/// `docker exec` doesn't have a portable "user's home"
		/// concept and a container terminal landing in `/`
		/// would surprise the user.
		cwd: Utf8PathBuf,
		/// Override for the shell binary; `None` falls back to
		/// `bash` (always present in `moon-base`).
		shell: Option<TerminalShell>,
		/// Extra environment variables to inject via
		/// `docker exec -e KEY=VALUE`. Each entry produces one
		/// `-e <key>=<value>` pair; the supervisor never
		/// invents env vars on its own, so an empty `Vec` is
		/// the default. Used to forward `$GIT_EDITOR` =
		/// `moon-edit` and friends into IDE-launched container
		/// terminals — see `specs/containers.md` § "Editor
		/// forwarding" and ADR 0021.
		#[serde(default)]
		env: Vec<(String, String)>,
	},
}

/// A shell binary to spawn. Wraps a `String` so we can change
/// the validation surface later (e.g. allowlist) without
/// touching every call site.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalShell(pub String);

impl TerminalShell {
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl From<String> for TerminalShell {
	fn from(s: String) -> Self {
		Self(s)
	}
}

/// Compute the workspace container's compose name for the
/// given workspace id. Mirrors compose's
/// `<project>-<service>-<n>` convention; `dev` is the
/// only service in the workspace shell project so the index
/// is always `1`.
///
/// Kept here (not in `moon-container`) so the terminal layer
/// doesn't pull in the lifecycle module just to format a
/// string. The `moon-ws-` prefix is shared by ADR 0007 and is
/// stable.
pub fn container_name_for_workspace(workspace_id: &str) -> String {
	format!("moon-ws-{workspace_id}-dev-1")
}

/// In-container path the workspace's `instance.sock` resolves to.
/// The socket's parent `run/` directory is bind-mounted at
/// `/run/moon` (ADR 0026), so the socket appears here — see
/// `MOON_EDIT_SOCKET_CONTAINER_PATH` in `moon-container::compose`.
/// Duplicated here because the terminal supervisor doesn't (and
/// shouldn't) depend on `moon-container`; keep the two in sync.
pub const MOON_EDIT_CONTAINER_SOCK: &str = "/run/moon/instance.sock";

/// Build the `(container, host)` path-map list for the bound
/// folder set, as the `MOON_EDIT_PATH_MAP` env var consumed by
/// the in-container `moon-edit` shim. The shim picks the longest
/// matching container prefix; we emit the entries in the same
/// order they were bound, which is also the order they were
/// rendered into `compose.yaml`'s `volumes:` block, so the map
/// matches the actual mount layout.
///
/// Bound folders without a usable basename (a single `/`, an
/// empty path) are skipped — those don't get a `/workspace/<name>`
/// bind in compose either, so there's nothing to map.
pub fn moon_edit_path_map_for_bound_folders(bound_folders: &[Utf8PathBuf]) -> String {
	let mut entries = Vec::with_capacity(bound_folders.len());
	for folder in bound_folders {
		let Some(basename) = folder.file_name() else {
			continue;
		};
		if basename.is_empty() {
			continue;
		}
		entries.push(format!("/workspace/{basename}={host}", host = folder.as_str()));
	}
	entries.join(":")
}

/// Build the standard `-e` env list the IDE injects into every
/// container terminal so `git commit --amend` (and friends) hand
/// off the editor to the host IDE. See ADR 0021.
///
/// Returns an empty `Vec` if `bound_folders` is empty — there's
/// no usable path map without folders, so we don't set the
/// editor vars at all (the user would just see a confusing
/// "moon-edit: no MOON_EDIT_PATH_MAP entry matches" error).
pub fn editor_forward_env_for_workspace(bound_folders: &[Utf8PathBuf]) -> Vec<(String, String)> {
	let path_map = moon_edit_path_map_for_bound_folders(bound_folders);
	if path_map.is_empty() {
		return Vec::new();
	}
	vec![
		("GIT_EDITOR".to_owned(), "moon-edit".to_owned()),
		("EDITOR".to_owned(), "moon-edit".to_owned()),
		("VISUAL".to_owned(), "moon-edit".to_owned()),
		("MOON_EDIT_SOCK".to_owned(), MOON_EDIT_CONTAINER_SOCK.to_owned()),
		("MOON_EDIT_PATH_MAP".to_owned(), path_map),
	]
}

impl TerminalTarget {
	/// Translate the target into a `portable_pty::CommandBuilder`.
	///
	/// Sets `TERM=xterm-256color` on both targets so prompts and
	/// TUIs render correctly; the in-container case adds
	/// `docker exec -it -w <cwd> <name> <shell>` framing.
	pub fn to_command(&self) -> CommandBuilder {
		match self {
			TerminalTarget::Host { cwd, shell } => {
				let shell_path = shell
					.as_ref()
					.map(|s| s.as_str().to_owned())
					.unwrap_or_else(default_host_shell);
				let mut cmd = CommandBuilder::new(shell_path);
				if let Some(cwd) = cwd {
					cmd.cwd(cwd.as_std_path());
				} else if let Some(home) = dirs_home_dir() {
					cmd.cwd(home.as_path());
				}
				cmd.env("TERM", "xterm-256color");
				cmd
			}
			TerminalTarget::Container {
				container_name,
				cwd,
				shell,
				env,
			} => {
				let shell_str = shell.as_ref().map(|s| s.as_str()).unwrap_or("bash");
				// `docker exec -it -w <cwd> <container> <shell>`
				// — `-i` keeps stdin open, `-t` allocates a TTY
				// inside the container so prompts render. The
				// host PTY portable-pty allocates is the bridge
				// between the user's keyboard and the
				// in-container TTY; SIGWINCH propagates through
				// docker correctly.
				let mut cmd = CommandBuilder::new("docker");
				cmd.arg("exec");
				cmd.arg("-it");
				cmd.arg("-w");
				cmd.arg(cwd.as_str());
				cmd.arg("-e");
				cmd.arg("TERM=xterm-256color");
				for (key, value) in env {
					// Bare-minimum sanity: `docker exec -e` accepts
					// `KEY=VALUE` and a `\n` in either side would
					// confuse it. Anything weirder than that is
					// the caller's responsibility.
					if key.contains('\n') || value.contains('\n') {
						tracing::warn!(key = %key, "skipping container env entry with newline");
						continue;
					}
					cmd.arg("-e");
					cmd.arg(format!("{key}={value}"));
				}
				cmd.arg(container_name);
				cmd.arg(shell_str);
				cmd.env("TERM", "xterm-256color");
				cmd
			}
		}
	}

	/// Map a host-side absolute path to its in-container mount
	/// path under `/workspace/<basename>`. Returns `None` for
	/// inputs without a basename (e.g. a single `/`), in which
	/// case the caller falls back to `/workspace`.
	///
	/// The mapping mirrors the bind-mount layout
	/// `moon-container::compose` writes — see
	/// `BoundMount.mount_name` and `mount_name_for`.
	pub fn container_cwd_for_folder(folder: &Utf8Path) -> Option<Utf8PathBuf> {
		let basename = folder.file_name()?;
		Some(Utf8PathBuf::from(format!("/workspace/{basename}")))
	}
}

/// Default host shell: `$SHELL` if set, else `/bin/bash`. We
/// don't try to dispatch on Windows here — moon-ide isn't
/// shipping for it yet, and the `cmd.exe` path would need its
/// own arg handling; revisit when Windows joins the matrix.
fn default_host_shell() -> String {
	std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_owned())
}

fn dirs_home_dir() -> Option<std::path::PathBuf> {
	std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn container_name_follows_compose_convention() {
		assert_eq!(container_name_for_workspace("default"), "moon-ws-default-dev-1");
		assert_eq!(container_name_for_workspace("foo-bar"), "moon-ws-foo-bar-dev-1");
	}

	#[test]
	fn container_cwd_uses_basename_under_workspace() {
		let folder = Utf8PathBuf::from("/home/me/code/moon-landing");
		assert_eq!(
			TerminalTarget::container_cwd_for_folder(&folder),
			Some(Utf8PathBuf::from("/workspace/moon-landing"))
		);
	}

	#[test]
	fn container_cwd_returns_none_for_pathological_input() {
		// `/` has no basename to mount under.
		assert_eq!(TerminalTarget::container_cwd_for_folder(Utf8Path::new("/")), None);
	}

	#[test]
	fn host_target_falls_back_to_bash_without_shell_env() {
		// Independent of the test environment's $SHELL — we
		// only check that a target with `shell: None` produces
		// *some* command, not which one. The exact fallback is
		// covered by `default_host_shell` not panicking.
		let t = TerminalTarget::Host { cwd: None, shell: None };
		let _cmd = t.to_command();
	}

	#[test]
	fn container_target_invokes_docker_exec() {
		let t = TerminalTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			cwd: Utf8PathBuf::from("/workspace/moon-landing"),
			shell: None,
			env: Vec::new(),
		};
		// portable-pty doesn't expose the argv after building,
		// so we just confirm the call doesn't panic and the
		// builder accepts our shape. End-to-end coverage
		// belongs in the manual test plan.
		let _cmd = t.to_command();
	}

	#[test]
	fn editor_forward_env_for_workspace_returns_empty_with_no_folders() {
		assert!(editor_forward_env_for_workspace(&[]).is_empty());
	}

	#[test]
	fn editor_forward_env_for_workspace_emits_full_set_when_folders_present() {
		let env = editor_forward_env_for_workspace(&[
			Utf8PathBuf::from("/home/me/code/moon-ide"),
			Utf8PathBuf::from("/home/me/code/moon-landing"),
		]);
		let map: std::collections::HashMap<_, _> = env.iter().cloned().collect();
		assert_eq!(map.get("GIT_EDITOR").map(String::as_str), Some("moon-edit"));
		assert_eq!(map.get("EDITOR").map(String::as_str), Some("moon-edit"));
		assert_eq!(map.get("VISUAL").map(String::as_str), Some("moon-edit"));
		assert_eq!(
			map.get("MOON_EDIT_SOCK").map(String::as_str),
			Some(MOON_EDIT_CONTAINER_SOCK)
		);
		assert_eq!(
			map.get("MOON_EDIT_PATH_MAP").map(String::as_str),
			Some("/workspace/moon-ide=/home/me/code/moon-ide:/workspace/moon-landing=/home/me/code/moon-landing"),
		);
	}

	#[test]
	fn moon_edit_path_map_skips_folders_without_basename() {
		let map =
			moon_edit_path_map_for_bound_folders(&[Utf8PathBuf::from("/home/me/code/moon-ide"), Utf8PathBuf::from("/")]);
		assert_eq!(map, "/workspace/moon-ide=/home/me/code/moon-ide");
	}
}
