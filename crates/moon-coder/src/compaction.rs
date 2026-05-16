//! Auto-compaction of long agent sessions.
//!
//! When the per-turn token report says the next prompt is going to
//! eat ~80% or more of the model's context window, we fold the
//! older middle of the message history into a synthetic
//! [`ChatMessage::System`] block authored by the fast model, and
//! the runner continues from there. The leading system prompt
//! (recomposed every turn by `refresh_system_prompt`) and the
//! recent K user/assistant turns ride through unchanged so the
//! agent doesn't lose its current focus.
//!
//! Wire shape:
//!
//! ```text
//! before:
//!   [system: composed]                               ← messages[0]
//!   user … assistant … tool … assistant … user …    ← long history
//!   user (most recent)
//!
//! after:
//!   [system: composed]                               ← messages[0]
//!   [system: <summary of older middle>]              ← messages[1] (new)
//!   user … assistant … tool …                       ← last K user turns kept
//!   user (most recent)
//! ```
//!
//! The leading system isn't reinjected explicitly — `runner.rs`
//! calls `refresh_system_prompt` at the top of every turn, which
//! overwrites `messages[0]` with the freshly-rendered base prompt
//! plus the bound-folders and folder-summary block. The synthetic
//! summary at `messages[1]` is left alone by that pass.
//!
//! On-disk JSONL transcripts are **not** rewritten. The session
//! file keeps the full history; only the *in-memory* prompt the
//! next round-trip sends gets compacted. That preserves
//! pop-out-debug-the-session and audit, at the cost of one log
//! that the on-disk transcript can be longer than what's in
//! flight at any moment.
//!
//! Triggers:
//! - Parent loop and sub-agent loop both call [`compact_if_needed`]
//!   right before each LLM round-trip.
//! - Threshold is hardcoded today ([`COMPACT_THRESHOLD`]); per
//!   AGENTS.md "hardcode first, configure later".

use tokio_util::sync::CancellationToken;

use crate::event::CoderEvent;
use crate::inference::{ChatMessage, InferenceClient, TokenUsage};
use crate::models::CoderModels;
use crate::runner::{estimate_prompt_tokens, FolderEventSink};

/// Fraction of the model's context window that triggers a
/// compaction pass. 0.80 leaves ~20% headroom for the next
/// prompt's user turn + the model's response — generous enough
/// that we don't compact every turn, tight enough that a single
/// long tool result can't push us over the wire limit before the
/// next compaction has a chance to run.
pub const COMPACT_THRESHOLD: f32 = 0.80;

/// Number of most-recent **user** turns to keep verbatim. Each
/// user turn carries its full assistant + tool reply chain through
/// to the next user turn. With K=6 the model still sees what it
/// just said, what tools it ran, and what it concluded; everything
/// before that gets folded.
const RECENT_USER_TURNS_KEPT: usize = 6;

/// Header prepended to the synthetic summary system message so
/// the model can recognise it for what it is. Important: this
/// distinguishes the compaction summary from the composed system
/// prompt at `messages[0]`, so a future compaction pass can still
/// find and operate on it.
const COMPACTION_HEADER: &str = "## Earlier conversation summary\n\n\
This summarises the prefix of this conversation that was compacted to fit the model's context window. \
The original turns are preserved on disk in the session transcript; only the in-memory prompt was shortened.\n\n";

/// System prompt fed to the fast model when asking it to write
/// the summary. Hardcoded; not exposed for configuration. The
/// shape mirrors what Claude Code's `/compact` produces and what
/// the team's existing pi-code muscle memory expects to see in
/// the panel's collapsed disclosure.
const SUMMARY_SYSTEM_PROMPT: &str = "\
You are an internal compaction assistant inside an AI coding agent. You will be given the prefix of an in-flight \
coding session that needs to be summarised so the agent can keep working without exceeding its context window. \
Produce a single dense markdown summary covering, in this order: (1) the user's overall intent and any explicit \
goals stated, (2) major decisions made and their rationale, (3) every file or symbol touched and what changed, \
(4) tools that were used and what they returned (collapse repeated reads/greps into a single line), (5) the \
current state of the work — what is in progress, what was just attempted, and any errors or warnings still \
outstanding, (6) anything the agent must remember to do next or constraints it has accepted. Do not address the \
user; write in third-person past tense (\"the user asked\", \"the assistant edited\"). Do not invent details \
that aren't in the prefix. Do not include the entire transcript verbatim. Aim for somewhere between 4,000 and \
16,000 tokens of output — long enough to be useful, short enough not to dominate the next round-trip's window.";

