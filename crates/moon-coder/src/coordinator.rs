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
pub const COORDINATOR_SYSTEM_PROMPT: &str = r#"You are moon-coder, running as a **coordinator** inside the moon-ide editor. Your job is to decompose a goal into worker tasks, spawn each as an autonomous agent (in its own git worktree by default), and drive them to completion — not to edit files yourself.

You are a **pure coordinator**: you cannot edit files. Your `write_file` / `edit_file` calls are rejected at the dispatch boundary. To change the codebase, you **delegate** — spawn a worker and give it the task. Read-only inspection (`read_file`, `list_dir`, `grep`, `bash` for inspection, `web_fetch` / `web_search`) stays available so you can gather context, review what a worker changed, and answer a worker's question without polluting your context with its transcript.

## Workers

A **worker** is a peer top-level coder session — by default in its own git worktree, on its own branch. It is not a sub-agent: it doesn't block you, it doesn't return one string and die, and it isn't hidden under a tool row. It shows up in the sessions list like any session the user opened, and the user can open it mid-run.

Each worker is **named** at spawn (`spawn_worker`'s `name`), and that name is everywhere you'll see it: its branch `moon/<name>`, its worktree directory, its session title, and its session id (`sess-<name>-…`). Your wake-ups and `list_workers` lead with the name, so you can tell workers apart at a glance — you never have to track an opaque id.

The user can message a worker directly at any time. You get a notice quoting what they said — mid-turn if you're running, otherwise queued for your next turn (it doesn't wake you; the worker already has the instruction). Nothing else changes: you keep the worker and every control tool. Factor the message into what you do next; don't repeat it back to the worker.

The user can also **disconnect** a worker from you with the panel's disconnect button. You get one final notice naming the worker plus a snapshot of its branch (ahead / behind, uncommitted files, drift) so you can re-plan from the handover. From then on the worker is theirs: its updates stop reaching you and your control tools refuse it — re-plan without it, don't wait on it. Its session, branch, and worktree are untouched. The read-only tools (`observe_worker`, `review_worker_changes`, `workspace_scm_status`, `check_worker_base`) still work on it if you need one last look.

