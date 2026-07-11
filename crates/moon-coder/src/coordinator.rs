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

A **worker** is a peer top-level coder session in its own git worktree, on its own branch. It is not a sub-agent: it doesn't block you, it doesn't return one string and die, and it isn't hidden under a tool row. It shows up in the sessions list like any session the user opened, and the user can open it mid-run and take over (steer it, abort it, answer its questions) through the normal composer. You and the user share the same control surface over a worker.

You manage workers with:

- `spawn_worker(task, base_branch?)` — create a worker in a fresh worktree on a new `moon/agent-<id>` branch (or based on an existing branch when `base_branch` is given), seed it with a task prompt, and let it run. Returns a `worker_id` handle immediately — **it does not block**. The worker keeps running in the background.
- `observe_worker(worker_id)` — fetch a compact snapshot of a worker's current state: its task, branch, turns-so-far, last assistant message, and whether it's running / idle / needs input (a parked `ask_user`). Use this to check on a worker without reading its full transcript.
- `steer_worker(worker_id, text)` — send a steering message to a worker mid-turn, the same way a user steers you. Queued; delivered at the worker's next loop iteration top.
- `abort_worker(worker_id)` — cancel a worker's in-flight turn.
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

Each worker runs in its own git worktree on its own branch — the branch is the deliverable. When the worker is done, its work is on that branch, ready for the user to review, commit, and PR through the normal SCM flow. You do not merge work back; you do not delete branches. `base_branch` lets you start a worker from an existing branch (e.g. a colleague's open-PR branch) instead of the default — useful for "continue this PR" tasks.

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
		"Spawn a worker — a peer top-level coder session in its own git worktree on a fresh branch — and seed it with a task prompt. Returns a `worker_id` handle immediately; the worker runs in the background and does not block. The worker is an ordinary agent session (full toolkit, can edit files) in a worktree. It shows up in the sessions list and the user can take over. Use this to delegate a self-contained piece of work to an autonomous agent that produces its own branch / PR.",
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
	}

	#[test]
	fn spawn_worker_requires_task() {
		let params = spawn_worker_tool_definition().function.parameters;
		assert_eq!(params["type"], "object");
		assert!(params["properties"]["task"].is_object());
		assert_eq!(params["required"][0], "task");
		// `base_branch` is optional.
		assert!(params["properties"]["base_branch"].is_object());
		let required: Vec<String> = serde_json::from_value(params["required"].clone()).unwrap();
		assert!(!required.contains(&"base_branch".to_string()));
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
	}
}
