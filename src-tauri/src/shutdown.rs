//! Graceful shutdown of the desktop process. The workspace side
//! (terminals, coder processes, LSP, compose projects) lives in
//! [`moon_app::AppCore::stop_all`]; next-edit is desktop-local.

use crate::state::AppState;

pub async fn stop_all(state: &AppState) {
	if let Err(err) = state.next_edit_server.stop().await {
		tracing::warn!(error = %err, "stop_all: next_edit_server stop failed");
	} else {
		tracing::info!("stop_all: stopped next_edit llama-server child (if any)");
	}
	state.core.stop_all().await;
}
