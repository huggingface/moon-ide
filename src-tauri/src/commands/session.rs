//! Tauri commands wrapping [`moon_core::session`].
//!
//! One `session.json` per workspace, holding the per-workspace
//! UI state (folders bound, open tabs, splits, focused folder,
//! SCM filters). Loaded on hydrate, saved on every persist
//! tick.
//!
//! Process-per-workspace: each process owns one workspace and
//! reads/writes its own `session.json`. No `workspace_id`
//! parameter: the file path is derived from
//! `state.workspace_id()`.

use moon_core::session as core_session;
use moon_protocol::session::WorkspaceSession;
use moon_protocol::MoonError;
use tauri::State;

use crate::commands::window::bump_last_active;
use crate::state::AppState;

fn require_workspace_id(state: &AppState) -> Result<&str, MoonError> {
	state
		.workspace_id()
		.ok_or_else(|| MoonError::invalid("session: no workspace bound to this process"))
}

#[tauri::command]
pub async fn session_load(state: State<'_, AppState>) -> Result<WorkspaceSession, MoonError> {
	let id = require_workspace_id(&state)?;
	core_session::load(&state.workspaces_dir, id).await
}

/// Overlay the frontend's UI-only fields onto the backend-managed
/// slice of `WorkspaceSession`. Pure helper so the merge
/// invariant is testable without an `AppState`.
///
/// The frontend is authoritative for `folders` (bound folder
/// list + per-folder open files / splits) and `active_folder_path`.
/// Every other field on [`WorkspaceSession`] is **backend-managed**
/// (`coder_hub_bucket`, `coder_provider_lock`, `forwarded_ports`)
/// and gets written through its own dedicated Tauri command —
/// `coder_hub_create_bucket` / `coder_hub_set_autosync` /
/// `coder_hub_disconnect`, `coder_set_workspace_provider_lock`,
/// `ports_set`.
///
/// We can't trust the frontend to round-trip those fields
/// accurately because (a) it doesn't know about them at every
/// callsite that triggers a persist tick, and (b) even when it
/// does, a fast `setHubAutosync` → `persistAppState` race would
/// land the stale value. So we read the on-disk session, keep
/// its backend-managed fields, and overlay the frontend's payload
/// on top. The frontend's view of those fields is ignored on
/// purpose.
fn merge_frontend_session(existing: WorkspaceSession, frontend: WorkspaceSession) -> WorkspaceSession {
	WorkspaceSession {
		folders: frontend.folders,
		active_folder_path: frontend.active_folder_path,
		coder_provider_lock: existing.coder_provider_lock,
		forwarded_ports: existing.forwarded_ports,
		coder_hub_bucket: existing.coder_hub_bucket,
		compose_auto_resume: existing.compose_auto_resume,
	}
}

