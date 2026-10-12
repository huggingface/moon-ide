use serde_json::Value;
use tauri::{AppHandle, Emitter};

/// Delivers `moon-app` events to this process's webview.
pub struct TauriEvents(pub AppHandle);

impl moon_app::EventSink for TauriEvents {
	fn emit(&self, event: &str, payload: Value) {
		if let Err(err) = self.0.emit(event, payload) {
			tracing::warn!(error = %err, event, "failed to emit event");
		}
	}
}
