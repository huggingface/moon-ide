//! [`ShellResolver`] implementation that picks the right shell
//! target for moon-core's host-issued subprocesses (today:
//! format-on-save).
//!
//! Mirrors the routing decision the LSP layer makes in
//! `commands::lsp::resolve_target` and the agent's bash tool
//! makes in `moon_coder::tools::resolve_bash_target` — build a
//! `ContainerWorkspace` from the current bound-folder set, query
//! its lifecycle status, and return [`ShellTarget::Container`]
//! only when the workspace shell is `Running`. Any failure path
//! (missing compose project, daemon unreachable, parse error)
//! collapses to [`ShellTarget::Host`] so format-on-save never
//! becomes worse than it was before container routing existed.
//!
//! The resolver holds a `Weak<WorkspaceRegistry>` rather than an
//! `Arc<…>` to break the cycle: the registry owns a
//! `OnceLock<ShellResolverHandle>` (the handle wraps this
//! resolver), and the resolver wants to read the registry's
//! current bound-folder set on each call. Weak avoids leaking
//! the registry across shutdown.

use std::sync::Weak;

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::shell::{ShellResolver, ShellTarget};
use moon_core::WorkspaceRegistry;
use moon_protocol::container::ContainerState;
use moon_terminal::{container_name_for_workspace, TerminalTarget};

/// Async resolver that asks `moon-container` whether the
/// workspace shell is up and chooses host vs. container per call.
pub struct WorkspaceShellResolver {
	workspaces: Weak<WorkspaceRegistry>,
	workspaces_dir: Utf8PathBuf,
}

impl WorkspaceShellResolver {
	pub fn new(workspaces: Weak<WorkspaceRegistry>, workspaces_dir: Utf8PathBuf) -> Self {
		Self {
			workspaces,
			workspaces_dir,
		}
	}
}

#[async_trait::async_trait]
impl ShellResolver for WorkspaceShellResolver {
	async fn resolve(&self, host_root: &Utf8Path) -> ShellTarget {
		let Some(workspaces) = self.workspaces.upgrade() else {
			return ShellTarget::Host;
		};
		let workspace_id = workspaces.workspace_id().await;
		let state_dir = self.workspaces_dir.join(&workspace_id);
		let entries = workspaces.folders().await;
		// Worktree-backed session folders (ADR 0029) live inside the
		// parent repo at `<parent>/.worktrees/…`, so they ride the
		// parent's bind mount: a worktree's server_root is the parent's
		// `/workspace/<name>` mount plus the relative tail, not a mount
		// of its own. (W.4 routed these host-side, which wrongly built
		// an isolated session with the host toolchain.)
		let worktree_server_root = entries.iter().find_map(|entry| {
			if entry.folder.path != host_root.as_str() {
				return None;
			}
			let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = &entry.folder.origin else {
				return None;
			};
			moon_core::worktree::worktree_container_path(Utf8Path::new(parent_path), host_root)
		});
		// Worktrees aren't individually bound — they ride their parent's
		// mount — so keep them out of the per-folder bound-mount set
		// used to resolve container status.
		let bound: Vec<Utf8PathBuf> = entries
			.iter()
			.filter(|entry| {
				!matches!(
					entry.folder.origin,
					moon_protocol::workspace::FolderOrigin::Worktree { .. }
				)
			})
			.map(|entry| Utf8PathBuf::from(&entry.folder.path))
			.collect();

		let ws = match ContainerWorkspace::new(WorkspaceConfig {
			workspace_id: workspace_id.clone(),
			state_dir,
			bound_folders: bound,
		}) {
			Ok(ws) => ws,
			Err(err) => {
				tracing::debug!(%err, "shell-resolver: container config unavailable, routing to host");
				return ShellTarget::Host;
			}
		};

		match ws.status().await {
			Ok(status) if matches!(status.state, ContainerState::Running) => {
				let server_root = worktree_server_root.unwrap_or_else(|| {
					TerminalTarget::container_cwd_for_folder(host_root).unwrap_or_else(|| Utf8PathBuf::from("/workspace"))
				});
				ShellTarget::Container {
					container_name: container_name_for_workspace(&workspace_id),
					host_root: host_root.to_path_buf(),
					server_root,
				}
			}
			Ok(_) => ShellTarget::Host,
			Err(err) => {
				tracing::debug!(%err, "shell-resolver: container status query failed, routing to host");
				ShellTarget::Host
			}
		}
	}
}
