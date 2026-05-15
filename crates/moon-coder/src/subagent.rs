//! Sub-agent runner.
//!
//! A sub-agent is a fresh agent loop spawned from inside the
//! parent's tool dispatch. It has its own short-lived `messages`
//! history, its own [`ToolContext`] (folder + mode), and its own
//! cancellation token derived from the parent's so an abort
//! cascades. Sub-agents return a single text result that the parent
//! sees as the `spawn_subagent` tool's return value; the rest of
//! the sub-agent's transcript is kept addressable via its
//! `sub_session_id` for the UI's pop-out view (Phase E).
//!
//! Today's contract:
//!
//! - Mode is `Research` or `Coder`. `Research` blocks `write_file`
//!   and `edit_file` at the dispatch boundary (see
//!   [`ToolRegistry::dispatch`]); the "no mutation via `bash`" half
//!   is behavioural and lives in the system prompt.
//! - Sub-agents inherit the parent's everyday driver model
//!   ([`crate::models::CoderModels::standard`]). There used to be
//!   a `fast`/`large` selector; we dropped it because (1) it
//!   implied sub-agents were second-class workers, which made the
//!   parent reluctant to delegate non-trivial tasks; and (2) it
//!   was unused complexity — the team uses one model for actual
//!   work and a separate cheap model for the auto-rename title
//!   generator + compaction summaries. The cheap model
//!   ([`crate::models::CoderModels::cheap`]) is still used
//!   internally by sub-agents for compaction; it's not selectable
//!   per-call.
//! - Iteration cap: same [`MAX_TURN_ITERATIONS`] as the parent.
//!   Sub-agents used to run a tighter cap (50) on the assumption
//!   they were scoped tasks, but in practice that just made them
//!   bail mid-refactor when delegated a meaty chunk of work; auto-
//!   compaction handles the "context too big" failure mode the
//!   older byte cap was approximating, so there's no reason to
//!   throttle a sub-agent harder than its parent.
//! - Token-aware compaction kicks in at the same threshold as the
//!   parent loop ([`crate::compaction::COMPACT_THRESHOLD`]); the
//!   summary becomes a synthetic `system` message that replaces
//!   the older prefix.
//! - Depth cap: hardcoded to 1. The sub-agent's own tool list does
//!   not include `spawn_subagent`, so a sub-agent literally cannot
//!   spawn a sub-sub-agent.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use moon_core::WorkspaceFolderEntry;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::compaction;
use crate::defaults::MAX_TURN_ITERATIONS;
use crate::error::CoderError;
use crate::event::CoderEvent;
use crate::inference::{
	AssistantResponse, ChatMessage, FunctionCall, InferenceClient, StreamEvent, TokenUsage, ToolDefinition,
};
use crate::models::CoderModels;
use crate::runner::FolderEventSink;
use crate::sessions::{
	self, current_time_ms, new_session_id, sessions_dir, subagent_session_dir, SessionHeader, SessionRecord,
	SESSION_SCHEMA_VERSION,
};
use crate::tools::{CoderMode, ToolContext, ToolRegistry};

/// Generated as `sub-<19-char-id>`. Different prefix from session
/// ids (`sess-...`) so a tail of the events can tell them apart at
/// a glance. Reuses [`new_session_id`]'s entropy generator so we
/// don't ship a second timestamp scheme.
fn new_subagent_id() -> String {
	format!("sub-{}", new_session_id().trim_start_matches("sess-"))
}

/// Caller-provided plan for one sub-agent run. Built by the
/// parent's `spawn_subagent` tool dispatch from validated args.
#[derive(Debug, Clone)]
pub struct Subagent {
	pub id: String,
	pub parent_session_id: String,
	pub parent_tool_call_id: String,
	/// Bound folder of the **parent** session that originated this
	/// sub-agent. Persistence routes the sub-agent's JSONL under
	/// this folder's slug so it shows up in the parent project's
	/// session list (per the multi-session decision: sub-agents
	/// belong to whichever project originated them, not whichever
	/// folder they happened to operate against).
	pub parent_folder: Utf8PathBuf,
	pub task: String,
	pub system_prompt_override: Option<String>,
	pub mode: CoderMode,
	/// Folder the sub-agent's tools operate against. May differ
	/// from `parent_folder` when the model passed an explicit
	/// `folder` argument to `spawn_subagent`. Surfaced as
	/// `target_folder` in events / persistence metadata.
	pub folder: Arc<WorkspaceFolderEntry>,
}

/// What the sub-agent runner returns to the parent's tool
/// dispatch. `result` is the only field the parent's model sees
/// (as the tool's stringified return value); the others are
/// metadata the UI uses to render the collapsed card / pop-out.
#[derive(Debug, Clone)]
pub struct SubagentReport {
	pub result: String,
	pub tokens_used_estimate: u32,
	pub sub_session_id: String,
	pub mode: CoderMode,
	pub iterations_used: u32,
}