You manage workers with (details in each tool's description):

- `spawn_worker(task, name, base_branch?, folder?, worktree?)` — create a worker and seed it with a task. Non-blocking; returns a `worker_id`. `name` names the deliverable (`fix-login-redirect`, never `worker-2`) — it becomes the branch, worktree directory, and session title. `worktree: false` runs it **in place** on the folder's current checkout instead (see Worktrees and branches below).
- `observe_worker(worker_id)` — compact snapshot: state, last message, diff summary. Check on a worker without reading its transcript.
- `list_workers(include_disconnected?)` — the whole fleet at a glance, from the same registry your wake-ups come from. Prefer it over hand-maintained fleet state, which drifts.
- `review_worker_changes(worker_id, files?)` — the actual patch text, when `observe_worker`'s summary shows something worth inspecting.
- `steer_worker(worker_id, text)` — queued steering message, delivered at the worker's next loop iteration.
- `abort_worker(worker_id)` — cancel a worker's in-flight turn.
- `workspace_scm_status(worker_id?)` — read-only git status of a worker's worktree (or the main folder).
- `commit_worker_changes(worker_id, message?)` — `git add -A` + commit on the worker's tree; omit `message` for an AI-suggested one.
- `merge_worker_changes(worker_id, base_branch?)` — merge a worker's branch into a base branch locally. Commit first; run `check_worker_base` first. For repos with a remote, prefer leaving the branch for the user to PR.
- `check_worker_base(worker_id)` — the rebase-before-merge / rebase-before-PR gate (see Worktrees and branches below).
- `discard_worker_worktree(worker_id, force?)` — remove a landed worker's checkout and unbind its folder; the branch is kept. Never remove a worktree with `bash` — the folder would stay bound.
- `retire_worker(worker_id)` — drop a fully-landed, discarded worker from the fleet. Retire as you go so `list_workers` stays a live picture instead of a graveyard.
- `clone_repo(url, path?)` / `init_repo(name)` — clone or create a repo and bind it as a workspace folder; pass the returned path to `spawn_worker`'s `folder` to put a worker in it.
- `respond_to_worker_prompt(worker_id, answers)` — answer a worker's parked `ask_user` prompt (surfaced by `observe_worker` and needs-input wakes).

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

By default each worker runs in its own git worktree on its own branch — the branch is the deliverable, and `spawn_worker`'s `name` is what it's called (`moon/<name>`). That name is the user's handle on the worker: it titles the session row in the panel and labels the branch plus the worktree directory under `.worktrees/`, so pick one that describes the deliverable and don't reuse it across workers doing different work. `base_branch` lets you start a worker from an existing branch (e.g. a colleague's open-PR branch) instead of the default — useful for "continue this PR" tasks.

An **in-place worker** (`worktree: false`) trades that isolation away: it works directly in the folder's checkout, on the current branch, and sees uncommitted changes. There is no branch deliverable and nothing to merge — its commits land where the user already is. Prefer worktrees for parallel independent deliverables; go in-place deliberately, when the shared tree is the point.

**Bases go stale.** A worker's branch is created off `origin/main` at spawn time and drifts further behind as its siblings' PRs merge. A stale branch can produce a diff that *deletes* work it didn't write — merging it silently reverts those merged PRs. So: **before any merge or PR, run `check_worker_base`; after one worker's PR merges, re-check every still-in-flight worker's base** and steer a rebase where it overlaps. Never land a branch it flags as reverting.

## Reading rules

- `read_file` returns each line prefixed with `<line_number>|<line>`. The prefix is metadata, not part of the file — strip it before quoting content.
- For large files, pass `start_line` / `end_line` to read just the slice you need.
- `grep` results give you exact line numbers; a typical workflow is `grep` → `read_file` with a range around the match.

## Todo list

`todo_write` is a small in-context plan you maintain as you work. Use it to track the high-level decomposition (which worker addresses which sub-goal) and the rolling state (who's running, who's stuck, who's done). Keep exactly one item `in_progress` at a time. Don't narrate the list back in prose — the UI already renders it.

## Scratchpad

For complex decompositions — many workers, cross-worker dependencies, a multi-phase plan too large for your context — you can write scratch files to `/tmp` (e.g. `/tmp/moon-coord-plan.md`). Reach for this only when you'd otherwise lose track; note `list_workers` already covers fleet state. Don't leave scratch files in the workspace.

Be concise. Do not narrate what each tool call is for; the UI already shows the call to the user.
"#;

/// `spawn_worker` — create a peer top-level session in a worktree and
/// seed it with a task. Returns a handle immediately; the worker runs
/// detached. Lives outside `ToolRegistry::definitions()` (like
/// `task_tool_definition`) so non-coordinator sessions never see it.
pub fn spawn_worker_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"spawn_worker",
		"Spawn a worker — a peer top-level coder session — and seed it with a task prompt. Returns a `worker_id` handle immediately; the worker runs in the background and does not block. The worker is an ordinary agent session (full toolkit, can edit files). It shows up in the sessions list and the user can take over. Use this to delegate a self-contained piece of work to an autonomous agent.\n\nBy default the worker gets its own git worktree on a fresh `moon/<name>` branch off the coordinator's own project — isolated deliverable, mergeable later. Pass `worktree: false` to run the worker **in place** instead: directly in the target folder's existing checkout, on whatever branch is checked out, uncommitted changes included. Use in-place for quick fixes on the current branch, work that must build on uncommitted state, or a follow-up inside an existing worker's worktree (pass that worktree as `folder`). In-place workers share the tree with anything else working there — edits and commits are not isolated, and `merge_worker_changes` does not apply (commits already land on the checked-out branch). Only spawn multiple in-place workers on one folder when their files can't collide.\n\nGive the worker a short `name`: it becomes its session title (and, for worktree workers, the branch `moon/<name>` and worktree directory), so the user can tell your workers apart at a glance.",
		json!({
			"type": "object",
			"properties": {
				"task": {
					"type": "string",
					"description": "Self-contained description of what the worker should do. Include any context the worker needs — it does not see your conversation history. The worker is an autonomous agent with the full toolkit; describe the goal, not the steps."
				},
				"name": {
					"type": "string",
					"description": "Short name for the work, 2-4 words, kebab-case (e.g. `fix-login-redirect`, `port-s3-client`). Becomes the worker's branch `moon/<name>`, its worktree directory, and its session title in the sessions list — name the deliverable, not the worker (`add-retry-backoff`, not `worker-3`). Slugged to `[a-z0-9-]` and suffixed `-2`, `-3`, … if that branch already exists. Ignored when `base_branch` is given (the worker continues a branch that already has a name)."
				},
				"base_branch": {
					"type": "string",
					"description": "Optional existing branch to base the worker on instead of the default branch. A local branch, or a remote one DWIM-created locally the way `git switch` does. Useful for 'continue this PR' or 'work on top of colleague's branch' tasks. Omit for a fresh branch off the default."
				},
				"folder": {
					"type": "string",
					"description": "Optional. The absolute host path of a bound workspace folder to spawn the worker under. Defaults to the coordinator's own folder. Use this to spawn a worker in a project you created with `init_repo` or `clone_repo` — pass the `path` that tool returned. With `worktree: false` this may also be an existing worker's worktree, putting the new worker inside that same checkout. The folder must already be bound in the workspace."
				},
				"worktree": {
					"type": "boolean",
					"description": "Optional, default true. `true` (default): the worker gets its own git worktree on a fresh branch — isolated, mergeable. `false`: the worker runs in place, directly in the target folder's checkout on the currently checked-out branch (uncommitted state included). In-place workers share the working tree with the user and any sibling in-place worker — no isolation. Incompatible with `base_branch`."
				}
			},
			"required": ["task", "name"]
		}),
	)
}

/// `list_workers` — the coordinator's fleet inventory: one snapshot per
/// spawned worker (title, branch, running / idle / needs-input, attached
/// state, how far behind the default branch its base is). Reads the same
/// orchestrator → worker registry the dispatch feeder uses, so it's the
/// ground truth the coordinator would otherwise hand-maintain in
/// `todo_write` / a scratchpad (and could let drift). Use it to stay on
/// top of the fleet and to drive the re-triage-every-worker loop after a
/// merge.
pub fn list_workers_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"list_workers",
		"List every worker you've spawned — your fleet's ground truth, from the same registry your dispatch wake-ups come from. Returns one snapshot per worker: `worker_id`, title, branch, running / idle / needs input, attached state, and drift behind the default branch. Use it to see the whole fleet at a glance instead of polling `observe_worker` per id.",
		json!({
			"type": "object",
			"properties": {
				"include_disconnected": {
					"type": "boolean",
					"description": "Also list workers the user disconnected (still registered but no longer driven by you). Default false — just your live fleet."
				}
			}
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

/// `init_repo` — initialize a new git repository as a sibling of the
/// coordinator's project folder and add it as a workspace folder.
/// Takes a directory `name`, not a path — new projects belong next
/// to the projects the user already works in, not at arbitrary
/// filesystem locations (a model given a free path picks `/tmp`).
/// `add_folder` — bind an existing sibling directory as a workspace
/// folder. The already-on-disk counterpart of `clone_repo` /
/// `init_repo`: same sibling-only posture (the model names a
/// directory next to its project; it never picks an arbitrary host
/// path).
pub fn add_folder_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"add_folder",
		"Bind an existing repository/directory that is a **sibling of your project folder** (same parent directory) as a workspace folder. The already-on-disk counterpart of `clone_repo` / `init_repo` — use it when the repo is already checked out locally (e.g. cloned in an earlier session) instead of re-cloning. Pass `name` = the sibling directory's name. Returns the folder's path and name; pass that path to `spawn_worker`'s `folder` to put a worker in it. Idempotent: binding an already-bound folder just returns it.",
		json!({
			"type": "object",
			"properties": {
				"name": {
					"type": "string",
					"description": "The sibling directory's name (a single path component, e.g. `gaia-project`)."
				}
			},
			"required": ["name"]
		}),
	)
}

pub fn init_repo_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"init_repo",
		"Initialize a new git repository and add it as a workspace folder. The repo is created as a **sibling of your project folder** (same parent directory) using `name` as the directory name — you don't choose the location. Runs `git init` on the host. Use this when a task needs a fresh project — a scratch repo, a new microservice, a test harness. Returns the new folder's path and name; pass that path to `spawn_worker`'s `folder` to put a worker in it.",
		json!({
			"type": "object",
			"properties": {
				"name": {
					"type": "string",
					"description": "Directory name for the new repo (e.g. `my-service`). Letters, digits, `-`, `_`, `.` only — no slashes. The repo is created next to your project folder."
				}
			},
			"required": ["name"]
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

/// `discard_worker_worktree` — remove a finished worker's checkout
/// and unbind its folder (ADR 0044). The branch is kept: it is the
/// deliverable. This is the coordinator's cleanup tool; without it a
/// long run leaves one bound folder per worker in the user's project
/// bar, and a `git worktree remove` through `bash` would leave the
/// folder bound (the workspace registry never learns about it).
pub fn discard_worker_worktree_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"discard_worker_worktree",
		"Remove a finished worker's git worktree and unbind its folder from the workspace. **The branch is kept** — it's the deliverable. Use this once the work is landed (merged, or pushed for a PR). Refuses while the worker's turn is running, and refuses uncommitted changes unless `force` is set — commit or merge first rather than forcing. The worker's session stays in the list and falls back to the parent project. Set `retire: true` to also retire the worker in the same call (the usual cleanup pair); pass `worker_ids` to clean up several at once.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`. Provide this or `worker_ids`."
				},
				"worker_ids": {
					"type": "array",
					"items": { "type": "string" },
					"description": "Several worker ids to discard in one call. Each reports its own result; one failure doesn't stop the rest."
				},
				"force": {
					"type": "boolean",
					"description": "Discard even when the worktree has uncommitted or untracked changes (`git worktree remove --force`). Those changes are lost. Default false — prefer `commit_worker_changes` first."
				},
				"retire": {
					"type": "boolean",
					"description": "Also retire the worker(s) from your fleet after a successful discard — same effect as a follow-up `retire_worker`, without the second call. Default false."
				}
			}
		}),
	)
}

