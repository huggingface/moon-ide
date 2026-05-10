//! `app_info` — the launcher answer the frontend reads on
//! hydrate. Process-per-workspace makes the answer fixed at
//! startup: either we're in a real workspace or we're the
//! preboot landing process whose only job is to collect a
//! workspace name from the user.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::workspace::WorkspaceId;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
	/// `"workspace"` when this process is bound to a real
	/// workspace; `"preboot"` for the first-launch landing
	/// process.
	pub mode: AppInfoMode,
	/// Workspace slug this process owns. `None` in preboot
	/// mode.
	pub workspace_id: Option<WorkspaceId>,
	/// Human-readable workspace name from the catalog.
	/// `None` in preboot mode and as a defensive fallback
	/// when the catalog and the CLI arg disagree.
	pub workspace_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum AppInfoMode {
	Workspace,
	Preboot,
}