/// JSON tool definition for `spawn_subagent`. Lives outside the
/// `ToolRegistry` so sub-agents (which use the registry's
/// `definitions()`) don't see the spawn tool — that's how depth
/// is enforced. The parent's `run_turn` appends this to the tool
/// list it advertises.
pub fn spawn_subagent_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"spawn_subagent",
		"Delegate a self-contained task to a sub-agent and get back a single summarised string. Sub-agents run in their own context with their own LLM round-trips — you spend tokens on the task description and the final summary, not on every intermediate read or edit. \
\
Reach for this when one of these applies: \
**(1) context preservation** — when the inputs are large but the answer is small (`grep`-then-read sweeps, \"is feature X already implemented?\", \"find every callsite of Y\", \"summarise this folder\"), spawning a sub-agent keeps the noisy tool output out of your own transcript. Your synthesis turn stays focused on the question, not the rummaging. \
**(2) parallelism** — multiple `spawn_subagent` calls in one assistant message run concurrently (capped at 4), so an N-way investigation finishes in one round-trip instead of N. Use this when sub-tasks are independent. \
**(3) scoped delegation** — when you want a fresh agent to take ownership of a self-contained piece of work (\"port this client to the new endpoints\", \"investigate why these tests fail\") without your own session's prior context biasing the approach. \
\
For mechanically related cross-folder work (you know exactly what to change in folder B because of work you just did in folder A), you do **not** need a sub-agent — your own tools accept `/workspace/<other-name>/...` paths and operate against any bound folder directly. Sub-agents are for *delegation*, not for *access*. \
\
Mode: `\"research\"` (recommended for investigations) gets `read_file`, `list_dir`, `grep`, and `bash` for inspection commands (`git log`, `git diff`, `cargo check`, `pytest --collect-only`, …) but is instructed not to mutate anything. `\"agent\"` (default) is the full toolkit — same capabilities as you have, including edits. \
\
The sub-agent has no access to your conversation history — describe the task self-containedly. Sub-agents cannot spawn further sub-agents.",
		json!({
			"type": "object",
			"properties": {
				"task": {
					"type": "string",
					"description": "Self-contained description of what the sub-agent should do. Include any context the sub-agent needs — it does not see the parent's transcript."
				},
				"folder": {
					"type": "string",
					"description": "Basename of a currently-bound workspace folder to scope the sub-agent against (matches the `<name>` in `/workspace/<name>` from the Bound folders section). Omit (or set to the active folder's basename) to target the parent's active folder — useful for context-isolation even within the same folder. Targeting an unbound folder errors."
				},
				"mode": {
					"type": "string",
					"enum": ["research", "agent"],
					"description": "`research` is read-only intent (read_file, list_dir, grep, bash for inspection); `agent` (default) is the full toolkit and may edit files. The sub-agent in `agent` mode has the same capabilities you do."
				},
				"system_prompt": {
					"type": "string",
					"description": "Optional override for the sub-agent's system prompt. Most callers should leave this empty and rely on the mode-default prompt."
				}
			},
			"required": ["task"]
		}),
	)
}

/// Run a sub-agent to completion (or a budget cap, or
/// cancellation). Mirrors the structure of `run_turn` but emits
/// every event wrapped in [`CoderEvent::SubagentEvent`] so the UI
/// can route updates to the right collapsed card / pop-out pane.
/// Persists a JSONL transcript at
/// `<coder_sessions_dir>/<parent-folder-slug>/<parent-session-id>/<sub-id>.jsonl`,
/// with a header carrying the parent's session id + tool_call_id
/// so a "pop out" lookup survives IDE restarts.
pub(crate) async fn run_subagent(
	tools: &ToolRegistry,
	inference: &InferenceClient,
	sink: &FolderEventSink,
	coder_sessions_dir: &Utf8Path,
	models: &CoderModels,
	spec: Subagent,
	cancel: CancellationToken,
) -> Result<SubagentReport, CoderError> {
	let id = spec.id.clone();
	let mode = spec.mode;
	sink.send(CoderEvent::SubagentSpawned {
		tool_call_id: spec.parent_tool_call_id.clone(),
		subagent_id: id.clone(),
		target_folder: spec.folder.folder.path.clone(),
		mode: mode.as_wire().to_string(),
	});

	let outcome = run_subagent_inner(tools, inference, sink, coder_sessions_dir, models, &spec, cancel).await;

	let was_error = outcome.is_err();
	sink.send(CoderEvent::SubagentFinished {
		subagent_id: id.clone(),
		tokens_used_estimate: outcome.as_ref().ok().map(|r| r.tokens_used_estimate).unwrap_or(0),
		was_error,
	});
	outcome
}

