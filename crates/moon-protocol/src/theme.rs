//! UI theme. Per-user / per-machine, not per-workspace, which is why it
//! lives in [`crate::app_state::AppState`] alongside the persisted UI
//! session — see `specs/decisions/0006-no-settings-file.md` for the
//! reasoning.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, Default, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
	#[default]
	Dark,
	Light,
}
