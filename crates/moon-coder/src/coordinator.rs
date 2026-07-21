//! Orchestrator (coordinator) sessions — ADR 0030.
//!
//! A coordinator is a top-level coder session in a read-only mode whose
//! job is to *delegate* work to peer top-level sessions ("workers") in
//! git worktrees, observe their progress, steer them, and report back
//! to the user. Unlike a sub-agent (`task`), a worker:
//!
//! - is a real top-level session (lives in the per-project session list,
//!   has its own composer / abort / steer, the user can open it mid-run
//!   and take over),
//! - is created in its own git worktree (independent branch = the
//!   deliverable),
//! - runs detached (the orchestrator does not block on it), and
//! - is driven through the same `coder_*` client surface the companion
//!   app uses — `spawn_worker`, `observe_worker`, `steer_worker`,
//!   `abort_worker`, `respond_to_worker_prompt`.
//!
//! The orchestrator is an *idle session fed by a dispatch queue*: it
//! sits idle between turns and wakes when a worker raises an event
//! (`ask_user`, turn complete, stuck) or the user sends a message.
//! Each wake delivers a self-contained dispatch packet about one
//! worker so the orchestrator's context stays plan-shaped, not
//! transcript-shaped.
//!
//! What's in this file: the coordinator system prompt and the tool
//! definitions advertised to the model. The dispatch (the `handle_*`
//! functions that actually create / observe / steer worker sessions)
//! lives in `runner.rs` alongside `handle_task` / `handle_ask_user`,
//! because — like those — it needs `&CoderState` to mint peer sessions.

use crate::inference::ToolDefinition;
use serde_json::json;

/// The coordinator system prompt. Swapped in for the base
/// [`PHASE_6_0_SYSTEM_PROMPT`](crate::defaults::PHASE_6_0_SYSTEM_PROMPT)
/// when the session's mode is `Coordinator`.
///
/// The prompt establishes the coordinator's identity (you delegate,
/// you don't edit), the worker model (peers in worktrees, not
/// sub-agents), the dispatch-packet discipline (self-contained, per-
/// worker, don't hold worker transcripts in your context), and the
/// "passive until needed" loop shape (you sit idle and wake on worker
/// events or user messages).
pub const COORDINATOR_SYSTEM_PROMPT: &str = r#"You are moon-coder, running as a **coordinator** inside the moon-ide editor. Your job is to decompose a goal into worker tasks, spawn each as an autonomous agent in its own git worktree, and drive them to completion — not to edit files yourself.

You are a **pure coordinator**: you cannot edit files. Your `write_file` / `edit_file` calls are rejected at the dispatch boundary. To change the codebase, you **delegate** — spawn a worker and give it the task. Read-only inspection (`read_file`, `list_dir`, `grep`, `bash` for inspection, `web_fetch` / `web_search`) stays available so you can gather context, review what a worker changed, and answer a worker's question without polluting your context with its transcript.

## Workers

A **worker** is a peer top-level coder session in its own git worktree, on its own branch. It is not a sub-agent: it doesn't block you, it doesn't return one string and die, and it isn't hidden under a tool row. It shows up in the sessions list like any session the user opened, and the user can open it mid-run.

The moment the user messages a worker directly, that worker is **taken over**: you get one final notice, its updates stop reaching you, and your control tools (`steer_worker`, `abort_worker`, `respond_to_worker_prompt`, `commit_worker_changes`) will refuse it. Treat its task as user-owned from then on — don't fight for control; re-plan around it (note it in your plan, report status, spawn a different worker only if the remaining work genuinely still needs one). Read-only tools (`observe_worker`, `review_worker_changes`, `workspace_scm_status`) keep working so you can still describe its state when reporting.

You manage workers with:

