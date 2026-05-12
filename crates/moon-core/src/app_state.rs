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
//! ## Concurrent writers
//!
//! Phase 7 ships one process per workspace. Every running `moon-ide`
//! shares the same `state.json` (catalog, theme, slack slice, …) and
//! competes for it. Inside one process, several Tauri commands also
//! race — periodic `bump_last_active`, the frontend's
//! `app_state_save`, slack writes, etc. all do load → mutate → save.
//!
//! A naive `load()` then `save()` loses updates whenever two writers
//! interleave: A reads, B reads, A writes, B writes — A's mutation
//! is gone. The user-visible symptom is "I created a workspace in
//! window B, but the picker in window A doesn't show it".
//!
//! [`mutate`] is the only safe way to change `state.json`:
//!
//! - Acquires an exclusive cross-process advisory lock on a sidecar
//!   `state.json.lock` file via `flock(2)`. Sibling processes block
//!   until the lock is free.
//! - Loads the current state from disk inside the lock.
//! - Runs the caller's mutator on the freshly-loaded state.
//! - Writes a temporary file and renames it atomically over
//!   `state.json` so readers (which don't take the lock) always see
//!   a complete document.
//! - Drops the lock.
//!
//! Read-only callers can keep using [`load`]; the atomic rename means
//! they can never observe a half-written file.
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

fn lock_path(config_dir: &Utf8Path) -> Utf8PathBuf {
	config_dir.join("state.json.lock")
}

fn tmp_path(config_dir: &Utf8Path) -> Utf8PathBuf {
	config_dir.join("state.json.tmp")
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

/// Load → mutate → save under a cross-process exclusive lock, with an
/// atomic rename on the way out. See the module docs for why this is
/// the only correct way to write `state.json`.
///
/// `mutator` runs synchronously inside `spawn_blocking` because
/// `flock(2)` is a blocking syscall — we don't want to park a tokio
/// worker thread on a contended lock. It receives `&mut AppState` and
/// returns a value the caller wants to extract from the locked region
/// (e.g. the cloned [`WorkspaceMeta`] that just got inserted).
///
/// On a parse error of an existing file we fall back to defaults, just
/// like [`load`]. The next save will overwrite the corrupted document
/// — better than refusing to start.
pub async fn mutate<F, R>(config_dir: &Utf8Path, mutator: F) -> MoonResult<R>
where
	F: FnOnce(&mut AppState) -> R + Send + 'static,
	R: Send + 'static,
{
	let cfg = config_dir.to_owned();
	tokio::task::spawn_blocking(move || mutate_blocking(&cfg, mutator))
		.await
		.map_err(|e| MoonError::Internal(format!("app state mutate join error: {e}")))?
}

fn mutate_blocking<F, R>(config_dir: &Utf8Path, mutator: F) -> MoonResult<R>
where
	F: FnOnce(&mut AppState) -> R,
{
	std::fs::create_dir_all(config_dir.as_std_path()).map_err(MoonError::from)?;

	let lock_file = std::fs::OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.open(lock_path(config_dir).as_std_path())
		.map_err(MoonError::from)?;
	// `File::lock` is `flock(2)` on unix, `LockFileEx` on windows
	// — both blocking and advisory. Released automatically when
	// `lock_file` drops at end of scope.
	lock_file.lock().map_err(MoonError::from)?;

	let state_path = state_path(config_dir);
	let mut state = if state_path.exists() {
		let text = std::fs::read_to_string(state_path.as_std_path()).map_err(MoonError::from)?;
		match serde_json::from_str::<AppState>(&text) {
			Ok(s) => s,
			Err(e) => {
				tracing::warn!(error = %e, path = %state_path, "app state parse failed; starting from defaults");
				AppState::default()
			}
		}
	} else {
		AppState::default()
	};

	let r = mutator(&mut state);

	let text =
		serde_json::to_string_pretty(&state).map_err(|e| MoonError::Internal(format!("app state serialize error: {e}")))?;
	let tmp = tmp_path(config_dir);
	std::fs::write(tmp.as_std_path(), text).map_err(MoonError::from)?;
	std::fs::rename(tmp.as_std_path(), state_path.as_std_path()).map_err(MoonError::from)?;

	Ok(r)
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::theme::ThemeMode;
	use tempfile::TempDir;

	#[tokio::test]
	async fn load_default_when_missing() {
		let dir = TempDir::new().unwrap();
		let cfg = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let s = load(&cfg).await.unwrap();
		assert!(s.workspaces.is_empty());
		assert_eq!(s.theme, ThemeMode::System);
	}

	#[tokio::test]
	async fn mutate_then_load_roundtrip() {
		let dir = TempDir::new().unwrap();
		let cfg = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

		mutate(&cfg, |s| {
			s.theme = ThemeMode::Light;
		})
		.await
		.unwrap();

		let loaded = load(&cfg).await.unwrap();
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
		assert_eq!(s.theme, ThemeMode::System);
	}

	#[tokio::test]
	async fn concurrent_mutate_no_lost_update() {
		// Two concurrent mutators on the same `state.json` must both
		// land. Without the lock, one of them silently overwrites the
		// other — exactly the bug Phase 7 process-per-workspace ran
		// into when window A's `bump_last_active` clobbered window
		// B's freshly-created workspace entry.
		use moon_protocol::workspace::WorkspaceMeta;
		let dir = TempDir::new().unwrap();
		let cfg = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

		let cfg_a = cfg.clone();
		let a = tokio::spawn(async move {
			for i in 0..50 {
				mutate(&cfg_a, move |s| {
					s.workspaces.push(WorkspaceMeta {
						id: format!("a-{i}"),
						name: format!("a-{i}"),
						last_active_at: i,
						color: None,
					});
				})
				.await
				.unwrap();
			}
		});
		let cfg_b = cfg.clone();
		let b = tokio::spawn(async move {
			for i in 0..50 {
				mutate(&cfg_b, move |s| {
					s.workspaces.push(WorkspaceMeta {
						id: format!("b-{i}"),
						name: format!("b-{i}"),
						last_active_at: i,
						color: None,
					});
				})
				.await
				.unwrap();
			}
		});
		a.await.unwrap();
		b.await.unwrap();

		let final_state = load(&cfg).await.unwrap();
		assert_eq!(final_state.workspaces.len(), 100);
	}
}
