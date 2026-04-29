//! Storage layer for [`moon_protocol::app_state::AppState`].
//!
//! AppState is moon-ide's per-machine, per-user scratchpad: which
//! workspace was open last, which tabs were on screen, which theme to
//! paint. There is no separate `Settings` file — see
//! `specs/decisions/0006-no-settings-file.md`. Project-level code style
//! lives in `.editorconfig` from Phase 1.5.
//!
//! The on-disk path is decided by the caller (the Tauri shell uses
//! `app.path().app_config_dir()`); this module only knows how to read
//! and write `state.json` inside whatever directory it is given.
//!
//! Per AGENTS.md "no premature migrations": a corrupt or schema-drifted
//! file is not worth crashing for. Log a warning, fall back to defaults,
//! and let the next save heal it.

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::app_state::AppState;
use moon_protocol::{MoonError, MoonResult};

pub fn state_path(config_dir: &Utf8Path) -> Utf8PathBuf {
	config_dir.join("state.json")
}

pub async fn load(config_dir: &Utf8Path) -> MoonResult<AppState> {
	let path = state_path(config_dir);
	if !path.exists() {
		return Ok(AppState::default());
	}
	let text = tokio::fs::read_to_string(path.as_std_path())
		.await
		.map_err(MoonError::from)?;
	match serde_json::from_str::<AppState>(&text) {
		Ok(state) => Ok(state),
		Err(e) => {
			tracing::warn!(error = %e, path = %path, "app state parse failed; ignoring");
			Ok(AppState::default())
		}
	}
}

pub async fn save(config_dir: &Utf8Path, state: &AppState) -> MoonResult<()> {
	let path = state_path(config_dir);
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent.as_std_path())
			.await
			.map_err(MoonError::from)?;
	}
	let text =
		serde_json::to_string_pretty(state).map_err(|e| MoonError::Internal(format!("app state serialize error: {e}")))?;
	tokio::fs::write(path.as_std_path(), text)
		.await
		.map_err(MoonError::from)?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::session::{FolderSession, SplitSide, WorkspaceSession};
	use moon_protocol::theme::ThemeMode;
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
			}],
			active_folder_path: Some("/tmp/example".into()),
		}
	}

	#[tokio::test]
	async fn load_default_when_missing() {
		let dir = TempDir::new().unwrap();
		let cfg = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let s = load(&cfg).await.unwrap();
		assert!(s.last_session.is_none());
		assert_eq!(s.theme, ThemeMode::Dark);
	}

	#[tokio::test]
	async fn save_then_load_roundtrip() {
		let dir = TempDir::new().unwrap();
		let cfg = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

		let s = AppState {
			last_session: Some(sample_session()),
			theme: ThemeMode::Light,
			..Default::default()
		};
		save(&cfg, &s).await.unwrap();

		let loaded = load(&cfg).await.unwrap();
		let session = loaded.last_session.expect("session present");
		assert_eq!(session.folders.len(), 1);
		let folder = &session.folders[0];
		assert_eq!(folder.folder_path, "/tmp/example");
		assert_eq!(folder.open_files_left, vec!["src/main.rs", "Cargo.toml"]);
		assert!(folder.open_files_right.is_empty());
		assert_eq!(folder.active_left.as_deref(), Some("src/main.rs"));
		assert_eq!(session.active_folder_path.as_deref(), Some("/tmp/example"));
		assert_eq!(loaded.theme, ThemeMode::Light);
	}

	#[tokio::test]
	async fn corrupt_state_falls_back_to_default() {
		let dir = TempDir::new().unwrap();
		let cfg = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		tokio::fs::write(state_path(&cfg).as_std_path(), b"not json")
			.await
			.unwrap();

		let s = load(&cfg).await.unwrap();
		assert!(s.last_session.is_none());
		assert_eq!(s.theme, ThemeMode::Dark);
	}
}
