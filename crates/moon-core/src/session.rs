//! Storage layer for [`moon_protocol::session::WorkspaceSession`].
//!
//! One `session.json` per workspace, living under
//! `<workspaces_dir>/<id>/`. Holds the per-workspace UI session
//! blob — folders bound into the workspace, open tabs and splits
//! per folder, focused side, the SCM compare baseline, the PR
//! list scope. Pure frontend-owned data; the backend just
//! persists the JSON-serialised shape and hands it back on next
//! launch.
//!
//! Phase 7.5 lifts this out of the global `state.json` (where
//! it lived in `AppState.last_session`) and into a per-workspace
//! file so multi-workspace launches don't fight over one slot.
//!
//! Per AGENTS.md "no premature migrations": a corrupt or
//! schema-drifted file is not worth crashing for. Log a warning,
//! fall back to a default session, the next save heals it. The
//! legacy `state.json` field is gone — the user re-opens their
//! folders once.

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::session::WorkspaceSession;
use moon_protocol::{MoonError, MoonResult};

/// Absolute path of `session.json` for `workspace_id` under
/// `workspaces_dir`. Only computed; the directory itself is
/// created lazily on the next [`save`] call.
pub fn session_path(workspaces_dir: &Utf8Path, workspace_id: &str) -> Utf8PathBuf {
	workspaces_dir.join(workspace_id).join("session.json")
}

/// Read the workspace's persisted session, or a default
/// (empty-folder-list) one if there's no file on disk yet or the
/// file failed to parse. Errors only on truly unexpected I/O
/// failures — a missing file or a malformed JSON document both
/// resolve to `Default::default()` so the user gets a fresh start
/// instead of a hard error.
pub async fn load(workspaces_dir: &Utf8Path, workspace_id: &str) -> MoonResult<WorkspaceSession> {
	let path = session_path(workspaces_dir, workspace_id);
	if !path.exists() {
		return Ok(WorkspaceSession::default());
	}
	let bytes = tokio::fs::read(path.as_std_path()).await.map_err(MoonError::from)?;
	// A torn write (crash mid-save) can leave arbitrary bytes on
	// disk, not just malformed JSON. Lossy-decode so that case
	// lands in the same warn-and-default branch below instead of
	// erroring — an error here would also abort every
	// load-then-save caller, so the corrupt file could never heal.
	let text = String::from_utf8_lossy(&bytes);
	match serde_json::from_str::<WorkspaceSession>(&text) {
		Ok(session) => Ok(session),
		Err(e) => {
			tracing::warn!(error = %e, path = %path, "workspace session parse failed; ignoring");
			Ok(WorkspaceSession::default())
		}
	}
}

