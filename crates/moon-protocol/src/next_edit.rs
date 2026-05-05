//! Local autocomplete (Sweep-style “next-edit”) via a llama.cpp HTTP server.
//!
//! Prompt layout follows Sweep's public description of the Qwen2.5-Coder
//! next-edit format (file separators + original / current / updated
//! windows). See <https://blog.sweep.dev/posts/oss-next-edit>.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Default listen port for managed next-edit `llama-server` (dynamic/private range; avoids common dev ports like 8080).
pub const DEFAULT_NEXT_EDIT_SERVER_PORT: u16 = 53281;

/// Default Hugging Face repo for `llama-server --hf-repo` (Sweep next-edit model family).
pub const DEFAULT_NEXT_EDIT_HF_REPO: &str = "sweepai/sweep-next-edit-1.5B";

/// Managed `llama-server` spawn settings plus an optional HTTP override when the user runs the server themselves.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct NextEditAppState {
	/// When non-empty, probes and completion use this URL instead of `http://{server_host}:{server_port}`.
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub external_base_url: String,
	/// Executable name (`llama-server`) or absolute path to the binary.
	pub llama_binary: String,
	/// Hugging Face repo for `--hf-repo` (e.g. `sweepai/sweep-next-edit-1.5B` or `org/repo:Q4_K_M`).
	pub hf_repo: String,
	pub server_host: String,
	pub server_port: u16,
	/// Managed mode only: start `llama-server` on IDE launch. Cleared when the user stops from the UI.
	#[serde(default)]
	pub server_autostart: bool,
}

impl Default for NextEditAppState {
	fn default() -> Self {
		let port = DEFAULT_NEXT_EDIT_SERVER_PORT;
		Self {
			external_base_url: String::new(),
			llama_binary: String::new(),
			hf_repo: DEFAULT_NEXT_EDIT_HF_REPO.to_string(),
			server_host: "127.0.0.1".to_string(),
			server_port: port,
			server_autostart: false,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
#[ts(rename_all = "camelCase")]
pub struct NextEditServerStartParams {
	pub llama_binary: String,
	pub hf_repo: String,
	pub server_host: String,
	pub server_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
#[ts(rename_all = "camelCase")]
pub struct NextEditServerSnapshot {
	pub running: bool,
	pub pid: Option<u32>,
	pub last_exit_code: Option<i32>,
	pub start_error: Option<String>,
	pub log_tail: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum NextEditProbeKind {
	/// `GET /health` returned 200 — model is loaded.
	Ready,
	/// TCP failure, timeout, or DNS — nothing listening at the URL.
	Unreachable,
	/// HTTP 503 — llama-server is up but the model is still loading.
	ModelLoading,
	/// Any other HTTP status or unexpected response body.
	Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NextEditProbeResult {
	pub kind: NextEditProbeKind,
	pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
#[ts(rename_all = "camelCase")]
pub struct NextEditCompleteParams {
	pub base_url: String,
	/// Path relative to the active workspace folder, forward slashes.
	pub relative_path: String,
	/// Zero-based line index of the caret line.
	pub cursor_line: u32,
	pub document_text: String,
	/// `git show HEAD:<path>` text when available; feeds the `original` window.
	pub head_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct NextEditCompleteResult {
	/// Predicted text for the edited line range (newline-separated).
	pub replacement: String,
	pub from_line: u32,
	pub to_line: u32,
}
