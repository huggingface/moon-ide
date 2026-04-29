//! UI session state — the bag of "what was on screen last time" we
//! persist between launches: which folders, which one was active,
//! which tabs in each.
//!
//! This is **not** user-configurable like `Settings`. It is owned by
//! the frontend; the backend just stores and returns it. We type it
//! instead of using opaque JSON so cross-version mistakes show up at
//! compile time. Per AGENTS.md "no premature migrations": we change
//! this freely until the roadmap is done.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum SplitSide {
	Left,
	Right,
}

/// One folder's slice of UI state. Multiple of these live inside a
/// [`WorkspaceSession`] — the user's tabs/active-pane state is per
/// folder, swapping when the active bar changes. Phase 2.5 ships
/// multi-folder UX; before that, this list always had exactly one
/// entry but the wire shape stays the same.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FolderSession {
	/// Absolute path of the folder on the host. Used to re-bind the
	/// folder on next launch and to invalidate stale entries when the
	/// user opens a different folder.
	pub folder_path: String,
	/// Tabs open in the left pane, in tab order. May reference files
	/// that no longer exist; the frontend filters those out at restore
	/// time.
	pub open_files_left: Vec<String>,
	/// Tabs open in the right pane, in tab order. Empty when no split
	/// is active. The two lists are independent — a file can live in
	/// one pane, both, or neither (VSCode/Zed convention).
	pub open_files_right: Vec<String>,
	/// Active tab on the left pane, if any. Must appear in `open_files_left`.
	pub active_left: Option<String>,
	/// Active tab on the right pane, if any. `None` when the split is
	/// closed; `Some` only makes sense alongside `has_split = true`.
	pub active_right: Option<String>,
	pub has_split: bool,
	pub focused_side: SplitSide,
}

/// Persisted UI session for the singleton workspace. Holds one
/// [`FolderSession`] per bound folder, plus a pointer to which folder
/// was active at last save.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(default)]
pub struct WorkspaceSession {
	/// Folders bound into the workspace, in insertion order — same
	/// order the folder bars render in.
	pub folders: Vec<FolderSession>,
	/// Absolute path of the active folder, if any. Must match one of
	/// `folders[].folder_path` when set.
	pub active_folder_path: Option<String>,
}