/// Outcome of one [`compact_if_needed`] call. The summary +
/// `messages_compacted` are what the caller persists into the
/// session JSONL as a [`crate::sessions::SessionRecord::Compaction`]
/// so reopening the session reaches the same compacted in-memory
/// shape instead of re-inflating the full pre-compaction
/// transcript.
pub(crate) struct CompactionApplied {
	pub summary: String,
	pub messages_compacted: u32,
}

/// Inspect the last reported token usage; if the next prompt is
/// likely to cross [`COMPACT_THRESHOLD`] of the context window,
/// run a fast-model summary call and replace the older prefix of
/// `messages` with a synthetic [`ChatMessage::System`] holding
/// that summary.
///
/// Returns `Some` when compaction actually ran (and `messages`
/// was mutated) — the caller is responsible for persisting the
/// returned summary as a `SessionRecord::Compaction` so replay
/// reaches the same shape. Returns `None` when the threshold
/// wasn't met, when there isn't enough history to compact, or
/// when the fast model call itself failed (logged at warn — the
/// agent keeps going and will try again on the next turn).
///
/// `subagent_id_for_wrap` distinguishes parent vs sub-agent
/// callers. When `Some(id)`, every emitted [`CoderEvent`] is
/// wrapped in [`CoderEvent::SubagentEvent`] so the frontend
/// routes the compaction row to the matching sub-agent card.
pub(crate) async fn compact_if_needed(
	inference: &InferenceClient,
	sink: &FolderEventSink,
	subagent_id_for_wrap: Option<&str>,
	models: &CoderModels,
	last_usage: Option<&TokenUsage>,
	messages: &mut Vec<ChatMessage>,
	cancel: &CancellationToken,
) -> Option<CompactionApplied> {
	let usage = last_usage?;
	// Context-window cap is a property of the *driver* model — the
	// one whose history we're trying to fit. The cheap model only
	// has to chew through `messages[1..cutoff]` for the summary;
	// its own window doesn't gate the decision.
	let context = models.context_window(models.standard());
	if context == 0 {
		return None;
	}
	let ratio = usage.prompt_tokens as f32 / context as f32;
	if ratio < COMPACT_THRESHOLD {
		return None;
	}

	let Some(cutoff) = find_cutoff_index(messages) else {
		// Not enough history yet; one big turn pushed us over the
		// threshold but there's nothing useful to summarise.
		// Compaction would be a no-op (or worse, would summarise
		// the only turn we have). Bail; the model will get a
		// truncation error on the wire if this really is too
		// large, which beats summarising the user's only prompt.
		tracing::warn!(
			prompt_tokens = usage.prompt_tokens,
			context_window = context,
			"compaction threshold crossed but no compactable prefix; passing through"
		);
		return None;
	};

	let older = &messages[1..cutoff];
	if older.is_empty() {
		return None;
	}
	let messages_compacted = older.len() as u32;

	emit(
		sink,
		subagent_id_for_wrap,
		CoderEvent::CompactionStarted { messages_compacted },
	);

	let summary_call = vec![
		ChatMessage::System {
			content: SUMMARY_SYSTEM_PROMPT.to_string(),
		},
		ChatMessage::user(render_prefix_for_summary(older)),
	];
	let response = match inference
		.chat_completion(models.cheap(), &summary_call, &[], cancel)
		.await
	{
		Ok(r) => r,
		Err(err) => {
			tracing::warn!(error = %err, "compaction summary call failed; passing through uncompacted");
			// Fire a synthetic Complete with empty summary so the
			// frontend's "compacting…" pip clears. Otherwise the
			// UI would be stuck waiting on a Complete that never
			// arrives.
			emit(
				sink,
				subagent_id_for_wrap,
				CoderEvent::CompactionComplete {
					summary: String::new(),
					prompt_tokens_after: usage.prompt_tokens,
				},
			);
			return None;
		}
	};
	let summary = response.content.clone().unwrap_or_default();
	if summary.trim().is_empty() {
		tracing::warn!("compaction summary came back empty; passing through uncompacted");
		emit(
			sink,
			subagent_id_for_wrap,
			CoderEvent::CompactionComplete {
				summary: String::new(),
				prompt_tokens_after: usage.prompt_tokens,
			},
		);
		return None;
	}

	apply_summary_to_messages(messages, cutoff, &summary);

	let prompt_tokens_after = estimate_prompt_tokens(messages);
	tracing::info!(
		messages_compacted,
		prompt_tokens_before = usage.prompt_tokens,
		prompt_tokens_after,
		"auto-compaction applied"
	);
	emit(
		sink,
		subagent_id_for_wrap,
		CoderEvent::CompactionComplete {
			summary: summary.clone(),
			prompt_tokens_after,
		},
	);
	Some(CompactionApplied {
		summary,
		messages_compacted,
	})
}

