//! Workspace registration and identification.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Unique opaque ID for a registered workspace. Currently a UUID-shaped string.
pub type WorkspaceId = String;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Workspace {
	pub id: WorkspaceId,
	pub name: String,
	pub root: String,
	pub host: HostKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
	/// Workspace lives directly on the user's host filesystem.
	Local,
	/// Workspace lives inside a devcontainer; ops route through `moon-agent`.
	Devcontainer,
}
