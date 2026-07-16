//! MCP (Model Context Protocol) server configuration for the coder.
//!
//! The coder exposes MCP servers through a meta-tool surface
//! (`mcp_list_tools` / `mcp_call`) rather than advertising every
//! server tool directly — see `specs/coder.md` § "MCP servers" and
//! ADR 0033. A small curated preset list lives in
//! `moon-coder::mcp`; user-defined servers and the per-workspace
//! enable set persist here, on
//! [`crate::session::WorkspaceSession::coder_mcp`].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where an MCP server process runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum McpRunTarget {
	/// Spawned directly on the host — the right default for
	/// servers that need host resources (playwright needs a real
	/// browser, which moon-base doesn't ship).
	#[default]
	Host,
	/// Spawned via `docker exec -i` inside the workspace shell
	/// container when it's running (falls back to host otherwise,
	/// same posture as the `bash` tool).
	Container,
}

/// One MCP server definition — either a hardcoded preset
/// (moon-coder's `mcp::preset_servers`) or a user-defined custom
/// entry persisted per workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct McpServerConfig {
	/// Stable id — the preset's well-known name (`playwright`) or
	/// an opaque `mcp-<unix-ms>` for custom entries.
	pub id: String,
	/// Human label for the settings UI.
	pub label: String,
	/// Executable to spawn (stdio transport only).
	pub command: String,
	/// Arguments passed verbatim.
	pub args: Vec<String>,
	/// Host or container spawn — per-server, not global.
	pub runs: McpRunTarget,
	/// 1-2 sentences surfaced to the model in the meta-tool
	/// descriptions so it knows when the server is worth a
	/// `mcp_list_tools` round-trip.
	pub description: String,
}

impl Default for McpServerConfig {
	fn default() -> Self {
		Self {
			id: String::new(),
			label: String::new(),
			command: String::new(),
			args: Vec::new(),
			runs: McpRunTarget::Host,
			description: String::new(),
		}
	}
}

/// Per-workspace MCP state, persisted on
/// [`crate::session::WorkspaceSession`]. Per-workspace by design:
/// enabling playwright is a statement about one project's testing
/// needs, not a global preference, and custom servers are usually
/// project-specific.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct CoderMcpWorkspaceConfig {
	/// Ids (preset or custom) enabled for this workspace. Empty =
	/// the meta-tools aren't advertised at all.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub enabled: Vec<String>,
	/// User-defined servers for this workspace.
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub custom: Vec<McpServerConfig>,
}

impl CoderMcpWorkspaceConfig {
	pub fn is_empty(&self) -> bool {
		self.enabled.is_empty() && self.custom.is_empty()
	}
}

/// One row of the settings UI's server list: a preset or custom
/// server plus its enabled state for the current workspace.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct McpServerStatus {
	#[serde(flatten)]
	#[ts(flatten)]
	pub config: McpServerConfig,
	/// `true` for hardcoded presets (not removable).
	pub preset: bool,
	/// Enabled for the current workspace.
	pub enabled: bool,
}