async fn run_subagent_inner(
	tools: &ToolRegistry,
	inference: &InferenceClient,
	sink: &FolderEventSink,
	coder_sessions_dir: &Utf8Path,
	models: &CoderModels,
	spec: &Subagent,
	cancel: CancellationToken,
) -> Result<SubagentReport, CoderError> {
	let standard_model = models.standard().to_owned();
	let id = spec.id.clone();
	let system_prompt = spec
		.system_prompt_override
		.clone()
		.unwrap_or_else(|| build_subagent_system_prompt(spec.mode, &spec.folder, &spec.task));
	let mut messages: Vec<ChatMessage> = vec![
		ChatMessage::System {
			content: system_prompt.clone(),
		},
		ChatMessage::User {
			content: spec.task.clone(),
		},
	];
	// Most-recent provider-supplied usage. Populated whenever an
	// LLM call returns a `usage` block; `None` when every round
	// fell back to the bytes/4 estimate. Used both to drive the
	// compaction trigger and as the eventual `tokens_used_estimate`
	// the parent sees back.
	let mut last_usage: Option<TokenUsage> = None;
	// Sub-agent-local todo list. Sub-agents see `todo_write`
	// advertised on the same footing the parent does and maintain
	// their own scratchpad; it never bubbles up to the parent's
	// pill (the parent has its own list) but the per-call result
	// renders inside the sub-agent's transcript card.
	let mut todos: Vec<crate::TodoItem> = Vec::new();

	// JSONL transcript lives under the **parent** folder's slug,
	// nested inside a per-parent-session subdirectory:
	// `<sessions_dir>/<parent_session_id>/<sub-id>.jsonl`. Sub-agents
	// belong to whichever project originated them, and grouping by
	// parent session means listing the sessions dir flat returns
	// only top-level sessions (the picker stays clean) while
	// `<parent_id>/` keeps every sub-agent that ran during that
	// conversation in one obvious spot. The header carries
	// `target_folder` as metadata so the UI can still show which
	// folder the sub-agent was scoped to.
	let parent_dir = sessions_dir(coder_sessions_dir, spec.parent_folder.as_path());
	let session_dir = subagent_session_dir(&parent_dir, &spec.parent_session_id);
	let now = current_time_ms();
	// `subagent_target_folder` is `Some(...)` only when the
	// sub-agent operated against a folder different from the
	// parent's (i.e. an explicit `folder` argument was passed to
	// `spawn_subagent`). When the sub-agent targets the same
	// folder as its parent, `None` keeps the on-disk header tidy.
	let target_folder_path = spec.folder.folder.path.clone();
	let target_differs = Utf8Path::new(target_folder_path.as_str()) != spec.parent_folder.as_path();
	let header = SessionHeader {
		schema: SESSION_SCHEMA_VERSION,
		id: id.clone(),
		title: subagent_session_title(&spec.task),
		created_at_ms: now,
		updated_at_ms: now,
		// Informational seed; the actual model used per round-trip
		// comes from the parent's `CoderModels` snapshot at
		// `run_subagent` time. See note in
		// [`crate::runner::Session::new_blank`].
		model: standard_model.clone(),
		parent_session_id: Some(spec.parent_session_id.clone()),
		parent_tool_call_id: Some(spec.parent_tool_call_id.clone()),
		subagent_mode: Some(spec.mode.as_wire().to_string()),
		subagent_target_folder: if target_differs { Some(target_folder_path) } else { None },
	};

	// Best-effort: persistence failures log at warn but never
	// fail the sub-agent. The model still gets its answer; the
	// only loss is an absent transcript on disk for the pop-out.
	persist_subagent(
		&session_dir,
		&header,
		&SessionRecord::User {
			text: spec.task.clone(),
		},
	)
	.await;

	// The sub-agent's tool list deliberately omits `spawn_subagent`.
	// That's how the depth=1 cap is enforced: a sub-agent literally
	// cannot describe a sub-sub-agent because the model never sees
	// the tool.
	let tool_defs = tools.definitions();
	let cx = ToolContext::new(spec.folder.clone(), spec.mode);

	for iter in 0..MAX_TURN_ITERATIONS {
		if cancel.is_cancelled() {
			return Err(CoderError::Aborted);
		}

		// Compaction-before-send: if the last round's prompt size
		// is bumping into the model's context window, fold the
		// older messages into a synthetic system summary before
		// the next request goes out. No-op early in the run.
		compaction::compact_if_needed(
			inference,
			sink,
			Some(&id),
			models,
			last_usage.as_ref(),
			&mut messages,
			&cancel,
		)
		.await;

		let assistant_id = format!("{id}::msg-{iter}");
		let id_for_cb = assistant_id.clone();
		let id_for_subagent = id.clone();
		let sink_for_cb = sink.clone();
		let started = std::sync::atomic::AtomicBool::new(false);
		let response = inference
			.chat_completion_stream(&standard_model, &messages, &tool_defs, &cancel, |event| match event {
				StreamEvent::ContentDelta { delta } => {
					if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
						sink_for_cb.send(wrap_inner(
							&id_for_subagent,
							CoderEvent::AssistantMessageStart { id: id_for_cb.clone() },
						));
					}
					sink_for_cb.send(wrap_inner(
						&id_for_subagent,
						CoderEvent::AssistantMessageDelta {
							id: id_for_cb.clone(),
							delta: delta.to_string(),
						},
					));
				}
				StreamEvent::ThinkingDelta { delta } => {
					if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
						sink_for_cb.send(wrap_inner(
							&id_for_subagent,
							CoderEvent::AssistantMessageStart { id: id_for_cb.clone() },
						));
					}
					sink_for_cb.send(wrap_inner(
						&id_for_subagent,
						CoderEvent::AssistantThinkingDelta {
							id: id_for_cb.clone(),
							delta: delta.to_string(),
						},
					));
				}
				StreamEvent::ToolCallDelta { .. } => {}
			})
			.await?;
		if started.into_inner() {
			sink.send(wrap_inner(
				&id,
				CoderEvent::AssistantMessageEnd {
					id: assistant_id.clone(),
					text: response.content.clone().unwrap_or_default(),
					thinking: response.thinking.clone(),
				},
			));
		}

		if let Some(u) = response.usage {
			last_usage = Some(u);
		}
		// Wrap the parent runner's helper so the sub-agent's ring
		// updates land on the same wire as parent updates. The
		// envelope is `SubagentEvent { inner: TokenUsage … }`.
		emit_subagent_token_usage(sink, &id, models, &standard_model, &messages, &response);
		messages.push(response_to_message(&response));
		persist_subagent(
			&session_dir,
			&header,
			&SessionRecord::Assistant {
				content: response.content.clone(),
				thinking: response.thinking.clone(),
				tool_calls: response.tool_calls.clone(),
			},
		)
		.await;

		if response.tool_calls.is_empty() {
			let result_text = response.content.clone().unwrap_or_default();
			return Ok(SubagentReport {
				result: result_text,
				tokens_used_estimate: tokens_used_for_report(&messages, last_usage),
				sub_session_id: id.clone(),
				mode: spec.mode,
				iterations_used: (iter + 1) as u32,
			});
		}

		// Sub-agents dispatch their tools sequentially today —
		// recursive parallelism (a sub-agent's own tools running
		// concurrently) is out of scope for the current slice.
		// Parallelism happens *one level up*: multiple sub-agents
		// in the parent's batch run concurrently via the parent's
		// `dispatch_subagent_batch`.
		for call in &response.tool_calls {
			if cancel.is_cancelled() {
				return Err(CoderError::Aborted);
			}
			let args = parse_tool_args(&call.function);
			sink.send(wrap_inner(
				&id,
				CoderEvent::ToolCall {
					id: call.id.clone(),
					name: call.function.name.clone(),
					args: args.clone(),
				},
			));
			let outcome = if call.function.name == "todo_write" {
				// Same short-circuit shape as the parent runner —
				// `todo_write` mutates per-session state
				// (`todos`), so it can't go through the stateless
				// registry dispatch.
				handle_subagent_todo_write(&mut todos, &args, &session_dir, &header).await
			} else {
				tools.dispatch(&call.function.name, &args, &cx, &cancel).await
			};
			let (content, is_error) = match outcome {
				Ok(value) => (value.to_string(), false),
				Err(CoderError::Aborted) => return Err(CoderError::Aborted),
				Err(err) => (json!({ "error": err.to_string() }).to_string(), true),
			};
			let payload: Value = serde_json::from_str(&content).unwrap_or_else(|_| Value::String(content.clone()));
			sink.send(wrap_inner(
				&id,
				CoderEvent::ToolResult {
					id: call.id.clone(),
					result: payload,
					is_error,
				},
			));
			messages.push(ChatMessage::Tool {
				tool_call_id: call.id.clone(),
				content: content.clone(),
			});
			persist_subagent(
				&session_dir,
				&header,
				&SessionRecord::Tool {
					tool_call_id: call.id.clone(),
					content,
				},
			)
			.await;
		}
	}

	// Iteration cap reached. Mirror the parent's behaviour: ask
	// the model for one final tools-disabled wrap-up turn so the
	// parent gets a real answer back instead of a canned "stopped
	// after N iterations" stub. The wrap-up reply becomes the
	// sub-agent's `result` string (with a leading note so the
	// parent's model knows the budget was exhausted).
	let wrap_up = subagent_wrap_up(
		inference,
		sink,
		spec,
		&session_dir,
		&header,
		&standard_model,
		&mut messages,
		&cancel,
	)
	.await;
	let result = match wrap_up {
		Ok(text) if !text.trim().is_empty() => {
			format!("[Sub-agent reached the {MAX_TURN_ITERATIONS}-iteration cap; final wrap-up follows.]\n\n{text}")
		}
		_ => format!("Sub-agent stopped after {MAX_TURN_ITERATIONS} iterations without producing a final answer.",),
	};
	Ok(SubagentReport {
		result,
		tokens_used_estimate: tokens_used_for_report(&messages, last_usage),
		sub_session_id: id.clone(),
		mode: spec.mode,
		iterations_used: MAX_TURN_ITERATIONS as u32,
	})
}

