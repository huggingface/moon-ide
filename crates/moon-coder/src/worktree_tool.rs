//! `enter_worktree` — an ordinary agent session moves itself into a
//! git worktree (ADR 0096).
//!
//! The tool definition and argument parsing live here; the handler
//! sits in the runner because it re-points the turn's live
//! `ToolContext` at the worktree, so later tool calls in the same
//! turn already run there.

use serde_json::{json, Value};

use crate::error::CoderError;
use crate::inference::ToolDefinition;

pub const ENTER_WORKTREE: &str = "enter_worktree";

/// Advertised only to top-level `Agent` sessions: a coordinator
/// already isolates its workers with `spawn_worker`, and a sub-agent
/// gets its worktree from the parent's `task` call (`worktree` arg).
pub fn enter_worktree_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		ENTER_WORKTREE,
		"Move this session into its own git worktree of the project, so your work lands on a separate branch without touching the project's main checkout (or other agents working in it). Use this instead of running `git worktree add` / `git checkout -b` yourself. From the next tool call on, relative paths, `grep`, `bash` and git all operate inside the worktree; the conversation continues as is. Pass `name` to start a fresh branch `moon/<name>` off the default branch (`origin/main`) or `base`, or `branch` to work on an existing local or remote branch (e.g. an open PR) — if that branch already has an IDE-managed worktree, the session attaches to it. A fresh branch has no upstream: push it with `git push -u origin HEAD`, never to `main`.",
		worktree_target_schema(),
	)
}

/// `{ name, base } | { branch }` — shared by `enter_worktree` and the
/// `task` tool's `worktree` argument.
pub fn worktree_target_schema() -> Value {
	json!({
		"type": "object",
		"properties": {
			"name": {
				"type": "string",
				"description": "Short description of the work (e.g. `fix login redirect`); becomes the new branch `moon/<slug>`. Mutually exclusive with `branch`."
			},
			"branch": {
				"type": "string",
				"description": "Existing branch to check out in the worktree, as a bare name (`fix-login`, not `origin/fix-login`); a remote-only branch gets a local tracking branch. Mutually exclusive with `name`."
			},
			"base": {
				"type": "string",
				"description": "Start point for a new `name` branch (a branch, tag or commit). Defaults to the repository's default branch on origin."
			}
		}
	})
}

/// What the agent asked for, validated.
#[derive(Debug, PartialEq, Eq)]
pub enum EnterWorktreeTarget {
	/// Fresh branch named after `name`, off `base` (`None` = the
	/// default branch on origin).
	New { name: String, base: Option<String> },
	/// Existing branch, bare name.
	Existing(String),
}

pub fn parse_enter_worktree_args(args: &Value) -> Result<EnterWorktreeTarget, CoderError> {
	parse_worktree_target(ENTER_WORKTREE, args)
}

/// Validate a [`worktree_target_schema`] object; errors are attributed
/// to `tool` so the model knows which call to fix.
pub fn parse_worktree_target(tool: &str, args: &Value) -> Result<EnterWorktreeTarget, CoderError> {
	#[derive(serde::Deserialize)]
	struct Args {
		#[serde(default)]
		name: Option<String>,
		#[serde(default)]
		branch: Option<String>,
		#[serde(default)]
		base: Option<String>,
	}
	let parsed: Args =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args(tool, err.to_string()))?;
	let non_empty = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
	let name = non_empty(parsed.name);
	let branch = non_empty(parsed.branch);
	let base = non_empty(parsed.base);
	match (name, branch) {
		(Some(_), Some(_)) => Err(CoderError::invalid_args(
			tool,
			"pass either `name` (new branch) or `branch` (existing branch), not both",
		)),
		(None, None) => Err(CoderError::invalid_args(
			tool,
			"pass `name` to start a new branch or `branch` to work on an existing one",
		)),
		(Some(name), None) => Ok(EnterWorktreeTarget::New { name, base }),
		(None, Some(branch)) => {
			if base.is_some() {
				return Err(CoderError::invalid_args(
					tool,
					"`base` only applies to a new `name` branch; an existing `branch` is checked out as is",
				));
			}
			// `git worktree add <path> origin/x` checks out a detached
			// HEAD instead of DWIM-creating local `x`; agents reach for
			// the remote-qualified name often enough to forgive it.
			let bare = branch.strip_prefix("origin/").unwrap_or(&branch).to_string();
			Ok(EnterWorktreeTarget::Existing(bare))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn new_branch_with_optional_base() {
		assert_eq!(
			parse_enter_worktree_args(&json!({ "name": "fix login" })).unwrap(),
			EnterWorktreeTarget::New {
				name: "fix login".into(),
				base: None
			}
		);
		assert_eq!(
			parse_enter_worktree_args(&json!({ "name": "x", "base": " develop " })).unwrap(),
			EnterWorktreeTarget::New {
				name: "x".into(),
				base: Some("develop".into())
			}
		);
	}

	#[test]
	fn existing_branch_strips_origin_prefix() {
		assert_eq!(
			parse_enter_worktree_args(&json!({ "branch": "origin/for-you-feed" })).unwrap(),
			EnterWorktreeTarget::Existing("for-you-feed".into())
		);
	}

	#[test]
	fn rejects_ambiguous_or_empty_args() {
		assert!(parse_enter_worktree_args(&json!({})).is_err());
		assert!(parse_enter_worktree_args(&json!({ "name": "  " })).is_err());
		assert!(parse_enter_worktree_args(&json!({ "name": "a", "branch": "b" })).is_err());
		assert!(parse_enter_worktree_args(&json!({ "branch": "b", "base": "main" })).is_err());
	}
}
