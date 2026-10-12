//! Workspace-process startup shared by the desktop and `moon-remote
//! serve`, so a workspace boots the same whichever host owns it.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use moon_core::WorkspaceRegistry;
use moon_protocol::app_state::AppState as AppStatePayload;
use moon_protocol::coder_models::CoderProviderLock;
use moon_protocol::session::WorkspaceSession;
use moon_protocol::workspace::FolderOrigin;

use crate::shell_resolver::WorkspaceShellResolver;

/// The workspace's folder registry, with format-on-save routed
/// through the workspace shell container whenever it's running.
pub fn new_registry(workspace_id: String, workspaces_dir: Utf8PathBuf) -> Arc<WorkspaceRegistry> {
	let registry = Arc::new(WorkspaceRegistry::new(workspace_id));
	// Weak: the registry owns the resolver handle.
	let resolver = Arc::new(WorkspaceShellResolver::new(Arc::downgrade(&registry), workspaces_dir));
	registry.set_shell_resolver(moon_core::ShellResolverHandle::new(resolver));
	registry
}

/// Seed the coder from the persisted model picks, `bill_to` and
/// providers. A per-workspace provider lock beats the global active
/// provider. Empty slugs resolve to the built-in defaults inside
/// `CoderModels`; `has_api_key` is re-read from the keyring by
/// `CoderHandle::new`, never trusted from disk.
pub fn coder_models(state: &AppStatePayload, lock: Option<&CoderProviderLock>) -> moon_coder::CoderModels {
	let active_provider = match lock {
		Some(CoderProviderLock::Hf) => None,
		Some(CoderProviderLock::User { id }) => Some(id.clone()),
		None => state.coder.active_provider.clone(),
	};
	moon_coder::CoderModels {
		standard: state.coder.standard_model.clone(),
		cheap: state.coder.cheap_model.clone(),
		bill_to: (!state.coder.bill_to.is_empty()).then(|| state.coder.bill_to.clone()),
		providers: state.coder.providers.clone(),
		active_provider,
		context_window_overrides: Arc::new(state.coder.context_window_overrides.clone()),
		rotation: Arc::new(state.coder.rotation.clone()),
		reasoning_effort: state.coder.reasoning_effort.clone(),
		..moon_coder::CoderModels::default()
	}
}

/// Re-bind the folders `session.json` lists and restore the active
/// folder.
pub async fn restore_folders(registry: &WorkspaceRegistry, session: &WorkspaceSession) {
	for folder in &session.folders {
		let path = Utf8PathBuf::from(&folder.folder_path);
		// Worktree-backed session folders (ADR 0028) re-bind as
		// nested worktree folders so their session keeps routing to
		// the checkout. Folders are stored in insertion order, so a
		// worktree's parent is always re-added before it.
		let result = match &folder.origin {
			FolderOrigin::Worktree { parent_path, branch } => {
				// An orphan worktree (its parent didn't restore) is
				// unreachable in the folder bar and can't be
				// discarded through the parent; skip it so the
				// workspace self-heals.
				let Some(parent) = registry.folder_for_path(parent_path).await else {
					tracing::warn!(path = %path, parent = %parent_path, "skipping orphan worktree folder (parent not restored)");
					continue;
				};
				// A checkout removed while the IDE was closed would
				// re-bind as a dead row; skip it and forget the stale
				// git metadata, which would otherwise refuse a later
				// `git worktree add` at the same path (ADR 0063).
				if !Utf8Path::new(&path).is_dir() {
					tracing::warn!(path = %path, "skipping worktree folder whose checkout is gone from disk");
					if let Err(err) = parent.host.git_worktree_forget(&path).await {
						tracing::warn!(error = %err, path = %path, "git worktree prune failed for a vanished checkout");
					}
					continue;
				}
				registry
					.add_worktree_folder(path.clone(), parent_path.clone(), branch.clone())
					.await
			}
			FolderOrigin::UserPicked => registry.add_folder(path.clone()).await,
		};
		if let Err(e) = result {
			tracing::warn!(error = %e, path = %path, "failed to restore folder");
		}
	}
	if let Some(active) = session.active_folder_path.as_ref() {
		if let Err(e) = registry.set_active_folder(active).await {
			tracing::warn!(error = %e, path = %active, "failed to restore active folder");
		}
	}
	// Disk is the source of truth for worktrees (ADR 0079): adopt
	// IDE-managed checkouts under a bound folder's `.worktrees/` that
	// session.json didn't list, so their rows reappear instead of
	// pinning branches behind git's "used by worktree" refusal.
	registry.adopt_disk_worktrees().await;
}
