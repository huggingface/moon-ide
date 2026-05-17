//! Tauri commands for workspace port forwarding.
//!
//! User-facing surface is small: list current declared forwards,
//! replace them as a full set, and snapshot per-port live state
//! for the picker's status dots. Mutating commands persist the
//! requested set to `session.json` (so it survives a relaunch)
//! *and* call [`moon_container::apply_forwards`] so the proxy
//! sidecar reflects the new set immediately. Every successful
//! mutation broadcasts a `ports:state` event so the panel and
//! status-bar entry can refresh without polling.
//!
//! See [`moon_container::port_forward`] for the sidecar
//! mechanism, and `specs/containers.md` § "Port forwarding" for
//! the user-facing model and what's not in scope yet.

use moon_container::{apply_forwards, list_forward_status, project_name_for_id};
use moon_core::session as core_session;
use moon_protocol::ports::{ForwardedPort, ForwardedPortStatus, PortsApplyResult};
use moon_protocol::MoonError;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;

/// Emitted after every successful mutating command (`ports_set`,
/// `ports_clear`). Payload: the latest `Vec<ForwardedPortStatus>`
/// — same shape as `ports_status` so subscribers can drop it
/// straight onto their store.
pub const PORTS_STATE_EVENT: &str = "ports:state";

/// Read the current declared forwards from `session.json`.
///
/// Cheap: a single `session.json` read + parse. Doesn't touch
/// docker. Returns `[]` for processes without a workspace bound
/// (preboot mode) or when the file doesn't exist yet.
#[tauri::command]
pub async fn ports_list(state: State<'_, AppState>) -> Result<Vec<ForwardedPort>, MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(Vec::new());
	};
	let session = core_session::load(&state.workspaces_dir, id).await?;
	Ok(session.forwarded_ports)
}

/// Replace the declared forward set with `forwards`. Validates,
/// persists, then re-creates the proxy sidecar so the new set is
/// served immediately.
///
/// Validation is intentionally minimal — duplicate `host_port`
/// entries and obviously broken values (`0`) are rejected here so
/// the user gets a typed error instead of a `docker run` failure;
/// host-port-busy lands as `conflicts` in the result rather than
/// an error so the picker can tell "your input was wrong" from
/// "the host machine is busy".
///
/// Persistence happens **before** the docker apply. If `apply`
/// fails, the user's intent is still on disk and the next
/// `ports_set` retry (or workspace re-setup) brings the sidecar
/// in line — that matters because the alternative ("docker first,
/// disk second") would let a transient daemon hiccup eat the
/// user's port edits.
#[tauri::command]
pub async fn ports_set(
	app: AppHandle,
	state: State<'_, AppState>,
	forwards: Vec<ForwardedPort>,
) -> Result<PortsApplyResult, MoonError> {
	validate_forwards(&forwards)?;
	let Some(id) = state.workspace_id() else {
		return Err(MoonError::invalid("no workspace bound to this process"));
	};
	let id = id.to_owned();
	persist_forwards(&state.workspaces_dir, &id, forwards.clone()).await?;
	let project = project_name_for_id(&id).map_err(|err| MoonError::invalid(err.to_string()))?;
	let result = apply_forwards(&project, &forwards).await?;
	emit_state(&app, &project, &forwards).await;
	Ok(result)
}

/// Snapshot the live state of every declared forward. Cheap-ish:
/// one `docker inspect` for the proxy sidecar plus N
/// `bind` probes. Run after every workspace-shell lifecycle event
/// the panel cares about (set up / rebuild / teardown), and on
/// initial mount.
///
/// Empty list when the workspace isn't bound or has no forwards
/// declared — keeps the panel a one-liner without a round-trip.
#[tauri::command]
pub async fn ports_status(state: State<'_, AppState>) -> Result<Vec<ForwardedPortStatus>, MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(Vec::new());
	};
	let session = core_session::load(&state.workspaces_dir, id).await?;
	if session.forwarded_ports.is_empty() {
		return Ok(Vec::new());
	}
	let project = project_name_for_id(id).map_err(|err| MoonError::invalid(err.to_string()))?;
	let status = list_forward_status(&project, &session.forwarded_ports).await?;
	Ok(status)
}

fn validate_forwards(forwards: &[ForwardedPort]) -> Result<(), MoonError> {
	let mut seen = std::collections::HashSet::with_capacity(forwards.len());
	for forward in forwards {
		if forward.host_port == 0 || forward.container_port == 0 {
			return Err(MoonError::invalid(
				"port forwards must use a non-zero host and container port",
			));
		}
		if !seen.insert(forward.host_port) {
			return Err(MoonError::invalid(format!(
				"duplicate host port {} in forward set",
				forward.host_port
			)));
		}
	}
	Ok(())
}

/// Round-trip the session through load + save so we don't clobber
/// folders, tabs, SCM filters, or any other field the frontend's
/// `session_save` flow keeps current.
async fn persist_forwards(
	workspaces_dir: &camino::Utf8Path,
	workspace_id: &str,
	forwards: Vec<ForwardedPort>,
) -> Result<(), MoonError> {
	let mut session = core_session::load(workspaces_dir, workspace_id).await?;
	if session.forwarded_ports == forwards {
		return Ok(());
	}
	session.forwarded_ports = forwards;
	core_session::save(workspaces_dir, workspace_id, &session).await
}

async fn emit_state(app: &AppHandle, project: &moon_container::ProjectName, forwards: &[ForwardedPort]) {
	let payload = match list_forward_status(project, forwards).await {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(error = %err, "failed to compute forward status for ports:state event");
			return;
		}
	};
	if let Err(err) = app.emit(PORTS_STATE_EVENT, &payload) {
		tracing::warn!(error = %err, "failed to emit ports:state");
	}
}

/// Re-apply the currently persisted forward set to the proxy
/// sidecar. Used by the frontend after the workspace shell comes
/// up: a fresh shell has no sidecar yet, but the user's
/// `forwarded_ports` from `session.json` are still on disk.
/// Calling this on every container-state-becomes-running event
/// is idempotent — `apply_forwards` always tears down the
/// existing sidecar before starting a new one.
///
/// Empty persisted list = no-op (we don't even probe docker).
#[tauri::command]
pub async fn ports_reapply(app: AppHandle, state: State<'_, AppState>) -> Result<PortsApplyResult, MoonError> {
	let Some(id) = state.workspace_id() else {
		return Ok(PortsApplyResult {
			applied: Vec::new(),
			conflicts: Vec::new(),
		});
	};
	let session = core_session::load(&state.workspaces_dir, id).await?;
	if session.forwarded_ports.is_empty() {
		return Ok(PortsApplyResult {
			applied: Vec::new(),
			conflicts: Vec::new(),
		});
	}
	let project = project_name_for_id(id).map_err(|err| MoonError::invalid(err.to_string()))?;
	let result = apply_forwards(&project, &session.forwarded_ports).await?;
	emit_state(&app, &project, &session.forwarded_ports).await;
	Ok(result)
}
