//! Where a host-issued subprocess actually runs.
//!
//! moon-ide spawns several kinds of host-side subprocess on the
//! user's behalf — format-on-save tools, lint-staged commands,
//! the agent's `bash` tool, LSP servers. Each one needs to land in
//! the same "userland" the file under work belongs to:
//!
//! - **No container set up** → run on the host directly, exactly
//!   the way the corresponding shell command would.
//! - **Workspace shell container is `Running`** → run inside the
//!   `moon-base` dev service so we pick up the project's actual
//!   toolchain (nightly rustfmt, pinned `prettier`, in-container
//!   `node_modules/.bin/`, …) instead of whatever the host has.
//!
//! [`ShellTarget`] is the tag (host vs. one specific container);
//! [`ShellResolver`] is the lookup the implementation layer
//! (`src-tauri/`) plugs in so moon-core stays free of `docker`
//! knowledge. The LSP layer (`lsp::spawn::LspSpawner`) and the
//! coder bash tool (`moon-coder::tools::resolve_bash_target`)
//! independently grew the same shape; this module is the version
//! the format-on-save pipeline uses, and a future cleanup can
//! collapse those two into the same trait.
//!
//! See ADR 0002 (workspace host) and `specs/lsp.md`'s
//! "container-backed LSP" section for the routing principle.

use camino::{Utf8Path, Utf8PathBuf};
use std::sync::Arc;

/// Where a subprocess should be spawned.
///
/// `host_root` and `server_root` (when present) describe the bind
/// mount mapping that lets host paths translate to in-container
/// paths: a file at `<host_root>/foo/bar.rs` on the host appears
/// at `<server_root>/foo/bar.rs` inside the container, and vice
/// versa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellTarget {
	/// Run the subprocess directly on the host's userland.
	Host,
	/// Run inside the workspace shell container via `docker exec`.
	///
	/// `container_name` is the compose-assigned name for the dev
	/// service (`moon-ws-<id>-dev-1`). Callers must verify the
	/// container is in the `Running` state before constructing
	/// this variant — the same contract every other container-
	/// aware spawner in the tree uses.
	///
	/// `host_root` is the absolute host path of the workspace
	/// folder this target is for; `server_root` is its in-
	/// container mount point (`/workspace/<basename>` per the
	/// bind-mount layout `moon-container::compose` writes).
	Container {
		container_name: String,
		host_root: Utf8PathBuf,
		server_root: Utf8PathBuf,
	},
}

impl ShellTarget {
	/// Translate a host-side absolute path inside `host_root` to
	/// its equivalent in-container absolute path. Returns the
	/// input unchanged for [`ShellTarget::Host`]; `None` when the
	/// path lives outside the bind mount under [`ShellTarget::
	/// Container`] (callers fall back to host execution rather
	/// than spawning with a path the in-container process can't
	/// see).
	pub fn translate_path(&self, abs_host_path: &Utf8Path) -> Option<Utf8PathBuf> {
		match self {
			ShellTarget::Host => Some(abs_host_path.to_path_buf()),
			ShellTarget::Container {
				host_root, server_root, ..
			} => {
				let rel = abs_host_path.strip_prefix(host_root).ok()?;
				// `Utf8PathBuf::join("")` appends an empty component
				// that renders with a trailing separator on some
				// platforms ("/workspace/foo/" instead of
				// "/workspace/foo"). The "input is the host root
				// itself" case is common (formatter `cwd` for files
				// at the workspace root) so guard it explicitly.
				if rel.as_str().is_empty() {
					return Some(server_root.to_path_buf());
				}
				Some(server_root.join(rel))
			}
		}
	}
}

/// Lazy resolver from a workspace folder to the [`ShellTarget`]
/// its subprocesses should run in. The Tauri layer plugs in an
/// implementation that queries `moon-container`'s lifecycle for
/// the live container state; tests use a stub that always returns
/// the variant under test.
#[async_trait::async_trait]
pub trait ShellResolver: Send + Sync {
	/// `host_root` is the absolute host path of the active folder
	/// the operation is scoped to. Implementations decide whether
	/// the workspace's shell container hosts that folder.
	async fn resolve(&self, host_root: &Utf8Path) -> ShellTarget;
}

/// Stable handle wrapping an `Arc<dyn ShellResolver>` so callers
/// don't have to spell the trait object out. Cloning is cheap.
#[derive(Clone)]
pub struct ShellResolverHandle(Arc<dyn ShellResolver>);

impl ShellResolverHandle {
	pub fn new(resolver: Arc<dyn ShellResolver>) -> Self {
		Self(resolver)
	}

	pub async fn resolve(&self, host_root: &Utf8Path) -> ShellTarget {
		self.0.resolve(host_root).await
	}
}

impl std::fmt::Debug for ShellResolverHandle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ShellResolverHandle").finish_non_exhaustive()
	}
}

/// Always-host resolver. Used when no real resolver is plugged in
/// (host-only runs, isolated tests, the moon-remote sidecar
/// before it grows its own routing).
pub struct AlwaysHostResolver;

#[async_trait::async_trait]
impl ShellResolver for AlwaysHostResolver {
	async fn resolve(&self, _host_root: &Utf8Path) -> ShellTarget {
		ShellTarget::Host
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn translate_path_host_is_identity() {
		let t = ShellTarget::Host;
		let abs = Utf8PathBuf::from("/home/dev/code/foo/bar.rs");
		assert_eq!(t.translate_path(&abs), Some(abs.clone()));
	}

	#[test]
	fn translate_path_container_rebases_under_server_root() {
		let t = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/workloads"),
			server_root: Utf8PathBuf::from("/workspace/workloads"),
		};
		assert_eq!(
			t.translate_path(Utf8Path::new("/home/dev/code/workloads/app/sdk/src/main.rs")),
			Some(Utf8PathBuf::from("/workspace/workloads/app/sdk/src/main.rs")),
		);
	}

	#[test]
	fn translate_path_container_input_equals_host_root_returns_server_root() {
		let t = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/workloads"),
			server_root: Utf8PathBuf::from("/workspace/workloads"),
		};
		// Input is the host root verbatim — must not introduce a
		// trailing separator. Regression guard for the
		// `Utf8PathBuf::join("")` quirk.
		assert_eq!(
			t.translate_path(Utf8Path::new("/home/dev/code/workloads")),
			Some(Utf8PathBuf::from("/workspace/workloads")),
		);
	}

	#[test]
	fn translate_path_container_returns_none_outside_mount() {
		let t = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/workloads"),
			server_root: Utf8PathBuf::from("/workspace/workloads"),
		};
		assert_eq!(t.translate_path(Utf8Path::new("/etc/hostname")), None);
	}

	#[tokio::test]
	async fn always_host_resolver_returns_host() {
		let r = AlwaysHostResolver;
		let target = r.resolve(Utf8Path::new("/anywhere")).await;
		assert_eq!(target, ShellTarget::Host);
	}
}
