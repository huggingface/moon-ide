//! Desktop-side controls for a remote workspace window (ADR 0095).

use moon_app::attach::RemoteStatus;
use moon_protocol::MoonError;
use tauri::State;

use crate::state::{AppState, Backend};

/// Latest connection status, or `None` for a local workspace. The
/// webview asks on load because it may have missed the first
/// `remote:status` events.
#[tauri::command]
pub async fn remote_status(state: State<'_, AppState>) -> Result<Option<RemoteStatus>, MoonError> {
	match &state.backend {
		Backend::Local(_) => Ok(None),
		Backend::Remote(remote) => Ok(Some(remote.status())),
	}
}

/// Take the workspace back after another desktop attached to it.
#[tauri::command]
pub async fn remote_reattach(state: State<'_, AppState>) -> Result<(), MoonError> {
	let Backend::Remote(remote) = &state.backend else {
		return Err(MoonError::invalid("not a remote workspace"));
	};
	remote.client.reattach();
	Ok(())
}
