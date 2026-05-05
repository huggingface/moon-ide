//! Workspace registration and identification.
//!
//! Phase 2.5 onward: a workspace is the singleton bag (`"default"`)
//! that holds zero or more folders the user has bound into a single
//! moon-ide session. The folder is what the user actually points at on
//! disk; the workspace is the container that gives every folder its
//! tab strip / file tree / future container indicator. See
//! [`specs/roadmaps/phase-02.5-multi-folder.md`](../../../specs/roadmaps/phase-02.5-multi-folder.md).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Unique opaque ID for a registered workspace. Currently fixed to
/// `"default"`; Phase 7 grows this to multiple named workspaces.
pub type WorkspaceId = String;

/// One folder bound into a workspace.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct WorkspaceFolder {
	/// Absolute, canonicalised path on the host.
	pub path: String,
	/// Display label (basename of `path` at add-time). Folder rename
	/// is a Phase 7 follow-up, so this is fixed for the folder's life
	/// in the workspace.
	pub name: String,
	pub host: HostKind,
}

/// The full workspace shape: a singleton `"default"` workspace
/// holding zero or more folders, with at most one currently active.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Workspace {
	pub id: WorkspaceId,
	/// Insertion order. Drives the folder-bar order in the sidebar.
	pub folders: Vec<WorkspaceFolder>,
	/// Absolute path of the currently active folder. Always matches
	/// some `folders[].path` when set; `None` only when the workspace
	/// is empty.
	pub active_folder: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
	/// Folder lives directly on the user's host filesystem.
	Local,
	/// Folder lives inside a devcontainer; ops route through `moon-remote`
	/// (or, for local docker, through bind-mount + `docker exec`).
	Devcontainer,
}
