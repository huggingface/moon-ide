//! Tauri commands for cross-cutting UI chrome that doesn't slot into
//! one feature module.
//!
//! Today the only entry is [`ui_set_right_panel`] — the right-side
//! slot is shared between the chat panel and the coder panel (per
//! ADR-style decision in the panel-consolidation change), so the
//! frontend needs one writer that flips the persisted pick *and*
//! feeds the slack poller. Spreading that across two slack-named
//! commands would make it look like a slack feature when it isn't.

use moon_core::app_state as app_state_store;
use moon_protocol::app_state::RightPanelKind;
use moon_protocol::MoonError;
use tauri::State;

use crate::state::AppState;

/// Persist the right-side panel pick and update any side-effects
/// downstream of it. `None` closes the slot entirely.
///
/// Side effects:
/// - The slack poller's `panel_visible` input is the boolean
///   `kind == Some(Chat)`. The poller short-circuits its
///   `conversations.history` ticks when the chat panel isn't on
///   screen, so a stale `true` would burn API budget on a hidden
///   panel.
#[tauri::command]
pub async fn ui_set_right_panel(state: State<'_, AppState>, kind: Option<RightPanelKind>) -> Result<(), MoonError> {
	state
		.slack
		.poller
		.set_panel_visible(matches!(kind, Some(RightPanelKind::Chat)));
	app_state_store::mutate(&state.config_dir, move |s| {
		s.right_panel = kind;
	})
	.await
}
