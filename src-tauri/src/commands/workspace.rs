use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::app_state as core_app_state;
use moon_protocol::workspace::{slugify_workspace_name, validate_workspace_id, WorkspaceMeta};
use moon_protocol::MoonError;
use tauri::{Manager, State};

use crate::focus_socket;
use crate::state::AppState;

/// Catalog of every workspace the user has on this machine,
/// sorted most-recently-active first. Drives the picker palette
/// (Phase 7.8) and the launcher's restore pick (Phase 7.9).
#[tauri::command]
pub async fn workspace_catalog(state: State<'_, AppState>) -> Result<Vec<WorkspaceMeta>, MoonError> {
	let mut metas = core_app_state::load(&state.config_dir).await?.workspaces;
	metas.sort_by_key(|m| std::cmp::Reverse(m.last_active_at));
	Ok(metas)
}

/// Create a workspace, **or return the existing one** if the
/// derived slug is already in the catalog. Pure catalog write
/// — no live registry is touched here, because each workspace
/// lives in its own process. The frontend follows up with
/// `window_open(slug)` to spawn / focus the matching child
/// process; the create-or-switch behaviour means
/// `Ctrl+Shift+N "Hugging Face"` does the right thing whether
/// the user has used that name before or not.
///
/// Pass an empty `slug` to derive it from `name`. The existing
/// entry's `name` is **not** overwritten on a hit — renames go
/// through `workspace_rename`. `last_active_at` is bumped on
/// every call so the picker / launcher recency sort reflects
/// that the user just touched this workspace.
#[tauri::command]
pub async fn workspace_create(
	state: State<'_, AppState>,
	slug: String,
	name: String,
) -> Result<WorkspaceMeta, MoonError> {
	let trimmed_name = name.trim();
	if trimmed_name.is_empty() {
		return Err(MoonError::invalid("workspace name must not be empty"));
	}
	let slug = if slug.is_empty() {
		let derived = slugify_workspace_name(trimmed_name);
		if derived.is_empty() {
			return Err(MoonError::invalid(
				"workspace name has no alphanumeric characters; pick a different name",
			));
		}
		derived
	} else {
		slug
	};
	validate_workspace_id(&slug)?;

	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);

	let name_owned = trimmed_name.to_string();
	let slug_owned = slug.clone();
	let meta = core_app_state::mutate(&state.config_dir, move |s| {
		if let Some(existing) = s.workspaces.iter_mut().find(|m| m.id == slug_owned) {
			existing.last_active_at = now;
			return existing.clone();
		}
		let meta = WorkspaceMeta {
			id: slug_owned,
			name: name_owned,
			last_active_at: now,
			color: None,
		};
		s.workspaces.push(meta.clone());
		meta
	})
	.await?;
	Ok(meta)
}

/// Drop a workspace: refuses if a sibling process currently
/// owns its instance lock (deleting underneath a live window
/// would leave that window pointing at nothing). Otherwise runs
/// `docker compose down` for its compose project, deletes
/// `<workspaces_dir>/<slug>/`, and removes the catalog entry.
#[tauri::command]
pub async fn workspace_delete(state: State<'_, AppState>, slug: String) -> Result<(), MoonError> {
	validate_workspace_id(&slug)?;

	if Some(slug.as_str()) == state.workspace_id() {
		return Err(MoonError::invalid(format!(
			"cannot delete `{slug}`: this window is showing it"
		)));
	}
	if focus_socket::workspace_is_live(&state.workspaces_dir, &slug).await {
		return Err(MoonError::invalid(format!(
			"workspace `{slug}` is open in another window; close it first"
		)));
	}

	let state_dir = state.workspace_state_dir(&slug);

	// Best-effort `compose down`. The compose project might
	// still be running from a previous launch even if the
	// workspace was never re-opened this session. A failure
	// here (e.g. docker daemon unreachable) is logged but not
	// fatal: the catalog removal below still happens so the
	// user can clean up the stray container manually.
	let container = ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: slug.clone(),
		state_dir: state_dir.clone(),
		bound_folders: Vec::new(),
	})?;
	if let Err(err) = container.teardown().await {
		tracing::warn!(error = %err, slug = %slug, "compose down failed during workspace_delete");
	}

	if state_dir.exists() {
		if let Err(err) = tokio::fs::remove_dir_all(state_dir.as_std_path()).await {
			tracing::warn!(error = %err, path = %state_dir, "failed to remove workspace state dir");
		}
	}

	let slug_owned = slug.clone();
	core_app_state::mutate(&state.config_dir, move |s| {
		s.workspaces.retain(|m| m.id != slug_owned);
	})
	.await?;
	Ok(())
}

/// Update the badge colour for a workspace. `color` is either a
/// `#rrggbb` (or `#rgb`) hex string or an empty string to clear
/// back to the deterministic hash-derived colour. When the
/// edited workspace is the one this process owns, the running
/// window's icon repaints immediately — sibling processes
/// repaint on next launch.
///
/// Validation is intentionally light: an unparseable colour is
/// stored as-is and the icon code falls back to the hash hue.
/// That way a partial typo round-trip from the picker doesn't
/// fail loudly; the user just sees the default colour and
/// re-edits.
#[tauri::command]
pub async fn workspace_set_color(
	app: tauri::AppHandle,
	state: State<'_, AppState>,
	slug: String,
	color: String,
) -> Result<WorkspaceMeta, MoonError> {
	validate_workspace_id(&slug)?;
	let trimmed = color.trim();
	let next_color: Option<String> = if trimmed.is_empty() {
		None
	} else {
		Some(trimmed.to_string())
	};
	let slug_owned = slug.clone();
	let color_for_mutate = next_color.clone();
	let meta = core_app_state::mutate(&state.config_dir, move |s| {
		let meta = s
			.workspaces
			.iter_mut()
			.find(|m| m.id == slug_owned)
			.ok_or_else(|| MoonError::NotFound(format!("workspace `{slug_owned}`")))?;
		meta.color = color_for_mutate;
		Ok::<WorkspaceMeta, MoonError>(meta.clone())
	})
	.await??;

	if Some(slug.as_str()) == state.workspace_id() {
		// Route through the agent indicator so a repaint mid-run
		// keeps the activity dot (and the tray icon follows the
		// new colour too).
		if let Some(indicator) = app.try_state::<std::sync::Arc<crate::agent_indicator::AgentIndicator>>() {
			indicator.set_color(next_color.clone());
		}
	}
	Ok(meta)
}

/// Update the human-readable name of a workspace. The id (and
/// therefore the on-disk state dir, the compose project name,
/// and any open process owning it) is immutable.
#[tauri::command]
pub async fn workspace_rename(
	state: State<'_, AppState>,
	slug: String,
	name: String,
) -> Result<WorkspaceMeta, MoonError> {
	let trimmed = name.trim();
	if trimmed.is_empty() {
		return Err(MoonError::invalid("workspace name must not be empty"));
	}
	let new_name = trimmed.to_string();
	let slug_owned = slug.clone();
	core_app_state::mutate(&state.config_dir, move |s| {
		let meta = s
			.workspaces
			.iter_mut()
			.find(|m| m.id == slug_owned)
			.ok_or_else(|| MoonError::NotFound(format!("workspace `{slug_owned}`")))?;
		meta.name = new_name;
		Ok::<WorkspaceMeta, MoonError>(meta.clone())
	})
	.await?
}
