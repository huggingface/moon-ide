//! Workspace registration and identification.
//!
//! Phase 2.5 onward: a workspace is the bag that holds zero or
//! more folders the user has bound into one moon-ide session.
//! The folder is what the user actually points at on disk; the
//! workspace is the container that gives every folder its tab
//! strip / file tree / container indicator.
//!
//! Phase 7 + ADR 0014: the user can name multiple workspaces
//! (`huggingface` / `gitaly` / …) and each runs in its own
//! `moon-ide --workspace <slug>` OS process. See
//! [`specs/roadmaps/phase-02.5-multi-folder.md`](../../../specs/roadmaps/phase-02.5-multi-folder.md)
//! for the multi-folder shape and
//! [`specs/roadmaps/phase-07-multi-workspace.md`](../../../specs/roadmaps/phase-07-multi-workspace.md)
//! for the multi-workspace + process-per-workspace details.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Unique opaque ID for a registered workspace — a user-picked
/// slug like `huggingface` / `gitaly`. Same string that ends up
/// in `moon-ws-<id>` (compose project name), the per-workspace
/// state dir, and the `moon-ide --workspace <id>` CLI arg, so
/// it must pass [`validate_workspace_id`].
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

/// Catalog entry for a workspace the user has on this machine.
/// Held in `AppState.workspaces` alongside theme + Slack creds —
/// distinct from the live [`Workspace`] snapshot, which carries
/// folder + active-folder state for the running process's
/// workspace.
///
/// The catalog is empty until the user names their first
/// workspace in preboot mode; from there on the picker
/// (Phase 7.8) and the launcher (Phase 7.9) read this list to
/// drive the create / focus / restore-most-recent flows.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMeta {
	/// Stable id — the slug the user picked at creation time.
	/// Same string that ends up in `moon-ws-<id>` and the
	/// per-workspace state dir, so it must pass
	/// [`validate_workspace_id`].
	pub id: WorkspaceId,
	/// Human-readable label the user typed at creation time. Free
	/// text — distinct from `id` so renaming the display name
	/// doesn't move the on-disk state dir.
	pub name: String,
	/// Last time anything in this workspace was touched, as Unix
	/// epoch seconds. Phase 7.9 uses this to pick which workspace
	/// the launcher restores; Phase 7.8's picker uses it for the
	/// "recent" sort. Bumped on `workspace_create`, `window_open`,
	/// and every `session_save` tick.
	pub last_active_at: i64,
	/// User-chosen badge colour as `#rrggbb`, applied to this
	/// workspace's per-window icon (alt-tab differentiation). `None`
	/// means "use the deterministic hash-derived hue" — i.e. the
	/// default colour every new workspace starts with. `#[serde(default)]`
	/// keeps state.json forward-compatible: pre-colour catalogs
	/// load cleanly and lazily promote to `None` on the next save.
	#[serde(default)]
	#[ts(optional)]
	pub color: Option<String>,
}

/// The running process's single workspace, holding zero or
/// more folders, with at most one currently active. Each
/// `moon-ide --workspace <slug>` process owns exactly one
/// `Workspace` for its lifetime (ADR 0014).
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

/// Maximum length we accept for a workspace slug. Compose project
/// names tolerate longer strings, but at this size `moon-ws-<slug>`
/// stays readable in `docker ps` output and the OS window label.
pub const MAX_WORKSPACE_SLUG_LEN: usize = 32;

