use serde::Serialize;
use serde_json::Value;

/// Where the workspace process's push events go: the desktop webview
/// in-process, or the attached desktop over SSH for a remote
/// workspace. Event names and payloads are the frontend's contract.
pub trait EventSink: Send + Sync {
	fn emit(&self, event: &str, payload: Value);
}

/// Serialise and emit; a payload that can't serialise is logged and
/// dropped rather than failing the caller.
pub fn emit<T: Serialize + ?Sized>(sink: &dyn EventSink, event: &str, payload: &T) {
	match serde_json::to_value(payload) {
		Ok(value) => sink.emit(event, value),
		Err(err) => tracing::warn!(error = %err, event, "failed to serialise event payload"),
	}
}
