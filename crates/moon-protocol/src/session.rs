//! UI session state — the bag of "what was on screen last time" we
//! persist between launches: which workspace, which tabs, which one was
//! active.
//!
//! This is **not** user-configurable like `Settings`. It is owned by the
//! frontend; the backend just stores and returns it. We type it instead
//! of using opaque JSON so cross-version mistakes show up at compile
//! time. Per AGENTS.md "no premature migrations": we change this freely
//! until the roadmap is done.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum SplitSide {
	Left,
	Right,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorkspaceSession {
	/// Absolute path of the workspace folder. Used to re-open the same
	/// workspace on the next launch and to invalidate stale sessions
	/// when the user opens a different folder.
	pub workspace_path: String,
	/// Paths of files that were open in tabs, in tab order, relative to
	/// `workspace_path`. May reference files that no longer exist; the
	/// frontend filters those out at restore time.
	pub open_files: Vec<String>,
	/// Active tab on the left pane, if any. Must appear in `open_files`.
	pub active_left: Option<String>,
	/// Active tab on the right pane, if any. `None` when the split is
	/// closed; `Some` only makes sense alongside `has_split = true`.
	pub active_right: Option<String>,
	pub has_split: bool,
	pub focused_side: SplitSide,
}
