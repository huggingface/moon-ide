//! Graceful-shutdown helpers.
//!
//! moon-ide treats itself as the **command centre** for everything
//! it spawned: when the IDE quits, the workspace shell and every
//! bound-folder compose project it knows about get a `docker
//! compose stop` so the daemon doesn't accumulate ghost projects
//! the user can't see anymore. The next IDE launch picks the
//! workspace shell back up automatically (see [`auto_resume_shell`]
//! callers in `lib.rs`); per-folder projects whose status was
//! `Running` immediately before the stop also auto-resume (see
//! [`auto_resume_project_composes`]) so the user doesn't have to
//! re-click "Start services" for every folder on every relaunch.
//! Folders the user had already taken down by hand stay down — we
//! only resurrect what was actually running.
//!
//! Best-effort by design: every step logs and continues on failure.
//! A SIGKILL escape hatch (process exits without `stop_all`
//! finishing) is still safe — `compose stop` is idempotent and the
//! daemon will surface the still-running containers on next
//! launch.

use camino::Utf8PathBuf;
use moon_container::{ProjectCompose, Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_protocol::container::ContainerState;

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
	// Abort every open terminal supervisor first. Each abort
	// drops the supervisor's `PtySession`, which SIGKILLs the
	// child process (host shell or `docker exec`) eagerly.
	// Without this step `docker exec` children survive the
	// IDE process and accumulate as orphans on the daemon.
	{
		let mut registry = state.terminal_streams.lock().await;
		let count = registry.len();
		for (_, handle) in registry.drain() {
			handle.abort.abort();
		}
		if count > 0 {
			tracing::info!(count, "stop_all: aborted terminal supervisors");
		}
	}

	// Shut down any spawned LSP servers. `kill_on_drop` on the
	// child handles the SIGKILL escape hatch, but `shutdown_all`
	// gives each server ~2s to flush state first (tsserver persists
	// a cache file on graceful exit that speeds up the next boot).
	{
		let handle = state.lsp.lock().await.take();
		if let Some(handle) = handle {
			handle.broker.shutdown_all().await;
			tracing::info!("stop_all: stopped lsp broker");
		}
	}

	if let Err(err) = state.next_edit_server.stop().await {
		tracing::warn!(error = %err, "stop_all: next_edit_server stop failed");
	} else {
		tracing::info!("stop_all: stopped next_edit llama-server child (if any)");
	}

	let snapshot = state.workspaces.snapshot().await;
	let workspace_id = snapshot.id.clone();
	let bound: Vec<Utf8PathBuf> = snapshot.folders.iter().map(|f| Utf8PathBuf::from(&f.path)).collect();

	// Capture which folders were `Running` *before* we stop
	// them, so the next launch can resurrect exactly those. A
	// folder that's `Paused`, `Failed`, `Stopped`, or `Absent`
	// is deliberately left off the list — the user either took
	// it down on purpose or it was never up; either way,
	// auto-starting it on the next launch would be presumptuous.
	let mut auto_resume: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();

	for folder in &bound {
		match ProjectCompose::for_folder(&workspace_id, folder) {
			Ok(Some(project)) => {
				let was_running = match project.status().await {
					Ok(status) => status.state == ContainerState::Running,
					Err(err) => {
						tracing::warn!(error = %err, folder = %folder, "stop_all: status query failed; will not mark for auto-resume");
						false
					}
				};
				if was_running {
					auto_resume.insert(folder.to_string(), true);
				}
				if let Err(err) = project.stop().await {
					tracing::warn!(error = %err, folder = %folder, "stop_all: per-folder compose stop failed");
				} else {
					tracing::info!(folder = %folder, was_running, "stop_all: stopped per-folder compose");
				}
			}
			Ok(None) => {}
			Err(err) => {
				tracing::warn!(error = %err, folder = %folder, "stop_all: failed to resolve per-folder compose");
			}
		}
	}

	// Persist the auto-resume snapshot. Best-effort: a write
	// failure means the next launch starts with stale flags
	// (worst case: we resume a project the user just took down,
	// or we *don't* resume one they wanted back). Either way
	// the user can recover with one folder-bar click, so we
	// just log and continue.
	if let Err(err) = persist_compose_auto_resume(state, &workspace_id, auto_resume).await {
		tracing::warn!(error = %err, "stop_all: failed to persist compose_auto_resume snapshot");
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

/// On startup, resurrect every per-folder compose project that was
/// `Running` when the previous IDE session quit cleanly.
///
/// The list is read from `WorkspaceSession.compose_auto_resume` in
/// `session.json`, which [`stop_all`] populates immediately before
/// it issues `docker compose stop` on each project. Folders the
/// user had already taken down by hand (Stopped / Down / Paused /
/// Failed) aren't in the map and stay stopped — matching the
/// principle that we only auto-resurrect what was actually
/// running.
///
/// Asymmetry with the workspace shell ([`auto_resume_shell`]):
/// the shell auto-resumes whenever it's `Stopped` regardless of
/// what the previous session looked like, because it's the IDE's
/// substrate. Per-folder projects are scenario-specific and we'd
/// rather under-resume than over-resume.
///
/// Best-effort: a failed `up` is logged at `warn` and the next
/// folder is attempted. The user sees the failure surface in the
/// folder-bar popover when they next open it. Callers should run
/// this *after* [`auto_resume_shell`] so the workspace dev
/// container exists by the time `compose up` tries to attach it
/// to each project's network.
pub async fn auto_resume_project_composes(state: &AppState) {
	let snapshot = state.workspaces.snapshot().await;
	let workspace_id = snapshot.id.clone();

	let session = match moon_core::session::load(&state.workspaces_dir, &workspace_id).await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "auto_resume_project_composes: session load failed");
			return;
		}
	};

	if session.compose_auto_resume.is_empty() {
		return;
	}

	// Iterate bound folders rather than the map's keys so a
	// stale entry for a folder the user has since unbound
	// doesn't trigger a confusing `compose up` against a
	// directory that isn't part of the workspace anymore.
	for folder in &snapshot.folders {
		let key = folder.path.clone();
		if !session.compose_auto_resume.get(&key).copied().unwrap_or(false) {
			continue;
		}
		let folder_path = Utf8PathBuf::from(&folder.path);
		let project = match ProjectCompose::for_folder(&workspace_id, &folder_path) {
			Ok(Some(p)) => p,
			Ok(None) => {
				tracing::warn!(folder = %folder_path, "auto_resume_project_composes: marked but no compose file found; skipping");
				continue;
			}
			Err(err) => {
				tracing::warn!(error = %err, folder = %folder_path, "auto_resume_project_composes: resolve failed");
				continue;
			}
		};

		// If the daemon already has the project running (last
		// session crashed without shutdown), `up` is a no-op
		// at the docker layer — skip the round-trip anyway to
		// keep startup snappy.
		match project.status().await {
			Ok(status) if status.state == ContainerState::Running => {
				tracing::info!(folder = %folder_path, "auto_resume_project_composes: already running, skipping");
				continue;
			}
			Ok(_) => {}
			Err(err) => {
				tracing::warn!(error = %err, folder = %folder_path, "auto_resume_project_composes: status query failed, attempting up anyway");
			}
		}

		tracing::info!(folder = %folder_path, "auto_resume_project_composes: resuming per-folder compose");
		if let Err(err) = project.up().await {
			tracing::warn!(error = %err, folder = %folder_path, "auto_resume_project_composes: up failed");
		}
	}
}