/// Persist `session` to disk, creating the per-workspace
/// directory if necessary. Pretty-prints the JSON so a human
/// debugging the on-disk file isn't reading a one-liner.
pub async fn save(workspaces_dir: &Utf8Path, workspace_id: &str, session: &WorkspaceSession) -> MoonResult<()> {
	let path = session_path(workspaces_dir, workspace_id);
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent.as_std_path())
			.await
			.map_err(MoonError::from)?;
	}
	let text = serde_json::to_string_pretty(session)
		.map_err(|e| MoonError::Internal(format!("workspace session serialize error: {e}")))?;
	// Write-fsync-rename so a crash mid-save can never leave a
	// torn `session.json`: readers see either the old document or
	// the new one, never garbage. The fsync before the rename
	// matters — without it the rename can land while the data
	// blocks never hit disk, which is exactly the all-garbage
	// file this replaced `fs::write` used to produce.
	let tmp = path.with_extension("json.tmp");
	{
		use tokio::io::AsyncWriteExt;
		let mut file = tokio::fs::File::create(tmp.as_std_path())
			.await
			.map_err(MoonError::from)?;
		file.write_all(text.as_bytes()).await.map_err(MoonError::from)?;
		file.sync_all().await.map_err(MoonError::from)?;
	}
	tokio::fs::rename(tmp.as_std_path(), path.as_std_path())
		.await
		.map_err(MoonError::from)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::session::{FolderSession, SplitSide};
	use tempfile::TempDir;

	fn sample_session() -> WorkspaceSession {
		WorkspaceSession {
			folders: vec![FolderSession {
				folder_path: "/tmp/example".into(),
				open_files_left: vec!["src/main.rs".into(), "Cargo.toml".into()],
				open_files_right: vec![],
				active_left: Some("src/main.rs".into()),
				active_right: None,
				has_split: false,
				focused_side: SplitSide::Left,
				..Default::default()
			}],
			active_folder_path: Some("/tmp/example".into()),
			terminals: Vec::new(),
			coder_provider_lock: None,
			forwarded_ports: Vec::new(),
			coder_hub_bucket: None,
			coder_mcp: Default::default(),
			compose_auto_resume: Default::default(),
		}
	}

	#[tokio::test]
	async fn load_default_when_missing() {
		let dir = TempDir::new().unwrap();
		let workspaces_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let s = load(&workspaces_dir, "default").await.unwrap();
		assert!(s.folders.is_empty());
		assert!(s.active_folder_path.is_none());
	}

	#[tokio::test]
	async fn save_then_load_roundtrip() {
		let dir = TempDir::new().unwrap();
		let workspaces_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		save(&workspaces_dir, "default", &sample_session()).await.unwrap();

		let loaded = load(&workspaces_dir, "default").await.unwrap();
		assert_eq!(loaded.folders.len(), 1);
		let folder = &loaded.folders[0];
		assert_eq!(folder.folder_path, "/tmp/example");
		assert_eq!(folder.open_files_left, vec!["src/main.rs", "Cargo.toml"]);
		assert!(folder.open_files_right.is_empty());
		assert_eq!(folder.active_left.as_deref(), Some("src/main.rs"));
		assert_eq!(loaded.active_folder_path.as_deref(), Some("/tmp/example"));
	}

	#[tokio::test]
	async fn corrupt_session_falls_back_to_default() {
		let dir = TempDir::new().unwrap();
		let workspaces_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let path = session_path(&workspaces_dir, "default");
		tokio::fs::create_dir_all(path.parent().unwrap().as_std_path())
			.await
			.unwrap();
		tokio::fs::write(path.as_std_path(), b"not json").await.unwrap();

		let s = load(&workspaces_dir, "default").await.unwrap();
		assert!(s.folders.is_empty());
		assert!(s.active_folder_path.is_none());
	}

	#[tokio::test]
	async fn non_utf8_session_falls_back_to_default() {
		// A torn write after a crash leaves arbitrary bytes, not
		// just malformed JSON. This must default like the corrupt
		// case, not error — an error would block every
		// load-then-save caller from ever healing the file.
		let dir = TempDir::new().unwrap();
		let workspaces_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let path = session_path(&workspaces_dir, "default");
		tokio::fs::create_dir_all(path.parent().unwrap().as_std_path())
			.await
			.unwrap();
		tokio::fs::write(path.as_std_path(), [0x90, 0x1b, 0xa9, 0xff, 0xfe])
			.await
			.unwrap();

		let s = load(&workspaces_dir, "default").await.unwrap();
		assert!(s.folders.is_empty());

		// And the next save heals the file in place.
		save(&workspaces_dir, "default", &sample_session()).await.unwrap();
		let healed = load(&workspaces_dir, "default").await.unwrap();
		assert_eq!(healed.folders.len(), 1);
	}

	#[tokio::test]
	async fn save_creates_workspace_directory() {
		let dir = TempDir::new().unwrap();
		let workspaces_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		save(&workspaces_dir, "huggingface", &sample_session()).await.unwrap();

		let path = session_path(&workspaces_dir, "huggingface");
		assert!(path.exists());
		assert!(path.parent().unwrap().is_dir());
	}
}
