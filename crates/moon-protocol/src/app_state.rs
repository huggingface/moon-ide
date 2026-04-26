//! Persisted, machine-local app state. Owned by moon-core's
//! [`moon_core::app_state`] storage layer, but the *shape* lives here so
//! the frontend and the backend agree on it byte-for-byte over IPC.
//!
//! There is deliberately no `Settings` type. Project-level code style
//! (indentation, EOL, charset) is delegated to `.editorconfig` from
//! Phase 1.5 onward; everything else moon-ide stores about a user is
//! per-machine and lives here. Per AGENTS.md "no premature migrations":
//! we change this struct freely until the roadmap is done — there are no
//! aliases or fallbacks.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::session::WorkspaceSession;
use crate::theme::ThemeMode;

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default, deny_unknown_fields)]
pub struct AppState {
	/// What was on screen at the last successful save: workspace, tabs,
	/// active pane. Restored verbatim on next launch when the workspace
	/// folder still exists.
	pub last_session: Option<WorkspaceSession>,
	/// Active UI theme. Per-machine; survives workspace switches.
	pub theme: ThemeMode,
}
