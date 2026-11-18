//! The coder's read-only Slack connection (ADR 0122).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The Slack workspace and user behind a moon-ide workspace's Slack
/// token. The token itself never leaves the OS keyring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackConnection {
	/// Slack workspace name.
	pub team: String,
	/// The user the token reads as.
	pub user: String,
	/// The Slack workspace's URL (`https://<team>.slack.com/`).
	pub url: String,
}

/// What the settings UI shows about Slack for this workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SlackStatus {
	/// `None` when the workspace has no token.
	pub connection: Option<SlackConnection>,
	/// User-token scopes the Slack app needs, for the setup steps.
	pub scopes: Vec<String>,
}