/// Final tools-disabled round-trip the sub-agent runs after its
/// iteration cap is hit. Same idea as the parent's
/// [`wrap_up_final_answer`](crate::runner::wrap_up_final_answer):
/// inject a sentinel user message asking the model to write its
/// best answer with what it has, then call inference with
/// `tools = []` so it can't loop again. Streams the response
/// inside [`CoderEvent::SubagentEvent`] envelopes so the pop-out
/// UI sees it land like any other sub-agent turn.
///
/// Returns the text of the wrap-up answer for the caller to wire
/// into [`SubagentReport::result`]. Errors / empty responses fall
/// back to the historical canned message in the caller.
// Eight args is a hair over clippy's seven-arg cap, but every one
// of them is genuinely needed by the wrap-up flow — `header` for
// persistence (sub-agents write their wrap-up reply into the same
// JSONL as the rest of the run), `session_dir` likewise,
// `standard_model` for the LLM call. Bundling any of these into
// an aux struct just for the arg-count gate would obscure the
// call site for no real signal.
#[allow(clippy::too_many_arguments)]
async fn subagent_wrap_up(
	inference: &InferenceClient,
	sink: &FolderEventSink,
	spec: &Subagent,
	session_dir: &Utf8Path,
	header: &SessionHeader,
	standard_model: &str,
	messages: &mut Vec<ChatMessage>,
	cancel: &CancellationToken,
) -> Result<String, CoderError> {
	let id = spec.id.as_str();
	tracing::info!(
		subagent_id = %id,
		iterations = MAX_TURN_ITERATIONS,
		"sub-agent iteration cap reached; running final tools-disabled wrap-up",
	);
	let sentinel = format!(
		"[Tool-call budget exhausted: you've used all {MAX_TURN_ITERATIONS} tool-call iterations available for this sub-agent. \
Do not call any more tools. Write a final response now using only what you've already gathered: summarise findings, what's still unfinished, and any uncertainty.]"
	);
	messages.push(ChatMessage::User {
		content: sentinel.clone(),
	});
	persist_subagent(session_dir, header, &SessionRecord::User { text: sentinel.clone() }).await;

	let assistant_id = format!("{id}::wrap-up");
	let id_for_cb = assistant_id.clone();
	let id_for_subagent = id.to_string();
	let sink_for_cb = sink.clone();
	let started = std::sync::atomic::AtomicBool::new(false);
	let response = inference
		.chat_completion_stream(standard_model, messages, &[], cancel, |event| match event {
			StreamEvent::ContentDelta { delta } => {
				if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
					sink_for_cb.send(wrap_inner(
						&id_for_subagent,
						CoderEvent::AssistantMessageStart { id: id_for_cb.clone() },
					));
				}
				sink_for_cb.send(wrap_inner(
					&id_for_subagent,
					CoderEvent::AssistantMessageDelta {
						id: id_for_cb.clone(),
						delta: delta.to_string(),
					},
				));
			}
			StreamEvent::ThinkingDelta { delta } => {
				if !started.swap(true, std::sync::atomic::Ordering::Relaxed) {
					sink_for_cb.send(wrap_inner(
						&id_for_subagent,
						CoderEvent::AssistantMessageStart { id: id_for_cb.clone() },
					));
				}
				sink_for_cb.send(wrap_inner(
					&id_for_subagent,
					CoderEvent::AssistantThinkingDelta {
						id: id_for_cb.clone(),
						delta: delta.to_string(),
					},
				));
			}
			// Tools were disabled in the request; drop any stragglers.
			StreamEvent::ToolCallDelta { .. } => {}
		})
		.await?;

	if started.into_inner() {
		sink.send(wrap_inner(
			id,
			CoderEvent::AssistantMessageEnd {
				id: assistant_id,
				text: response.content.clone().unwrap_or_default(),
				thinking: response.thinking.clone(),
			},
		));
	}

	let final_text = response.content.clone().unwrap_or_default();
	messages.push(response_to_message(&response));
	persist_subagent(
		session_dir,
		header,
		&SessionRecord::Assistant {
			content: response.content.clone(),
			thinking: response.thinking.clone(),
			tool_calls: response.tool_calls.clone(),
		},
	)
	.await;
	Ok(final_text)
}