/// Overwrite the `compose_auto_resume` slot in this workspace's
/// `session.json`, preserving every other field. Read-modify-write
/// because the frontend's persist tick might land between our read
/// and our write — we still need to surface our snapshot, but we
/// must not clobber the frontend's `folders` / `active_folder_path`
/// or any other backend-managed field in the meantime.
async fn persist_compose_auto_resume(
	state: &AppState,
	workspace_id: &str,
	auto_resume: std::collections::BTreeMap<String, bool>,
) -> moon_protocol::MoonResult<()> {
	let mut session = moon_core::session::load(&state.workspaces_dir, workspace_id).await?;
	session.compose_auto_resume = auto_resume;
	moon_core::session::save(&state.workspaces_dir, workspace_id, &session).await
}

/// Clear a single folder's `compose_auto_resume` flag.
///
/// Called by the per-folder `Stop` / `Down` Tauri commands so that
/// quitting from a deliberately-stopped state doesn't auto-resume
/// next launch. Best-effort: a failure is logged but doesn't
/// propagate up — the user's stop succeeded, which is what they
/// actually clicked.
pub async fn clear_compose_auto_resume(state: &AppState, workspace_id: &str, folder_path: &str) {
	let mut session = match moon_core::session::load(&state.workspaces_dir, workspace_id).await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, folder = %folder_path, "clear_compose_auto_resume: load failed");
			return;
		}
	};
	if session.compose_auto_resume.remove(folder_path).is_none() {
		return;
	}
	if let Err(err) = moon_core::session::save(&state.workspaces_dir, workspace_id, &session).await {
		tracing::warn!(error = %err, folder = %folder_path, "clear_compose_auto_resume: save failed");
	}
}