/// `retire_worker` — drop a finished worker from the coordinator's
/// fleet registry (ADR 0064). The session, its transcript, and its
/// branch are untouched; the worker just stops appearing in
/// `list_workers` and stops waking the coordinator. Without this a
/// long-running coordinator's fleet grows monotonically — every
/// landed worker lingers as an idle row forever.
pub fn retire_worker_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"retire_worker",
		"Retire a finished worker from your fleet: it stops appearing in `list_workers`, stops waking you, and the control tools no longer treat it as yours. Its session, transcript, and branch are untouched — bookkeeping, not deletion. Use it once the work is fully landed and the worktree discarded. Refuses a worker whose turn is running or whose worktree is still bound (use `discard_worker_worktree`'s `retire: true` to do both in one call). Pass `worker_ids` to retire several at once.",
		json!({
			"type": "object",
			"properties": {
				"worker_id": {
					"type": "string",
					"description": "The worker id returned by `spawn_worker`. Provide this or `worker_ids`."
				},
				"worker_ids": {
					"type": "array",
					"items": { "type": "string" },
					"description": "Several worker ids to retire in one call. Each reports its own result; one failure doesn't stop the rest."
				}
			}
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

/// `check_worker_base` — the rebase-before-merge / rebase-before-PR
/// gate. Reports how far a worker's branch has drifted behind the
/// repo's default branch and flags the stale-base revert tripwire: a
/// file with deletions the worker didn't write. Use this before
/// `merge_worker_changes`, before opening / updating a PR, and after a
/// wave-1 PR merges underneath an in-flight worker.
pub fn check_worker_base_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"check_worker_base",
		"Check whether a worker's branch has drifted stale against the repo's default branch — the rebase-before-merge / rebase-before-PR gate. Fetches, then reports how many commits behind `origin/main` the branch is and whether its diff **reverts work it didn't write** (a file with deletions the worker never touched). Run it before `merge_worker_changes` or opening / updating a PR, and on every in-flight worker after a sibling merges. If it flags a revert, don't merge / PR — steer the worker to rebase onto current `origin/main` first. Read-only.",
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
					"description": "Map of question id → answer. Each answer is an option id, a free-form string for a custom answer, or an array of those for a multi-select question. (The raw `ask_user`-response array form — `[{question_id, selected, free_text}]` — is also accepted.)",
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
		assert_eq!(list_workers_tool_definition().function.name, "list_workers");
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
		assert_eq!(check_worker_base_tool_definition().function.name, "check_worker_base");
		assert_eq!(retire_worker_tool_definition().function.name, "retire_worker");
		assert_eq!(
			discard_worker_worktree_tool_definition().function.name,
			"discard_worker_worktree"
		);
		assert_eq!(clone_repo_tool_definition().function.name, "clone_repo");
		assert_eq!(init_repo_tool_definition().function.name, "init_repo");
		assert_eq!(add_folder_tool_definition().function.name, "add_folder");
	}

	#[test]
	fn spawn_worker_requires_task_and_name() {
		let params = spawn_worker_tool_definition().function.parameters;
		assert_eq!(params["type"], "object");
		assert!(params["properties"]["task"].is_object());
		// `name` is required too (ADR 0042) — the branch / worktree /
		// session chip are named after it.
		assert!(params["properties"]["name"].is_object());
		let required: Vec<String> = serde_json::from_value(params["required"].clone()).unwrap();
		assert!(required.contains(&"task".to_string()));
		assert!(required.contains(&"name".to_string()));
		// `base_branch` is optional.
		assert!(params["properties"]["base_branch"].is_object());
		// `folder` is optional.
		assert!(params["properties"]["folder"].is_object());
		assert!(!required.contains(&"base_branch".to_string()));
		assert!(!required.contains(&"folder".to_string()));
	}

	#[test]
	fn discard_worker_worktree_takes_single_or_batch_ids_and_defaults_to_no_force() {
		let params = discard_worker_worktree_tool_definition().function.parameters;
		// `worker_id` or `worker_ids` — either is acceptable, so
		// neither is schema-required (the handler validates).
		assert!(params["properties"]["worker_id"].is_object());
		assert!(params["properties"]["worker_ids"].is_object());
		assert!(params["required"].is_null());
		assert_eq!(params["properties"]["force"]["type"], "boolean");
		assert_eq!(params["properties"]["retire"]["type"], "boolean");
	}

	#[test]
	fn retire_worker_takes_single_or_batch_ids() {
		let params = retire_worker_tool_definition().function.parameters;
		assert!(params["properties"]["worker_id"].is_object());
		assert!(params["properties"]["worker_ids"].is_object());
		assert!(params["required"].is_null());
	}

	#[test]
	fn init_repo_takes_a_name_not_a_path() {
		let params = init_repo_tool_definition().function.parameters;
		assert!(params["properties"]["name"].is_object());
		assert!(params["properties"]["path"].is_null());
		assert_eq!(params["required"][0], "name");
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
		// A user message into a worker is a notice, not a handover
		// (ADR 0043) — the prompt must not claim tools will refuse it.
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("message a worker directly"));
		assert!(!COORDINATOR_SYSTEM_PROMPT.contains("taken over"));
		// The disconnect gesture (ADR 0052) is the explicit handover —
		// the prompt names it and the refusal the tools will enforce.
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("disconnect"));
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("refuse"));
		// The stale-base gate: the prompt names the tool and the
		// rebase-before-merge / re-triage discipline.
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("check_worker_base"));
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("rebase"));
		// Fleet awareness: the prompt points at `list_workers` as the
		// ground-truth inventory and tells the coordinator workers are named.
		assert!(COORDINATOR_SYSTEM_PROMPT.contains("list_workers"));
	}
}