/// Sub-agent-specific wrapper around the parent runner's
/// [`emit_token_usage`](crate::runner::emit_token_usage). The
/// inner event is a [`CoderEvent::TokenUsage`] just like the
/// parent's, but wrapped in [`CoderEvent::SubagentEvent`] so the
/// frontend can target the right nested ring.
fn emit_subagent_token_usage(
	sink: &FolderEventSink,
	subagent_id: &str,
	models: &crate::models::CoderModels,
	model_slug: &str,
	messages: &[ChatMessage],
	response: &AssistantResponse,
) {
	let context_window = models.context_window(model_slug);
	let (prompt_tokens, completion_tokens, total_tokens, cache_read_tokens, cache_creation_tokens, source) =
		match response.usage {
			Some(u) => (
				u.prompt_tokens,
				u.completion_tokens,
				u.total_tokens,
				u.cache_read_input_tokens,
				u.cache_creation_input_tokens,
				crate::event::TokenUsageSource::Provider,
			),
			None => {
				let prompt = crate::runner::estimate_prompt_tokens(messages);
				// Mirror the parent's bytes/4 for completion side; the
				// inference module's helper isn't reachable here and
				// duplicating four lines is cleaner than threading it.
				let completion = (response.content.as_deref().map(str::len).unwrap_or(0)
					+ response.thinking.as_deref().map(str::len).unwrap_or(0)
					+ response
						.tool_calls
						.iter()
						.map(|c| c.function.name.len() + c.function.arguments.len())
						.sum::<usize>()) as u32
					/ 4;
				(
					prompt,
					completion,
					prompt + completion,
					0,
					0,
					crate::event::TokenUsageSource::Estimate,
				)
			}
		};
	sink.send(wrap_inner(
		subagent_id,
		CoderEvent::TokenUsage {
			prompt_tokens,
			completion_tokens,
			total_tokens,
			context_window,
			source,
			cache_read_tokens,
			cache_creation_tokens,
		},
	));
}

