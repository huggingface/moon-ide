//! The workspace process's command surface, independent of how it is
//! reached (ADR 0095). The desktop serves it in-process through one
//! Tauri command; a remote workspace serves the same dispatcher over
//! SSH. Desktop-shell actions (windows, dialogs, opener, clipboard)
//! are deliberately not here: they always run on the user's desktop.

use std::sync::Arc;

use moon_core::WorkspaceRegistry;
use moon_protocol::MoonError;
use serde::de::DeserializeOwned;
use serde_json::Value;

mod fs;

/// State every dispatched command runs against. Grows as command
/// groups move out of `src-tauri` (Phase 16.2).
pub struct AppCore {
	pub workspaces: Arc<WorkspaceRegistry>,
}

impl AppCore {
	pub fn new(workspaces: Arc<WorkspaceRegistry>) -> Self {
		Self { workspaces }
	}

	/// Run `method` with its JSON `params` (an object keyed by the
	/// camelCase argument names, same shape Tauri's `invoke` takes).
	pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, MoonError> {
		if let Some(result) = fs::dispatch(self, method, params).await {
			return result;
		}
		Err(MoonError::invalid(format!("unknown method `{method}`")))
	}
}

fn parse_params<T: DeserializeOwned>(method: &str, params: Value) -> Result<T, MoonError> {
	let params = if params.is_null() {
		Value::Object(serde_json::Map::new())
	} else {
		params
	};
	serde_json::from_value(params).map_err(|e| MoonError::invalid(format!("{method}: bad params: {e}")))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, MoonError> {
	serde_json::to_value(value).map_err(|e| MoonError::internal(e.to_string()))
}

/// Generates a module's `dispatch`: one arm per command, deserialising
/// the camelCase params object into the listed arguments. `[app]`
/// before the arguments passes the [`AppCore`] through.
macro_rules! commands {
	($( $name:ident ( $([$app:ident])? $( $arg:ident : $ty:ty ),* ) ),* $(,)?) => {
		pub(crate) async fn dispatch(
			app: &crate::AppCore,
			method: &str,
			params: serde_json::Value,
		) -> Option<Result<serde_json::Value, moon_protocol::MoonError>> {
			match method {
				$( stringify!($name) => Some(async {
					#[derive(serde::Deserialize)]
					#[serde(rename_all = "camelCase")]
					#[allow(dead_code)]
					struct Args { $( $arg: $ty ),* }
					#[allow(unused_variables)]
					let Args { $( $arg ),* } = crate::parse_params::<Args>(method, params)?;
					crate::to_value($name($(crate::pass_app!($app, app),)? $( $arg ),*).await?)
				}.await), )*
				_ => None,
			}
		}
	};
}
pub(crate) use commands;

/// Emits the dispatcher's own `app` binding for a command marked
/// `[app]` (macro hygiene keeps the caller's spelling from naming it).
macro_rules! pass_app {
	($marker:ident, $app:expr) => {
		$app
	};
}
pub(crate) use pass_app;

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn app() -> AppCore {
		AppCore::new(Arc::new(WorkspaceRegistry::new("test-workspace".into())))
	}

	#[tokio::test]
	async fn dispatches_camel_case_params_to_the_command() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a.txt");
		let path = path.to_str().unwrap();
		let app = app();
		app
			.dispatch("fs_write_file_host", json!({ "path": path, "text": "hi" }))
			.await
			.unwrap();
		let read = app
			.dispatch("fs_read_file_host", json!({ "path": path }))
			.await
			.unwrap();
		assert_eq!(read["text"], "hi");
	}

	#[tokio::test]
	async fn rejects_unknown_methods_and_bad_params() {
		let app = app();
		let unknown = app.dispatch("nope", Value::Null).await.unwrap_err();
		assert!(unknown.to_string().contains("unknown method `nope`"), "{unknown}");
		let bad = app.dispatch("fs_read_file_host", json!({})).await.unwrap_err();
		assert!(bad.to_string().contains("fs_read_file_host: bad params"), "{bad}");
	}
}