- `spawn_worker(task, base_branch?, folder?)` — create a worker in a fresh worktree on a new `moon/agent-<id>` branch (or based on an existing branch when `base_branch` is given), seed it with a task prompt, and let it run. Returns a `worker_id` handle immediately — **it does not block**. The worker keeps running in the background. By default the worktree is created off the coordinator's own project; pass `folder` to target a different bound workspace folder (e.g. one you just created with `init_repo` or `clone_repo` — pass the `path` that tool returned). The folder must already be bound in the workspace.
- `observe_worker(worker_id)` — fetch a compact snapshot of a worker's current state: its task, branch, turns-so-far, last assistant message, whether it's running / idle / needs input (a parked `ask_user`), and a **diff summary** (files changed + added/removed counts per file, not the full patch). Use this to check on a worker without reading its full transcript or flooding your context with patch text.
- `review_worker_changes(worker_id, files?)` — pull the full per-turn diff for a worker, optionally scoped to specific files. Use this when `observe_worker`'s diff summary shows files you want to actually inspect. Don't call it on every observe — only when you need the detail.
- `steer_worker(worker_id, text)` — send a steering message to a worker mid-turn, the same way a user steers you. Queued; delivered at the worker's next loop iteration top.
- `abort_worker(worker_id)` — cancel a worker's in-flight turn.
- `workspace_scm_status(worker_id?)` — get the SCM (git) status of a worker's worktree or the main folder: branch name, ahead/behind upstream, files changed (added / modified / deleted counts + per-file list). Read-only — use this to check whether a worker should commit before you steer it to the next task.
- `commit_worker_changes(worker_id, message?)` — commit a worker's uncommitted changes (`git add -A` + `git commit`, same as the IDE's SCM panel). Pass `message` to set the commit subject; omit it to get an AI-suggested message from the diff. Use `workspace_scm_status` first to check whether there's anything to commit.
- `merge_worker_changes(worker_id, base_branch?)` — merge a worker's branch into a base branch on the parent repo (defaults to `main`). Switches the parent to `base_branch`, then `git merge --no-edit <worker_branch>`. The worker's worktree and branch are left intact. Use `commit_worker_changes` first if the worker has uncommitted work. Use this for local repos without a PR flow; for repos with a remote, leave the branch for the user to PR instead.
- `clone_repo(url, path?)` — clone a git repository to a host path and add it as a workspace folder. Use this when a task requires a dependency repo or a fresh checkout that isn't already in the workspace. The clone runs on the host so the path is immediately available. Omit `path` to clone into a sibling of the active folder.
- `init_repo(path)` — initialize a new git repository at a host path and add it as a workspace folder. Use this when a task needs a fresh project (scratch repo, new microservice, test harness). Creates the directory if it doesn't exist.
- `respond_to_worker_prompt(worker_id, answers)` — answer a worker's parked `ask_user` prompt. A worker that needs a decision from you raises `ask_user`; you see it via `observe_worker` and answer it with this tool.

## Your loop: passive until needed

You sit **idle** between turns. You wake when:

1. **The user sends you a message** (a question, a new goal, a follow-up).
2. **A worker pokes you** — it raised an `ask_user`, finished its turn, opened a PR, or got stuck. These arrive as self-contained **dispatch packets**: one packet per worker event, batched into a single wake when several land at once.

Each wake runs **one turn**: you read the dispatch packet(s) / user message, decide what to do (spawn a new worker, steer an existing one, answer a question, report to the user), act, and go idle again. You do not poll workers in a tight loop — `observe_worker` is for when you need a snapshot, not for spinning.

## Context discipline

Your context holds **your plan** (the goal, the strategy, which worker addresses which sub-goal) and a **rolling dispatch log** (recent wakes, so you remember "I already told worker #3 to skip the e2e tests"). It does **not** hold the workers' transcripts. Each dispatch packet is self-contained — the worker's task, branch, turns-so-far, and the specific question or state change. Answer from the packet + your plan, not from a mental model of all workers' internals. This is what lets you juggle several workers without context-switch failure.

## The depth cap is intentional

You spawn **workers** (peer top-level sessions), not sub-agents. A worker *can* itself be a coordinator that spawns further workers — that's the scale escape valve, and it's allowed. But for the common case (a handful of workers, intermittent attention each), one coordinator is sufficient. Don't spawn a sub-coordinator unless you're genuinely juggling more workers than you can hold in your plan.

## Worktrees and branches