/// Choose the best available "tokens used" number for the report
/// the parent's tool dispatch sees. Prefer the final
/// provider-supplied usage; fall back to the parent runner's
/// bytes/4 estimator computed from the message history.
fn tokens_used_for_report(messages: &[ChatMessage], last_usage: Option<TokenUsage>) -> u32 {
	if let Some(u) = last_usage {
		return u.total_tokens.max(u.prompt_tokens + u.completion_tokens);
	}
	crate::runner::estimate_prompt_tokens(messages)
}

fn wrap_inner(subagent_id: &str, inner: CoderEvent) -> CoderEvent {
	CoderEvent::SubagentEvent {
		subagent_id: subagent_id.to_string(),
		inner: Box::new(inner),
	}
}

/// Compose a short, human-readable title for the sub-agent's
/// session list entry. Prefixed with `Sub-agent:` so the
/// persisted-session UI can tell parent and sub-agent sessions
/// apart at a glance even before the dedicated badge ships.
fn subagent_session_title(task: &str) -> String {
	const TITLE_BODY_LIMIT: usize = 60;
	let mut head = task.trim().lines().next().unwrap_or("").trim().to_string();
	if head.len() > TITLE_BODY_LIMIT {
		let mut idx = TITLE_BODY_LIMIT;
		while idx > 0 && !head.is_char_boundary(idx) {
			idx -= 1;
		}
		head.truncate(idx);
		head.push('…');
	}
	if head.is_empty() {
		"Sub-agent".into()
	} else {
		format!("Sub-agent: {head}")
	}
}

async fn persist_subagent(dir: &Utf8Path, header: &SessionHeader, record: &SessionRecord) {
	if let Err(err) = sessions::append_record(dir, header, record).await {
		// Persistence is best-effort: the parent's tool result
		// still carries the sub-agent's answer; the only thing
		// we lose on a write failure is the per-step transcript
		// the pop-out view would render. Logged at warn so it
		// doesn't slip silently.
		tracing::warn!(error = %err, "failed to persist sub-agent record");
	}
}

fn response_to_message(response: &AssistantResponse) -> ChatMessage {
	ChatMessage::Assistant {
		content: response.content.clone(),
		tool_calls: response.tool_calls.clone(),
	}
}

fn parse_tool_args(function: &FunctionCall) -> Value {
	serde_json::from_str(&function.arguments).unwrap_or(Value::Null)
}

/// Sub-agent counterpart to [`crate::runner::handle_todo_write`].
/// Same wire shape, same validation, same persistence record —
/// the only difference is the storage cell: this one mutates the
/// sub-agent's local `todos` vec (passed by `&mut`) and writes
/// into the sub-agent's per-parent JSONL via [`persist_subagent`].
/// Sub-agents see `todo_write` advertised the same way the parent
/// does (per the spec), so each sub-agent gets its own scratchpad
/// without bleeding into the parent's plan.
async fn handle_subagent_todo_write(
	todos: &mut Vec<crate::TodoItem>,
	args: &Value,
	session_dir: &Utf8Path,
	header: &SessionHeader,
) -> Result<Value, CoderError> {
	#[derive(serde::Deserialize)]
	struct TodoWriteArgs {
		todos: Vec<crate::TodoItem>,
		#[serde(default)]
		merge: bool,
	}
	let parsed: TodoWriteArgs =
		serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("todo_write", err.to_string()))?;
	for item in &parsed.todos {
		if item.id.trim().is_empty() {
			return Err(CoderError::invalid_args(
				"todo_write",
				"todo item `id` must be a non-empty string",
			));
		}
	}
	let merged = crate::merge_todos(todos, parsed.todos, parsed.merge);
	*todos = merged.clone();
	persist_subagent(
		session_dir,
		header,
		&SessionRecord::TodosUpdate { todos: merged.clone() },
	)
	.await;
	Ok(json!({ "todos": merged }))
}

const RESEARCH_SYSTEM_PROMPT: &str = r#"You are a research sub-agent inside moon-ide. You have been spawned by a parent agent to gather information from a workspace folder and report back. Your job is investigation, not editing.