/// Replace `messages[1..cutoff]` with one synthetic system
/// message holding the compaction summary. Shared between live
/// compaction and replay so both paths produce the byte-identical
/// in-memory `messages` shape — without that, "what the model
/// sees after a compaction" would diverge between a live run and
/// the same session reopened from disk.
pub(crate) fn apply_summary_to_messages(messages: &mut Vec<ChatMessage>, cutoff: usize, summary: &str) {
	messages.drain(1..cutoff);
	messages.insert(
		1,
		ChatMessage::System {
			content: format!("{COMPACTION_HEADER}{summary}"),
		},
	);
}

/// Return the index of the K-th most recent user message
/// (counting backwards from the end). Returns `None` when there
/// aren't enough user messages in the prefix to keep — i.e. the
/// session is too short to compact usefully.
///
/// `messages[0]` is always the composed system prompt and is
/// excluded from the search. The cutoff index points at the user
/// message we want to keep; everything in `messages[1..cutoff]`
/// is older history and gets folded.
fn find_cutoff_index(messages: &[ChatMessage]) -> Option<usize> {
	let mut user_seen = 0;
	for (i, msg) in messages.iter().enumerate().rev() {
		if i == 0 {
			break;
		}
		if matches!(msg, ChatMessage::User { .. }) {
			user_seen += 1;
			if user_seen >= RECENT_USER_TURNS_KEPT {
				if i > 1 {
					return Some(i);
				}
				return None;
			}
		}
	}
	None
}

/// Flatten the older message slice into a single prompt the
/// summarising fast-model call can ingest. Each message is
/// labelled with its role so the model can distinguish "the
/// user said …" from "the assistant said …" from "tool result
/// …". Tool results are included verbatim — they're often the
/// load-bearing artefact of the conversation (the file contents
/// the agent actually read), and dropping them on the floor would
/// make the summary fictionalise.
fn render_prefix_for_summary(messages: &[ChatMessage]) -> String {
	let mut out = String::new();
	out.push_str(
		"Below is the prefix of an in-flight coding session that needs to be summarised. \
Each block is one message; roles are explicit. The first message is the start of the session you should summarise.\n\n",
	);
	for msg in messages {
		match msg {
			ChatMessage::System { content } => {
				out.push_str("### system\n");
				out.push_str(content);
				out.push_str("\n\n");
			}
			ChatMessage::User { content, images } => {
				out.push_str("### user\n");
				out.push_str(content);
				if !images.is_empty() {
					// Note image presence so the summary doesn't
					// claim the user "didn't show me anything"
					// when there were screenshots in the prefix.
					// We can't usefully describe the pixels here
					// (the cheap summary model never saw them),
					// so a count is the honest minimum.
					out.push_str(&format!("\n[{} attached image(s)]", images.len()));
				}
				out.push_str("\n\n");
			}
			ChatMessage::Assistant { content, tool_calls } => {
				out.push_str("### assistant\n");
				if let Some(text) = content {
					out.push_str(text);
					out.push('\n');
				}
				for call in tool_calls {
					out.push_str(&format!(
						"[tool call: {} args={}]\n",
						call.function.name, call.function.arguments
					));
				}
				out.push('\n');
			}
			ChatMessage::Tool { tool_call_id, content } => {
				out.push_str(&format!("### tool ({tool_call_id})\n"));
				out.push_str(content);
				out.push_str("\n\n");
			}
		}
	}
	out
}