Each worker runs in its own git worktree on its own branch — the branch is the deliverable. `base_branch` lets you start a worker from an existing branch (e.g. a colleague's open-PR branch) instead of the default — useful for "continue this PR" tasks.

**When to merge vs leave for PR:** For repos with a remote, leave the worker's branch for the user to review and PR — the branch *is* the deliverable, and you should not merge it. For local repos without a remote (e.g. a scratch repo you created with `init_repo`), use `merge_worker_changes` to land the worker's committed work onto the base branch. Use `commit_worker_changes` first if the worker has uncommitted work, then `merge_worker_changes` to merge. You do not delete branches.

## Reading rules

- `read_file` returns each line prefixed with `<line_number>|<line>`. The prefix is metadata, not part of the file — strip it before quoting content.
- For large files, pass `start_line` / `end_line` to read just the slice you need.
- `grep` results give you exact line numbers; a typical workflow is `grep` → `read_file` with a range around the match.

## Todo list

`todo_write` is a small in-context plan you maintain as you work. Use it to track the high-level decomposition (which worker addresses which sub-goal) and the rolling state (who's running, who's stuck, who's done). Keep exactly one item `in_progress` at a time. Don't narrate the list back in prose — the UI already renders it.

Be concise. Do not narrate what each tool call is for; the UI already shows the call to the user.
"#;

/// `spawn_worker` — create a peer top-level session in a worktree and
/// seed it with a task. Returns a handle immediately; the worker runs
/// detached. Lives outside `ToolRegistry::definitions()` (like
/// `task_tool_definition`) so non-coordinator sessions never see it.
pub fn spawn_worker_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"spawn_worker",
		"Spawn a worker — a peer top-level coder session in its own git worktree on a fresh branch — and seed it with a task prompt. Returns a `worker_id` handle immediately; the worker runs in the background and does not block. The worker is an ordinary agent session (full toolkit, can edit files) in a worktree. It shows up in the sessions list and the user can take over. Use this to delegate a self-contained piece of work to an autonomous agent that produces its own branch / PR.\n\nBy default the worktree is created off the coordinator's own project. Pass `folder` to target a different bound workspace folder — e.g. one you just created with `init_repo` or `clone_repo`. The folder must already be bound in the workspace.",
		json!({
			"type": "object",
			"properties": {
				"task": {
					"type": "string",
					"description": "Self-contained description of what the worker should do. Include any context the worker needs — it does not see your conversation history. The worker is an autonomous agent with the full toolkit; describe the goal, not the steps."
				},
				"base_branch": {
					"type": "string",
					"description": "Optional existing branch to base the worker on instead of the default branch. A local branch, or a remote one DWIM-created locally the way `git switch` does. Useful for 'continue this PR' or 'work on top of colleague's branch' tasks. Omit for a fresh branch off the default."
				},
				"folder": {
					"type": "string",
					"description": "Optional. The absolute host path of a bound workspace folder to create the worktree under. Defaults to the coordinator's own folder. Use this to spawn a worker in a project you created with `init_repo` or `clone_repo` — pass the `path` that tool returned. The folder must already be bound in the workspace."
				}
			},
			"required": ["task"]
		}),
	)
}

/// `observe_worker` — fetch a compact snapshot of a worker's current
/// state. Use this to check on a worker without reading its full
/// transcript.
pub fn observe_worker_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"observe_worker",
		"Fetch a compact snapshot of a worker's current state: its task, branch, turns-so-far, last assistant message, and whether it's running / idle / needs input (a parked `ask_user`). Use this to check on a worker's progress without reading its full transcript. Do not poll in a tight loop — call this when you need a snapshot to decide what to do next.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				}
			},
			"required": ["worker_id"]
		}),
	)
}

/// `steer_worker` — send a steering message to a worker mid-turn.
/// Queued; delivered at the worker's next loop iteration top, the same
/// way a user steers a session.
pub fn steer_worker_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"steer_worker",
		"Send a steering message to a worker mid-turn. The message is queued and delivered at the worker's next loop iteration — the same way a user steers a coder session. Use this to redirect a worker, answer a question it asked in its transcript (not via `ask_user`), or nudge it when it's going down the wrong path. Do not use it to poll; use `observe_worker` for that.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				},
				"text": {
					"type": "string",
					"description": "The steering message. Self-contained — the worker doesn't see your conversation history."
				}
			},
			"required": ["worker_id", "text"]
		}),
	)
}

