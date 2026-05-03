//! Graceful-shutdown helpers.
//!
//! moon-ide treats itself as the **command centre** for everything
//! it spawned: when the IDE quits, the workspace shell and every
//! bound-folder compose project it knows about get a `docker
//! compose stop` so the daemon doesn't accumulate ghost projects
//! the user can't see anymore. The next IDE launch picks the
//! workspace shell back up automatically (see [`auto_resume_shell`]
//! callers in `lib.rs`); per-folder projects stay stopped so the
//! user keeps their fine-grained control.
//!
//! Best-effort by design: every step logs and continues on failure.
//! A SIGKILL escape hatch (process exits without `stop_all`
//! finishing) is still safe — `compose stop` is idempotent and the
//! daemon will surface the still-running containers on next
//! launch.

use camino::Utf8PathBuf;
use moon_container::{ProjectCompose, Workspace as ContainerWorkspace, WorkspaceConfig};

use crate::state::AppState;

/// Stop the workspace shell and every bound-folder compose project.
///
/// Called from the Tauri `RunEvent::ExitRequested` hook before
/// `app.exit(0)`. Bound folders without a root-level compose file
/// short-circuit cheaply (no `docker compose` invocation).
///
/// Stops run **sequentially** rather than in parallel: docker daemon
/// serialises compose project mutations anyway, and a sequential
/// loop keeps the shutdown logs readable when the user is staring
/// at a "closing…" overlay.
pub async fn stop_all(state: &AppState) {
	let snapshot = state.workspaces.snapshot().await;
	let workspace_id = snapshot.id.clone();
	let bound: Vec<Utf8PathBuf> = snapshot.folders.iter().map(|f| Utf8PathBuf::from(&f.path)).collect();

	for folder in &bound {
		match ProjectCompose::for_folder(&workspace_id, folder) {
			Ok(Some(project)) => {
				if let Err(err) = project.stop().await {
					tracing::warn!(error = %err, folder = %folder, "stop_all: per-folder compose stop failed");
				} else {
					tracing::info!(folder = %folder, "stop_all: stopped per-folder compose");
				}
			}
			Ok(None) => {}
			Err(err) => {
				tracing::warn!(error = %err, folder = %folder, "stop_all: failed to resolve per-folder compose");
			}
		}
	}

	let state_dir = state.workspace_state_dir(&workspace_id);
	let shell = ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: workspace_id.clone(),
		state_dir,
		bound_folders: bound,
	});
	match shell {
		Ok(shell) => {
			if let Err(err) = shell.stop().await {
				tracing::warn!(error = %err, "stop_all: workspace shell stop failed");
			} else {
				tracing::info!("stop_all: stopped workspace shell");
			}
		}
		Err(err) => {
			tracing::warn!(error = %err, "stop_all: failed to build workspace shell handle");
		}
	}
}

/// On startup, if the workspace shell is currently stopped (i.e.
/// the previous IDE session ran [`stop_all`] cleanly), bring it
/// back up.
///
/// We deliberately don't auto-resume per-folder compose projects:
/// the user manages those individually from the folder bar and
/// might have intentionally torn one down before closing.
///
/// Best-effort. If `setup` fails, the pip surfaces the error via
/// the user's first popover open — the IDE itself still launches.
pub async fn auto_resume_shell(state: &AppState) {
	use moon_container::DEFAULT_DEV_IMAGE;
	use moon_protocol::container::ContainerState;

	let snapshot = state.workspaces.snapshot().await;
	let workspace_id = snapshot.id.clone();
	let bound: Vec<Utf8PathBuf> = snapshot.folders.iter().map(|f| Utf8PathBuf::from(&f.path)).collect();

	let state_dir = state.workspace_state_dir(&workspace_id);
	let shell = match ContainerWorkspace::new(WorkspaceConfig {
		workspace_id,
		state_dir,
		bound_folders: bound,
	}) {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "auto_resume_shell: failed to build workspace handle");
			return;
		}
	};

	let status = match shell.status().await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "auto_resume_shell: status query failed");
			return;
		}
	};

	if status.state != ContainerState::Stopped {
		// `Absent` means the user never opted in — nothing to
		// resume. `Running` / `Creating` means it's already up.
		// `Failed` we leave alone so the user sees the error
		// surface in the popover instead of silently retrying.
		return;
	}

	tracing::info!("auto_resume_shell: previous session left the workspace shell stopped, resuming");
	if let Err(err) = shell.setup(DEFAULT_DEV_IMAGE).await {
		tracing::warn!(error = %err, "auto_resume_shell: setup failed");
	}
}