/// Validate a workspace id (the slug that ends up in
/// `moon-ws-<id>`, the per-workspace state dir, and the window
/// label). Mirrors the `[a-z0-9_-]` charset Docker compose
/// project names accept, with the extra constraint that the
/// first character must be alphanumeric so the slug never reads
/// as a flag (`-foo`) anywhere it gets concatenated.
///
/// Returns `Ok(())` for valid slugs and an `InvalidArgument`
/// error otherwise. The empty string is rejected.
pub fn validate_workspace_id(id: &str) -> Result<(), crate::MoonError> {
	if id.is_empty() {
		return Err(crate::MoonError::invalid("workspace id must not be empty"));
	}
	if id.len() > MAX_WORKSPACE_SLUG_LEN {
		return Err(crate::MoonError::invalid(format!(
			"workspace id is longer than {MAX_WORKSPACE_SLUG_LEN} characters"
		)));
	}
	let mut chars = id.chars();
	let first = chars.next().expect("non-empty id checked above");
	if !first.is_ascii_alphanumeric() {
		return Err(crate::MoonError::invalid(
			"workspace id must start with a letter or digit",
		));
	}
	for ch in std::iter::once(first).chain(chars) {
		let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_';
		if !ok {
			return Err(crate::MoonError::invalid(format!(
				"workspace id contains invalid character `{ch}` (allowed: a-z 0-9 - _)"
			)));
		}
	}
	Ok(())
}

/// Best-effort conversion of a free-text workspace name into a
/// valid slug: lowercases, replaces every run of non-`[a-z0-9]`
/// with a single `-`, trims leading/trailing `-`, and clamps to
/// [`MAX_WORKSPACE_SLUG_LEN`]. Returns the empty string when no
/// alphanumeric characters are present — callers should fall
/// back to a default in that case (e.g. let the user pick a
/// different name).
pub fn slugify_workspace_name(name: &str) -> String {
	let mut out = String::new();
	let mut prev_dash = true;
	for ch in name.chars() {
		let lower = ch.to_ascii_lowercase();
		if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
			out.push(lower);
			prev_dash = false;
			continue;
		}
		if !prev_dash && out.len() < MAX_WORKSPACE_SLUG_LEN {
			out.push('-');
			prev_dash = true;
		}
	}
	while out.ends_with('-') {
		out.pop();
	}
	while out.starts_with('-') {
		out.remove(0);
	}
	if out.len() > MAX_WORKSPACE_SLUG_LEN {
		out.truncate(MAX_WORKSPACE_SLUG_LEN);
		while out.ends_with('-') {
			out.pop();
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn validate_accepts_canonical_slugs() {
		validate_workspace_id("default").unwrap();
		validate_workspace_id("huggingface").unwrap();
		validate_workspace_id("moon-base").unwrap();
		validate_workspace_id("project_42").unwrap();
		validate_workspace_id("a").unwrap();
	}

	#[test]
	fn validate_rejects_bad_slugs() {
		assert!(validate_workspace_id("").is_err());
		assert!(validate_workspace_id("-foo").is_err());
		assert!(validate_workspace_id("_foo").is_err());
		assert!(validate_workspace_id("Foo").is_err());
		assert!(validate_workspace_id("foo bar").is_err());
		assert!(validate_workspace_id("foo/bar").is_err());
		assert!(validate_workspace_id("café").is_err());
		assert!(validate_workspace_id(&"a".repeat(MAX_WORKSPACE_SLUG_LEN + 1)).is_err());
	}

	#[test]
	fn slugify_handles_typical_names() {
		assert_eq!(slugify_workspace_name("Hugging Face"), "hugging-face");
		assert_eq!(slugify_workspace_name(" moon ide "), "moon-ide");
		assert_eq!(slugify_workspace_name("ACME Corp."), "acme-corp");
		assert_eq!(slugify_workspace_name("moon--ide"), "moon-ide");
		assert_eq!(slugify_workspace_name("---"), "");
		assert_eq!(slugify_workspace_name("héllo wörld"), "h-llo-w-rld");
	}

	#[test]
	fn slugify_clamps_long_names() {
		let long = "a".repeat(100);
		let slug = slugify_workspace_name(&long);
		assert!(slug.len() <= MAX_WORKSPACE_SLUG_LEN);
		validate_workspace_id(&slug).unwrap();
	}
}