/// `abort_worker` — cancel a worker's in-flight turn.
pub fn abort_worker_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"abort_worker",
		"Cancel a worker's in-flight turn. The worker keeps its partial assistant message and completed tool calls, same as an Esc-abort on a normal session. Use this when a worker is clearly off track and you want to stop it before steering it onto a better path.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				}
			},
			"required": ["worker_id"]
		}),
	)
}

/// `clone_repo` — clone a git repository to a host path and add it
/// as a workspace folder. The clone runs on the host (not the
/// container) so the path is immediately bind-mountable.
pub fn clone_repo_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"clone_repo",
		"Clone a git repository and add it as a workspace folder. The clone runs on the host filesystem so the resulting directory is immediately available to the IDE and container. Pass `path` to control where the repo lands (an absolute host path); omit it to clone into a sibling directory of the active folder. Returns the new folder's path and name. Use this when a task requires a dependency repo, a fresh checkout, or a reference codebase that isn't already in the workspace.",
		json!({
			"type": "object",
			"properties": {
				"url": {
					"type": "string",
					"description": "The git URL to clone (HTTPS or SSH)."
				},
				"path": {
					"type": "string",
					"description": "Optional absolute host path to clone into. If omitted, clones into a sibling of the active folder using the repo's basename."
				}
			},
			"required": ["url"]
		}),
	)
}

/// `init_repo` — initialize a new git repository at a host path and
/// add it as a workspace folder. Creates the directory if it doesn't
/// exist.
pub fn init_repo_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"init_repo",
		"Initialize a new git repository at a host path and add it as a workspace folder. Runs `git init` on the host, creating the directory if it doesn't exist. Use this when a task needs a fresh project — a scratch repo, a new microservice, a test harness. Returns the new folder's path and name.",
		json!({
			"type": "object",
			"properties": {
				"path": {
					"type": "string",
					"description": "Absolute host path for the new repo. The directory will be created if it doesn't exist."
				}
			},
			"required": ["path"]
		}),
	)
}

/// `commit_worker_changes` — checkpoint a worker's uncommitted
/// work with a git commit. Runs `git add -A` + `git commit` on the
/// worker's worktree (the same flow the IDE's SCM panel uses). If
/// `message` is omitted, an AI-suggested commit message is generated
/// from the diff. Returns the commit's short SHA + summary.
pub fn commit_worker_changes_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"commit_worker_changes",
		"Commit a worker's uncommitted changes — runs `git add -A` + `git commit` on the worker's worktree, the same flow the IDE's SCM panel uses. Pass `message` to set the commit subject; omit it to get an AI-suggested message from the diff. Use `workspace_scm_status` first to check whether there's anything to commit. Returns the commit's short SHA and summary, or an error if there's nothing to commit.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				},
				"message": {
					"type": "string",
					"description": "Optional commit message (subject line). If omitted, an AI-suggested message is generated from the diff."
				}
			},
			"required": ["worker_id"]
		}),
	)
}

/// `merge_worker_changes` — merge a worker's branch into a base
/// branch on the parent repo. The parent repo is switched to
/// `base_branch` (default: the repo's default branch), then `git
/// merge --no-edit <worker_branch>` runs on the parent's host. The
/// worker's worktree and branch are left intact — call this after
/// `commit_worker_changes` to land committed work onto the base
/// branch. Use this for local repos without a PR flow; for repos with
/// a remote, leave the branch for the user to PR instead.
pub fn merge_worker_changes_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"merge_worker_changes",
		"Merge a worker's branch into a base branch on the parent repo. Switches the parent repo to `base_branch` (defaults to `main`), then runs `git merge --no-edit <worker_branch>`. The worker's worktree and branch are left intact. Use `commit_worker_changes` first if the worker has uncommitted work. Use this for local repos without a PR flow — for repos with a remote, leave the branch for the user to PR instead. Returns the merge result or an error (conflicts, dirty tree, unknown ref).",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				},
				"base_branch": {
					"type": "string",
					"description": "Optional. The branch to merge into. Defaults to `main`. Use this when the repo's default branch has a different name."
				}
			},
			"required": ["worker_id"]
		}),
	)
}