#[tauri::command]
pub async fn session_save(state: State<'_, AppState>, session: WorkspaceSession) -> Result<(), MoonError> {
	let id = require_workspace_id(&state)?.to_owned();
	let existing = core_session::load(&state.workspaces_dir, &id).await?;
	let merged = merge_frontend_session(existing, session);
	core_session::save(&state.workspaces_dir, &id, &merged).await?;
	// Every persist tick is meaningful activity for the
	// workspace — bumping `last_active_at` here means the
	// "most-recently-active" sort tracks real usage rather than
	// just process-launch events.
	bump_last_active(&state, &id).await;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::coder_hub::CoderHubBucket;
	use moon_protocol::coder_models::CoderProviderLock;
	use moon_protocol::ports::ForwardedPort;
	use moon_protocol::session::{FolderSession, SplitSide};

	fn frontend(folder: &str) -> WorkspaceSession {
		WorkspaceSession {
			folders: vec![FolderSession {
				folder_path: folder.into(),
				open_files_left: vec!["src/main.rs".into()],
				open_files_right: vec![],
				active_left: Some("src/main.rs".into()),
				active_right: None,
				has_split: false,
				focused_side: SplitSide::Left,
				..Default::default()
			}],
			active_folder_path: Some(folder.into()),
			coder_provider_lock: None,
			forwarded_ports: Vec::new(),
			coder_hub_bucket: None,
			compose_auto_resume: Default::default(),
		}
	}

	fn backend_with_extras() -> WorkspaceSession {
		WorkspaceSession {
			folders: Vec::new(),
			active_folder_path: None,
			coder_provider_lock: Some(CoderProviderLock::Hf),
			forwarded_ports: vec![ForwardedPort {
				container_port: 3000,
				host_port: 3000,
				label: "vite".into(),
			}],
			coder_hub_bucket: Some(CoderHubBucket {
				namespace: "alice".into(),
				name: "my-workspace-traces".into(),
				private: true,
				autosync: true,
				uploaded: Default::default(),
			}),
			compose_auto_resume: {
				let mut m = std::collections::BTreeMap::new();
				m.insert("/home/me/work".to_string(), true);
				m
			},
		}
	}

	#[test]
	fn merge_keeps_backend_managed_fields_when_frontend_omits_them() {
		// The frontend payload (constructed by `persistAppState`
		// in `state.svelte.ts`) only fills in `folders` +
		// `active_folder_path`. Without the merge those nulled
		// defaults would clobber every backend-managed slot —
		// the user would lose their HF bucket binding, port
		// forwards, and provider lock on every folder switch.
		let existing = backend_with_extras();
		let payload = frontend("/home/me/work");
		let merged = merge_frontend_session(existing.clone(), payload);

		assert_eq!(merged.coder_hub_bucket, existing.coder_hub_bucket);
		assert_eq!(merged.coder_provider_lock, existing.coder_provider_lock);
		assert_eq!(merged.forwarded_ports, existing.forwarded_ports);
		assert_eq!(merged.compose_auto_resume, existing.compose_auto_resume);
		assert_eq!(merged.active_folder_path.as_deref(), Some("/home/me/work"));
		assert_eq!(merged.folders.len(), 1);
		assert_eq!(merged.folders[0].folder_path, "/home/me/work");
	}

	#[test]
	fn merge_overwrites_ui_fields_with_frontend_payload() {
		// The complement of the above: when the frontend payload
		// updates `folders` / `active_folder_path`, those land
		// verbatim — backend-managed fields don't get to veto
		// the frontend's own state.
		let existing = WorkspaceSession {
			folders: vec![FolderSession {
				folder_path: "/stale/path".into(),
				..Default::default()
			}],
			active_folder_path: Some("/stale/path".into()),
			..Default::default()
		};
		let payload = frontend("/fresh/path");
		let merged = merge_frontend_session(existing, payload);
		assert_eq!(merged.active_folder_path.as_deref(), Some("/fresh/path"));
		assert_eq!(merged.folders.len(), 1);
		assert_eq!(merged.folders[0].folder_path, "/fresh/path");
	}

	#[test]
	fn merge_ignores_backend_managed_fields_sent_by_frontend() {
		// A future / buggy frontend that sent a `coder_hub_bucket`
		// in its payload must not be able to override the
		// authoritative on-disk value. The merge always sources
		// those fields from `existing`.
		let existing = backend_with_extras();
		let mut payload = frontend("/home/me/work");
		payload.coder_hub_bucket = Some(CoderHubBucket {
			namespace: "attacker".into(),
			name: "wrong-bucket".into(),
			private: false,
			autosync: false,
			uploaded: Default::default(),
		});
		payload.forwarded_ports = vec![];
		payload.coder_provider_lock = None;
		payload.compose_auto_resume = std::collections::BTreeMap::new();
		let merged = merge_frontend_session(existing.clone(), payload);
		assert_eq!(merged.coder_hub_bucket, existing.coder_hub_bucket);
		assert_eq!(merged.forwarded_ports, existing.forwarded_ports);
		assert_eq!(merged.coder_provider_lock, existing.coder_provider_lock);
		assert_eq!(merged.compose_auto_resume, existing.compose_auto_resume);
	}
}
