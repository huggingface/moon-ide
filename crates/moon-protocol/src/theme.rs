//! UI theme. Per-user / per-machine, not per-workspace, which is why it
//! lives in [`crate::app_state::AppState`] alongside the persisted UI
//! session — see `specs/decisions/0006-no-settings-file.md` for the
//! reasoning.
//!
//! `System` resolves to dark or light at render time by consulting the
//! OS preference. The protocol stores the user's _choice_ (system /
//! dark / light), not the resolved value — otherwise the app couldn't
//! react to an OS theme flip without a save.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, Default, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
	/// Follow the OS's `prefers-color-scheme`. Default for new
	/// installs so moon-ide matches whatever the user already has
	/// configured system-wide on first launch.
	#[default]
	System,
	Dark,
	Light,
}

/// Resolved OS colour-scheme preference. Emitted by the desktop shell
/// via the `system_theme` Tauri command; `Unspecified` is the third
/// XDG portal value that we treat as "fall back to dark" on the
/// frontend.
///
/// Separate from [`ThemeMode`] because the OS can't express "follow
/// system" (it _is_ the system) and because conflating the two would
/// mask the `Unspecified` case we actually need to see.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum SystemTheme {
	Dark,
	Light,
	Unspecified,
}