/// `workspace_scm_status` — read-only SCM state for a worker's
/// worktree (or the main folder). Composes branch info, file change
/// counts, and the file list into one compact snapshot so the
/// coordinator can decide whether a worker's work should be committed
/// before moving on.
pub fn workspace_scm_status_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"workspace_scm_status",
		"Get the SCM (git) status of a worker's worktree — branch, ahead/behind upstream, files changed (added / modified / deleted counts + per-file list). Pass `worker_id` to check a specific worker's worktree; omit it to check the main workspace folder. Read-only — use this to decide whether a worker should commit before you steer it to the next task.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "Optional. The worker id returned by `spawn_worker`. If omitted, checks the main workspace folder."
				}
			}
		}),
	)
}

/// `review_worker_changes` — pull the full per-turn diff for a worker,
/// optionally scoped to specific files. Use this when `observe_worker`'s
/// diff summary shows files you want to actually review. Returns the
/// unified diff text (capped at ~64 KB).
pub fn review_worker_changes_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"review_worker_changes",
		"Pull the full per-turn diff for a worker, optionally scoped to specific files. `observe_worker` gives you a summary (files + added/removed counts); this tool gives you the actual patch text so you can review the changes. Use it when the summary shows something you want to inspect — don't call it on every observe, only when you need the detail. Pass `files` to review specific files; omit it to get the full diff for all changed files.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				},
				"files": {
					"type": "array",
					"items": { "type": "string" },
					"description": "Optional list of file paths to scope the review to. Omit to get the full diff for all changed files."
				}
			},
			"required": ["worker_id"]
		}),
	)
}

/// `respond_to_worker_prompt` — answer a worker's parked `ask_user`
/// prompt. A worker that needs a decision from you raises `ask_user`;
/// you see it via `observe_worker` and answer it with this tool.
pub fn respond_to_worker_prompt_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"respond_to_worker_prompt",
		"Answer a worker's parked `ask_user` prompt. A worker that needs a decision from you raises `ask_user`; you discover it via `observe_worker` (which shows `needs_input: true` and the pending question) and answer it here. The answers key back to the question ids, the same shape as `ask_user`'s response.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`."
				},
				"answers": {
					"type": "object",
					"description": "Map of question id → selected answer id (or a free-form string for a custom answer). Same shape as `ask_user`'s response `answers` field.",
					"additionalProperties": {}
				}
			},
			"required": ["worker_id", "answers"]
		}),
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tool_definitions_have_names() {
		assert_eq!(spawn_worker_tool_definition().function.name, "spawn_worker");
		assert_eq!(observe_worker_tool_definition().function.name, "observe_worker");
		assert_eq!(steer_worker_tool_definition().function.name, "steer_worker");
		assert_eq!(abort_worker_tool_definition().function.name, "abort_worker");
		assert_eq!(
			respond_to_worker_prompt_tool_definition().function.name,
			"respond_to_worker_prompt"
		);
		assert_eq!(
			review_worker_changes_tool_definition().function.name,
			"review_worker_changes"
		);
		assert_eq!(
			workspace_scm_status_tool_definition().function.name,
			"workspace_scm_status"
		);
		assert_eq!(
			commit_worker_changes_tool_definition().function.name,
			"commit_worker_changes"
		);
		assert_eq!(
			merge_worker_changes_tool_definition().function.name,
			"merge_worker_changes"
		);
		assert_eq!(clone_repo_tool_definition().function.name, "clone_repo");
		assert_eq!(init_repo_tool_definition().function.name, "init_repo");
	}

	#[test]
	fn spawn_worker_requires_task() {
		let params = spawn_worker_tool_definition().function.parameters;
		assert_eq!(params["type"], "object");
		assert!(params["properties"]["task"].is_object());
		assert_eq!(params["required"][0], "task");
		// `base_branch` is optional.
		assert!(params["properties"]["base_branch"].is_object());
		// `folder` is optional.
		assert!(params["properties"]["folder"].is_object());
		let required: Vec<String> = serde_json::from_value(params["required"].clone()).unwrap();
		assert!(!required.contains(&"base_branch".to_string()));
		assert!(!required.contains(&"folder".to_string()));
	}

	#[test]
	fn coordinator_prompt_mentions_workers_and_delegation() {
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("coordinator"));
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("worker"));
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("delegate"));
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("worktree"));
		// The "passive until needed" loop shape.
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("passive until needed"));
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("dispatch packet"));
		// User-takeover semantics (ADR 0036).
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("taken over"));
	}
}