Tools available: `read_file`, `list_dir`, `grep`, and `bash`. You are scoped to a single folder (shown below); `grep` and `bash` run against it, and relative paths resolve inside it. You cannot spawn further sub-agents.

The shell is for read-only inspection only — `git log`, `git diff --stat`, `cargo check`, `pytest --collect-only`, `ls -la`, `cat`, `wc`, and similar. Do **not** run commands that mutate the filesystem, network state, or remote services. No `git commit`, `git push`, `cargo build` (it writes to `target/`), `mv`, `rm`, `npm install`, `pip install`, redirection-to-file (`> path`, `>> path`), or anything that would change persistent state.

Return your findings as a single coherent text result when you finish. The parent will see only that string; do not address them as "you" or assume shared context.
"#;

const AGENT_SYSTEM_PROMPT: &str = r#"You are an agent sub-agent inside moon-ide. You have been spawned by a parent agent to perform a focused task in a workspace folder. Your capabilities are the same as the parent's — you can read, search, run commands, and edit files freely.

Tools available: `read_file`, `list_dir`, `grep`, `bash`, `write_file`, `edit_file`. You are scoped to a single folder (shown below); `grep` and `bash` run against it, and relative paths resolve inside it. You cannot spawn further sub-agents.

Read before you edit — don't invent file paths. Use `edit_file` for surgical changes inside large files; reach for `write_file` for new files and whole-file rewrites.

Return a single coherent text result when you finish. The parent will see only that string and a short transcript of your tool calls; do not address them as "you" or assume shared context.
"#;

fn build_subagent_system_prompt(mode: CoderMode, folder: &Arc<WorkspaceFolderEntry>, task: &str) -> String {
	let base = match mode {
		CoderMode::Research => RESEARCH_SYSTEM_PROMPT,
		CoderMode::Agent => AGENT_SYSTEM_PROMPT,
	};
	let header = format!(
		"## Task\n\n{task}\n\n## Working folder\n\n- **{name}** at `{path}`\n",
		task = task.trim(),
		name = folder.folder.name,
		path = folder.folder.path,
	);
	let mut out = String::with_capacity(base.len() + header.len() + 16);
	out.push_str(base.trim_end());
	out.push_str("\n\n");
	out.push_str(&header);
	out
}

/// Validate + materialise a `Subagent` from JSON args + parent
/// context. Surfaces actionable errors back to the parent's model
/// (folder not bound, unknown mode/model strings) so it can
/// recover or adjust.
///
/// `parent_folder` is the absolute path of the parent session's
/// bound folder — the sub-agent's JSONL persists under this
/// folder's slug so the sub-agent file shows up in the parent
/// project's session list, regardless of which folder the
/// sub-agent's tools operate against.
pub fn build_subagent_spec(
	parent_session_id: String,
	parent_tool_call_id: String,
	parent_folder: Utf8PathBuf,
	args: &Value,
	parent_active_folder: &Arc<WorkspaceFolderEntry>,
	bound_folders: &[Arc<WorkspaceFolderEntry>],
) -> Result<Subagent, CoderError> {
	let task = args
		.get("task")
		.and_then(Value::as_str)
		.ok_or_else(|| CoderError::invalid_args("spawn_subagent", "missing required string field `task`"))?
		.to_string();
	if task.trim().is_empty() {
		return Err(CoderError::invalid_args("spawn_subagent", "`task` must not be empty"));
	}

	let folder = match args.get("folder").and_then(Value::as_str) {
		None | Some("") => parent_active_folder.clone(),
		Some(name) => find_bound_folder(bound_folders, name)
			.ok_or_else(|| CoderError::tool_failed("spawn_subagent", format!("folder `{name}` is not bound")))?,
	};

	let mode = match args.get("mode").and_then(Value::as_str) {
		None | Some("agent") => CoderMode::Agent,
		Some("research") => CoderMode::Research,
		Some(other) => {
			return Err(CoderError::invalid_args(
				"spawn_subagent",
				format!("unknown mode `{other}` — expected `research` or `agent`"),
			));
		}
	};

	let system_prompt_override = args
		.get("system_prompt")
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|s| !s.is_empty())
		.map(str::to_string);

	Ok(Subagent {
		id: new_subagent_id(),
		parent_session_id,
		parent_tool_call_id,
		parent_folder,
		task,
		system_prompt_override,
		mode,
		folder,
	})
}

