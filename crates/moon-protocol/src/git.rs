//! Git status surfacing for the file tree. See `specs/roadmap.md`
//! Phase 5 for the end-state plan; this module carries only what the
//! current slice ships:
//!
//! - A lowercase enum matching Pierre Trees' `GitStatus` vocabulary
//!   (`'added' | 'modified' | 'deleted' | 'untracked' | 'ignored'`)
//!   so the TS mirror maps directly without a conversion table.
//! - A `{ path, status }` pair; no staged-vs-worktree split, no
//!   conflict marker, no per-hunk counts. The tree needs one label
//!   per row; more granular views go in the SCM panel later.
//!
//! Renames aren't in the enum on purpose: we ask git with
//! `--no-renames` so a rename lands as `deleted(old)` +
//! `added(new)`. Matches the roadmap's rendering contract ("we don't
//! try to be cleverer than git here") and keeps the frontend model
//! flat.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum GitFileStatus {
	/// New file that's been `git add`'d but not yet committed. Not to
	/// be confused with `Untracked` — an untracked file becomes
	/// `Added` the moment it enters the index.
	Added,
	/// Tracked file with worktree or staged content changes relative
	/// to `HEAD`.
	Modified,
	/// Tracked file that no longer exists on disk, or has been
	/// `git rm`'d into the index. The tree keeps deleted rows visible
	/// (see the roadmap note) even though they have no filesystem
	/// entry, so the backend reports the path even if the frontend
	/// never enumerated it.
	Deleted,
	/// On disk, no rule ignores it, not yet in the index. VSCode and
	/// friends render these at a different tint from `Added`; we
	/// mirror that so "I forgot to `git add`" is a glance away.
	Untracked,
	/// Covered by a `.gitignore` / `.git/info/exclude` rule and not
	/// in the index. Entire directories may be reported collapsed
	/// (`target/`); the tree fades those rows without expanding.
	Ignored,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusEntry {
	/// Workspace-relative path, directories terminated with `/` to
	/// match the `read_dir` and tree conventions. Deleted entries
	/// never carry a trailing slash — git can only delete files, not
	/// whole directories, in this representation.
	pub path: String,
	pub status: GitFileStatus,
}
