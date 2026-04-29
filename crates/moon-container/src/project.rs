//! Workspace ID → compose project name.
//!
//! Compose's `name:` key disambiguates concurrent projects on a
//! single Docker daemon (it prefixes container names, the project
//! network, and the `docker compose` filter that lifecycle commands
//! use). Pre-2.5 we derived it from a hash of the workspace's path
//! — fine when "the workspace" was just whichever folder the user
//! had open, brittle the moment a workspace contains _multiple_
//! folders (the project would have churned every time the active
//! folder switched). Post-2.5 the workspace has a stable identity
//! of its own, so the workspace shell project name is just
//! `moon-ws-<id>`.
//!
//! For per-folder project services (each bound folder's own
//! `docker-compose.yml`), the project name is
//! `moon-ws-<id>-<folder-slug>` — same prefix so a single
//! `docker compose ls --filter name=moon-ws-default-` enumerates
//! everything the workspace owns. See [`project_name_for_folder`]
//! for the slug rules.
//!
//! For now the IDE only ever uses the literal workspace ID
//! `default`; multi-workspace support (Phase 7) will introduce
//! more, at which point the validation here is the gate that
//! makes sure those IDs survive a `docker compose -p ...`
//! interpolation without quoting.

use std::fmt;

use thiserror::Error;

/// A validated Docker compose project name (`moon-ws-<id>`).
///
/// Construct via [`project_name_for_id`]; never store an arbitrary
/// string in this type — its existence is the proof that the
/// name is safe to interpolate into a `docker compose -p ...`
/// command line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ProjectName(String);

impl ProjectName {
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for ProjectName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

/// Reasons a workspace ID can't be turned into a project name.
///
/// The ID is the user-visible (Phase 7) handle for a workspace,
/// so we reject inputs that compose itself would refuse before
/// they reach the daemon — gives a clean error at the IDE
/// boundary instead of an opaque docker-compose stderr.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectNameError {
	#[error("workspace id must not be empty")]
	Empty,

	#[error("workspace id {id:?} contains an invalid character; allowed: lowercase letters, digits, '-', '_'")]
	InvalidChar { id: String },

	#[error("workspace id {id:?} must start with a lowercase letter or digit")]
	InvalidStart { id: String },
}

/// Derive the compose project name for a workspace.
///
/// Compose project names must match `^[a-z0-9][a-z0-9_-]*$`
/// (Docker enforces this at command time). We pre-validate so a
/// bad ID surfaces as a typed error in the lifecycle layer
/// rather than as a `docker compose failed (exit ...)` noise
/// downstream.
pub fn project_name_for_id(workspace_id: &str) -> Result<ProjectName, ProjectNameError> {
	if workspace_id.is_empty() {
		return Err(ProjectNameError::Empty);
	}
	// Validate the full set first; that way an input like
	// "Default" surfaces as `InvalidChar` (the actionable
	// problem) rather than `InvalidStart`, even though its first
	// character also fails the start rule.
	for c in workspace_id.chars() {
		if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
			return Err(ProjectNameError::InvalidChar {
				id: workspace_id.to_owned(),
			});
		}
	}
	let first = workspace_id.chars().next().expect("non-empty checked above");
	if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
		return Err(ProjectNameError::InvalidStart {
			id: workspace_id.to_owned(),
		});
	}
	Ok(ProjectName(format!("moon-ws-{workspace_id}")))
}

/// Derive the compose project name for a folder's own services
/// (its `docker-compose.yml`), namespaced under the workspace.
///
/// `folder_basename` is the folder's filesystem basename (e.g.
/// `moon-landing`). It's slugified (lowercase, non-alnum → `-`)
/// before being appended; collisions across two folders with the
/// same slug are the registry layer's problem to refuse and out
/// of scope here.
pub fn project_name_for_folder(workspace_id: &str, folder_basename: &str) -> Result<ProjectName, ProjectNameError> {
	// Validate the workspace half via the existing helper —
	// reusing the same error variants keeps the upstream
	// surface narrow.
	let _ = project_name_for_id(workspace_id)?;
	let slug = folder_slug(folder_basename);
	Ok(ProjectName(format!("moon-ws-{workspace_id}-{slug}")))
}