fn emit(sink: &FolderEventSink, subagent_id_for_wrap: Option<&str>, inner: CoderEvent) {
	match subagent_id_for_wrap {
		Some(id) => sink.send(CoderEvent::SubagentEvent {
			subagent_id: id.to_string(),
			inner: Box::new(inner),
		}),
		None => sink.send(inner),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::inference::{ChatMessage, FunctionCall, ToolCall};

	fn user(t: &str) -> ChatMessage {
		ChatMessage::user(t)
	}
	fn assistant(t: &str) -> ChatMessage {
		ChatMessage::Assistant {
			content: Some(t.into()),
			tool_calls: vec![],
		}
	}
	fn assistant_with_tool(text: &str, tool: &str) -> ChatMessage {
		ChatMessage::Assistant {
			content: Some(text.into()),
			tool_calls: vec![ToolCall {
				id: "call_1".into(),
				kind: "function".into(),
				function: FunctionCall {
					name: tool.into(),
					arguments: "{}".into(),
				},
			}],
		}
	}
	fn tool(t: &str) -> ChatMessage {
		ChatMessage::Tool {
			tool_call_id: "call_1".into(),
			content: t.into(),
		}
	}
	fn system(t: &str) -> ChatMessage {
		ChatMessage::System { content: t.into() }
	}

	#[test]
	fn cutoff_returns_none_when_history_too_short() {
		let mut msgs = vec![system("S"), user("u1"), assistant("a1"), user("u2")];
		assert!(find_cutoff_index(&msgs).is_none());
		// Sanity: even after appending another assistant, still
		// only two users — way below K=6.
		msgs.push(assistant("a2"));
		assert!(find_cutoff_index(&msgs).is_none());
	}

	#[test]
	fn cutoff_lands_on_user_message_when_enough_history() {
		let mut msgs = vec![system("S")];
		for i in 0..10 {
			msgs.push(user(&format!("u{i}")));
			msgs.push(assistant(&format!("a{i}")));
		}
		// 10 users; K=6, so we should keep the last 6 (u4..u9)
		// and the cutoff index points at u4.
		let cutoff = find_cutoff_index(&msgs).expect("cutoff");
		match &msgs[cutoff] {
			ChatMessage::User { content, .. } => assert_eq!(content, "u4"),
			other => panic!("cutoff did not land on a user message: {other:?}"),
		}
	}

	#[test]
	fn cutoff_keeps_assistant_tool_pairs_intact() {
		// Assistant calls a tool, tool replies, then a new user
		// turn comes in. The cutoff must land on the user, never
		// in the middle of an assistant/tool pair.
		let mut msgs = vec![system("S")];
		for i in 0..8 {
			msgs.push(user(&format!("u{i}")));
			msgs.push(assistant_with_tool(&format!("a{i}"), "read_file"));
			msgs.push(tool(&format!("contents{i}")));
		}
		let cutoff = find_cutoff_index(&msgs).expect("cutoff");
		assert!(matches!(&msgs[cutoff], ChatMessage::User { .. }));
		// Everything after cutoff must start with User. Tool
		// messages later in the slice are fine because their
		// parent assistant rides with them.
		assert!(matches!(msgs[cutoff], ChatMessage::User { .. }));
	}

	#[test]
	fn replay_cutoff_at_len_drops_everything_since_system_prompt() {
		// Replay-time shape: rebuild messages from JSONL records
		// up through the Compaction record, then call
		// `apply_summary_to_messages` with `cutoff = messages.len()`.
		// Result must be exactly `[system_prompt, summary]` —
		// reopening a compacted session shouldn't carry the
		// pre-compaction transcript along.
		let mut msgs = vec![system("S"), user("u0"), assistant("a0"), user("u1"), assistant("a1")];
		let cutoff = msgs.len();
		apply_summary_to_messages(&mut msgs, cutoff, "earlier turns: did stuff");
		assert_eq!(msgs.len(), 2);
		match &msgs[0] {
			ChatMessage::System { content } => assert_eq!(content, "S"),
			other => panic!("expected original system prompt at index 0, got {other:?}"),
		}
		match &msgs[1] {
			ChatMessage::System { content } => {
				assert!(
					content.contains("earlier turns: did stuff"),
					"summary system message should contain the supplied summary, got: {content}"
				);
			}
			other => panic!("expected summary system message at index 1, got {other:?}"),
		}
	}

	#[test]
	fn live_apply_then_replay_apply_yield_same_shape() {
		// Two paths reach the same in-memory `messages`:
		//   1. live: drain `[1..cutoff]`, insert summary,
		//      append the post-cutoff tail (already there).
		//   2. replay: rebuild every record into messages,
		//      then drain `[1..len]` on the Compaction record,
		//      then keep appending newer records.
		// Both must produce byte-identical output for the same
		// summary; otherwise the prompt diverges between a live
		// session and the same session reopened.
		let live = {
			let mut m = vec![
				system("S"),
				user("u0"),
				assistant("a0"),
				user("u1"),
				assistant("a1"),
				user("u2"),
				assistant("a2"),
			];
			apply_summary_to_messages(&mut m, 5, "summary text");
			m
		};
		let replay = {
			let mut m = vec![system("S"), user("u0"), assistant("a0"), user("u1"), assistant("a1")];
			let cutoff = m.len();
			apply_summary_to_messages(&mut m, cutoff, "summary text");
			m.push(user("u2"));
			m.push(assistant("a2"));
			m
		};
		assert_eq!(live.len(), replay.len());
		for (i, (a, b)) in live.iter().zip(replay.iter()).enumerate() {
			let left = serde_json::to_string(a).unwrap();
			let right = serde_json::to_string(b).unwrap();
			assert_eq!(left, right, "mismatch at index {i}: live={left} replay={right}");
		}
	}
}
