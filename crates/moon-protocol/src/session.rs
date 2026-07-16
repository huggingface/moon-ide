//! UI session state — the bag of "what was on screen last time" we
//! persist between launches: which folders, which one was active,
//! which tabs in each.
//!
//! This is **not** user-configurable like `Settings`. It is owned by
//! the frontend; the backend just stores and returns it. We type it
//! instead of using opaque JSON so cross-version mistakes show up at
//! compile time. Per AGENTS.md "no premature migrations": we change
//! this freely until the roadmap is done.

use std::collections::BTreeMap;

use crate::coder_hub::CoderHubBucket;
use crate::coder_mcp::CoderMcpWorkspaceConfig;
use crate::coder_models::CoderProviderLock;
use crate::git::{CompareBaseline, PrListScope};
use crate::ports::ForwardedPort;
use crate::review::{ReviewComment, ReviewedFile};
use crate::workspace::FolderOrigin;
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
#[serde(default)]
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
	/// Branch-switcher PR-section filter for this folder. Persisted
	/// per folder so flipping to "Participating" on a busy
	/// monorepo doesn't drag a sleepy side-project's palette into
	/// participating-only mode too. Defaults to
	/// [`PrListScope::All`] for fresh sessions and for sessions
	/// written by older builds (`#[serde(default)]`).
	pub pr_scope: PrListScope,
	/// SCM compare baseline for this folder. `Default` makes the
	/// file tree, change gutter, and diff view show "what this
	/// branch / PR changes versus main"; `Head` is the regular
	/// "what's modified since the last commit". Persisted per
	/// folder for the same reason as `pr_scope`.
	pub compare_baseline: CompareBaseline,
	/// Local-first review-comment drafts for this folder (Phase
	/// 5.7). Persisted until published to a GitHub PR and then
	/// cleared. `#[serde(default)]` on the struct fills an empty
	/// vec for sessions written by older builds.
	pub review_comments: Vec<ReviewComment>,
	/// Per-file "Viewed" marks for this folder (Phase 5.7).
	/// Pinned to the ticked version's blob SHA; the frontend
	/// drops stale entries on git-status refresh. Never published.
	pub reviewed_files: Vec<ReviewedFile>,
	/// How this folder came to be bound (ADR 0028). Persisted so a
	/// worktree-backed coder session's checkout is re-bound as a
	/// nested worktree folder on next launch rather than promoted to
	/// a plain top-level folder. Defaults to
	/// [`FolderOrigin::UserPicked`] for folders written by older
	/// builds.
	pub origin: FolderOrigin,
}

/// Dummy `Default` so `#[serde(default)]` on the struct can fill
/// in a fresh `FolderSession` if the on-disk JSON is missing
/// fields. Per AGENTS.md "no premature migrations": on disk we
/// rely on field-level defaults and tolerate missing fields,
/// rather than write migration code.
impl Default for FolderSession {
	fn default() -> Self {
		Self {
			folder_path: String::new(),
			open_files_left: Vec::new(),
			open_files_right: Vec::new(),
			active_left: None,
			active_right: None,
			has_split: false,
			focused_side: SplitSide::Left,
			pr_scope: PrListScope::default(),
			compare_baseline: CompareBaseline::default(),
			review_comments: Vec::new(),
			reviewed_files: Vec::new(),
			origin: FolderOrigin::default(),
		}
	}
}

/// Persisted UI session for one workspace. Holds one
/// [`FolderSession`] per bound folder, plus a pointer to which folder
/// was active at last save. Lives at
/// `<workspaces_dir>/<id>/session.json` from Phase 7.5 onward —
/// previously it was `AppState.last_session` in the global
/// `state.json`, and the IDE wipes that legacy slot on first run.
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
	/// Per-workspace lock on the coder's active provider. When set,
	/// the runner ignores
	/// [`crate::app_state::CoderAppState::active_provider`] for
	/// this workspace and uses the locked value — so toggling the
	/// global default from another workspace's modal doesn't bleed
	/// into a workspace the user pinned. `None` (the default) means
	/// "follow the global active_provider, just like before".
	///
	/// This is per-workspace because a single user often runs
	/// different repos against different providers (e.g. one repo
	/// always against Anthropic for cache-friendliness, another
	/// happily flipping between HF and OpenRouter). Storing the
	/// lock here rather than in `AppState` keeps the global default
	/// genuinely global while letting individual workspaces opt out.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[ts(optional, type = "CoderProviderLock | null")]
	pub coder_provider_lock: Option<CoderProviderLock>,
	/// User-declared host-to-dev port forwards. Each entry is
	/// served by the workspace's proxy sidecar
	/// (`moon-ws-<id>-ports-1`); the sidecar is recreated
	/// whenever this list changes — the dev container itself
	/// stays untouched, so terminals + any in-flight
	/// `bun dev` survive port edits.
	///
	/// Empty list = no sidecar running. Persisted in
	/// `session.json` so the user's per-workspace mappings
	/// (workspace A: `3000 -> 3000`; workspace B:
	/// `3001 -> 3000`) survive restarts and don't fight over
	/// the host's port space across workspaces. See
	/// [`crate::ports`] for the wire shape.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub forwarded_ports: Vec<ForwardedPort>,
	/// HF Hub bucket bound to this workspace, if any. When set,
	/// the runner pushes session JSONLs to the bucket on
	/// `TurnEnded` (autosync) or on the user's explicit "Upload"
	/// click (manual). See [`crate::coder_hub`] for the
	/// destination shape (`<namespace>/<name>` on the Hub) and
	/// the per-session `uploaded` cache. `None` (the default)
	/// means "no Hub binding; sessions stay local".
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[ts(optional, type = "CoderHubBucket | null")]
	pub coder_hub_bucket: Option<CoderHubBucket>,
	/// Per-workspace MCP server state: which servers (preset or
	/// custom) are enabled here, plus any user-defined custom
	/// servers. Backend-managed like the other coder fields —
	/// `session_save`'s merge keeps the on-disk value through
	/// frontend persist ticks; the `coder_mcp_*` commands are the
	/// only writers. See [`crate::coder_mcp`].
	#[serde(default, skip_serializing_if = "CoderMcpWorkspaceConfig::is_empty")]
	pub coder_mcp: CoderMcpWorkspaceConfig,
	/// Per-folder "this folder's `docker-compose.yml` project was
	/// `Running` when the IDE quit last time" map, keyed by the
	/// folder's absolute path. `shutdown::stop_all` populates
	/// this immediately before issuing `docker compose stop` for
	/// each folder; on next launch,
	/// `shutdown::auto_resume_project_composes` reads it and
	/// brings the marked folders back up with `compose up -d`.
	///
	/// Cleared (for a folder) by Stop / Down clicks in the UI so
	/// quitting from a deliberately-stopped state doesn't
	/// auto-resurrect the project. Backend-managed: the
	/// `session_save` merge in `commands::session` keeps the
	/// on-disk value through frontend persist ticks. A missing
	/// entry, or `false`, means "don't auto-start"; that's also
	/// what happens after a corrupt-session fallback, which is
	/// the right safe default.
	#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
	pub compose_auto_resume: BTreeMap<String, bool>,
}