/// Lower-case ASCII slug suitable for the suffix of a compose
/// project name. Anything outside `[a-z0-9_-]` collapses to `-`,
/// runs of `-` are collapsed, leading dashes/digits-only inputs
/// are normalised, and an empty result falls back to `folder` so
/// the caller never has to handle that edge case.
pub fn folder_slug(input: &str) -> String {
	let mut out = String::with_capacity(input.len());
	let mut last_dash = true; // start "after a dash" so we trim leading dashes
	for c in input.chars() {
		let ch = if c.is_ascii_uppercase() {
			c.to_ascii_lowercase()
		} else {
			c
		};
		if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' {
			out.push(ch);
			last_dash = false;
		} else if !last_dash {
			out.push('-');
			last_dash = true;
		}
	}
	while out.ends_with('-') {
		out.pop();
	}
	if out.is_empty() {
		return "folder".to_owned();
	}
	// Compose project names must start with `[a-z0-9]` — if the
	// slug accidentally starts with `_` (rare, but possible if
	// the basename was something like `_scratch`) prefix it.
	if !out
		.chars()
		.next()
		.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
	{
		out.insert(0, 'f');
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_id_yields_expected_name() {
		let name = project_name_for_id("default").unwrap();
		assert_eq!(name.as_str(), "moon-ws-default");
	}

	#[test]
	fn name_is_stable_across_calls() {
		let a = project_name_for_id("scratch").unwrap();
		let b = project_name_for_id("scratch").unwrap();
		assert_eq!(a, b);
	}

	#[test]
	fn distinct_ids_yield_distinct_names() {
		let a = project_name_for_id("default").unwrap();
		let b = project_name_for_id("scratch").unwrap();
		assert_ne!(a, b);
	}

	#[test]
	fn empty_id_is_rejected() {
		assert_eq!(project_name_for_id(""), Err(ProjectNameError::Empty));
	}

	#[test]
	fn id_with_uppercase_is_rejected() {
		assert!(matches!(
			project_name_for_id("Default"),
			Err(ProjectNameError::InvalidChar { .. }),
		));
	}

	#[test]
	fn id_starting_with_dash_is_rejected() {
		assert!(matches!(
			project_name_for_id("-foo"),
			Err(ProjectNameError::InvalidStart { .. }),
		));
	}

	#[test]
	fn id_with_dot_is_rejected() {
		// Compose would refuse `moon-ws-foo.bar` — disallow up front.
		assert!(matches!(
			project_name_for_id("foo.bar"),
			Err(ProjectNameError::InvalidChar { .. }),
		));
	}

	#[test]
	fn underscores_and_dashes_are_allowed() {
		let name = project_name_for_id("foo_bar-2").unwrap();
		assert_eq!(name.as_str(), "moon-ws-foo_bar-2");
	}

	#[test]
	fn folder_project_name_namespaces_under_workspace() {
		let name = project_name_for_folder("default", "moon-landing").unwrap();
		assert_eq!(name.as_str(), "moon-ws-default-moon-landing");
	}

	#[test]
	fn folder_project_name_slugifies_input() {
		let name = project_name_for_folder("default", "My Stuff").unwrap();
		assert_eq!(name.as_str(), "moon-ws-default-my-stuff");
	}

	#[test]
	fn folder_slug_lowercases_and_collapses_punctuation() {
		assert_eq!(folder_slug("moon-landing"), "moon-landing");
		assert_eq!(folder_slug("moon_landing"), "moon_landing");
		assert_eq!(folder_slug("Moon  Landing!!"), "moon-landing");
		assert_eq!(folder_slug("---weird---"), "weird");
	}

	#[test]
	fn folder_slug_falls_back_for_pathological_input() {
		assert_eq!(folder_slug(""), "folder");
		assert_eq!(folder_slug("***"), "folder");
		// Underscore-prefixed: prepend a leading char so the
		// final compose project name still starts with `[a-z0-9]`.
		assert_eq!(folder_slug("_scratch"), "f_scratch");
	}
}