fn find_bound_folder(folders: &[Arc<WorkspaceFolderEntry>], name: &str) -> Option<Arc<WorkspaceFolderEntry>> {
	// Match by basename first (the model usually says "moon-landing"
	// not the full absolute path) and fall back to absolute-path
	// equality so a model that does send a full path still works.
	if let Some(by_basename) = folders.iter().find(|f| f.folder.name == name) {
		return Some(by_basename.clone());
	}
	let candidate = Utf8Path::new(name);
	folders.iter().find(|f| f.folder.path == candidate.as_str()).cloned()
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_core::WorkspaceRegistry;
	use tempfile::TempDir;

	fn make_args(json_text: &str) -> Value {
		serde_json::from_str(json_text).unwrap()
	}

	async fn registry_with_folders(paths: &[&Utf8Path]) -> Vec<Arc<WorkspaceFolderEntry>> {
		let registry = WorkspaceRegistry::new("test-workspace".into());
		for path in paths {
			registry.add_folder(path.to_path_buf()).await.unwrap();
		}
		registry.folders().await
	}

	fn parent_folder_for(folders: &[Arc<WorkspaceFolderEntry>], idx: usize) -> Utf8PathBuf {
		Utf8PathBuf::from(folders[idx].folder.path.clone())
	}

	#[tokio::test]
	async fn build_spec_defaults_to_active_folder_and_coder_mode() {
		let dir = TempDir::new().unwrap();
		let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let folders = registry_with_folders(&[path.as_path()]).await;
		let active = folders[0].clone();
		let parent_folder = parent_folder_for(&folders, 0);
		let args = make_args(r#"{ "task": "do the thing" }"#);
		let spec = build_subagent_spec(
			"sess-x".into(),
			"call-1".into(),
			parent_folder.clone(),
			&args,
			&active,
			&folders,
		)
		.unwrap();
		assert_eq!(spec.mode, CoderMode::Agent);
		assert_eq!(spec.task, "do the thing");
		assert!(Arc::ptr_eq(&spec.folder, &active));
		assert_eq!(spec.parent_folder, parent_folder);
	}

	#[tokio::test]
	async fn build_spec_routes_research_mode_correctly() {
		let dir = TempDir::new().unwrap();
		let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let folders = registry_with_folders(&[path.as_path()]).await;
		let active = folders[0].clone();
		let args = make_args(r#"{ "task": "find auth code", "mode": "research" }"#);
		let spec = build_subagent_spec(
			"sess-x".into(),
			"call-1".into(),
			parent_folder_for(&folders, 0),
			&args,
			&active,
			&folders,
		)
		.unwrap();
		assert_eq!(spec.mode, CoderMode::Research);
	}

	#[tokio::test]
	async fn build_spec_rejects_unknown_mode_string() {
		let dir = TempDir::new().unwrap();
		let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let folders = registry_with_folders(&[path.as_path()]).await;
		let args = make_args(r#"{ "task": "x", "mode": "rogue" }"#);
		let err = build_subagent_spec(
			"sess-x".into(),
			"call-1".into(),
			parent_folder_for(&folders, 0),
			&args,
			&folders[0],
			&folders,
		)
		.unwrap_err();
		assert!(matches!(err, CoderError::InvalidToolArgs { .. }), "got {err:?}");
	}

	#[tokio::test]
	async fn build_spec_rejects_unbound_folder_basename() {
		let dir = TempDir::new().unwrap();
		let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let folders = registry_with_folders(&[path.as_path()]).await;
		let args = make_args(r#"{ "task": "x", "folder": "no-such-folder" }"#);
		let err = build_subagent_spec(
			"sess-x".into(),
			"call-1".into(),
			parent_folder_for(&folders, 0),
			&args,
			&folders[0],
			&folders,
		)
		.unwrap_err();
		assert!(matches!(err, CoderError::ToolFailed { .. }), "got {err:?}");
	}

	#[tokio::test]
	async fn build_spec_finds_bound_folder_by_basename() {
		let one = TempDir::new().unwrap();
		let two = TempDir::new().unwrap();
		let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
		let two_path = camino::Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();
		let folders = registry_with_folders(&[one_path.as_path(), two_path.as_path()]).await;
		let other_basename = folders[1].folder.name.clone();
		let args_text = format!(r#"{{ "task": "x", "folder": "{other_basename}" }}"#);
		let spec = build_subagent_spec(
			"sess-x".into(),
			"call-1".into(),
			parent_folder_for(&folders, 0),
			&make_args(&args_text),
			&folders[0],
			&folders,
		)
		.unwrap();
		assert!(Arc::ptr_eq(&spec.folder, &folders[1]));
		// Sub-agent's persistence still belongs to the **parent's**
		// folder slug, not the target's.
		assert_eq!(spec.parent_folder, parent_folder_for(&folders, 0));
	}

	#[test]
	fn build_subagent_system_prompt_includes_task_and_folder() {
		// Synthetic entry — we just need a folder shape, not an
		// actual host. Construct from `WorkspaceRegistry` would
		// require an existing dir, this is enough for the prompt
		// composer.
		let folder = Arc::new(WorkspaceFolderEntry {
			folder: moon_protocol::workspace::WorkspaceFolder {
				path: "/abs/path/to/proj".into(),
				name: "proj".into(),
				host: moon_protocol::workspace::HostKind::Local,
			},
			host: Arc::new(moon_core::LocalHost::new(camino::Utf8PathBuf::from(
				"/abs/path/to/proj",
			))),
		});
		let prompt = build_subagent_system_prompt(CoderMode::Research, &folder, "investigate auth flow");
		assert!(prompt.contains("research sub-agent"));
		assert!(prompt.contains("investigate auth flow"));
		assert!(prompt.contains("/abs/path/to/proj"));
	}
}
