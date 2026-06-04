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
	/// How many trailing messages rode through the fold unchanged
	/// (`messages[cutoff..]` at the time of compaction). Persisted
	/// into the `Compaction` record so replay can reproduce the
	/// same cutoff — folding everything *except* the last
	/// `messages_kept` messages — instead of draining the whole
	/// prefix and dropping the recent turns we deliberately kept.
	pub messages_kept: u32,
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

	let summary = match summarise_prefix(inference, models, older, cancel).await {
		Some(s) if !s.trim().is_empty() => s,
		_ => {
			// Either every summary call failed, or the model came
			// back empty. Fire a synthetic Complete with an empty
			// summary so the frontend's "compacting…" pip clears,
			// and pass through uncompacted — the loop tries again
			// next turn.
			tracing::warn!("compaction summary unavailable; passing through uncompacted");
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

	// Captured before the drain: everything from `cutoff` to the
	// end rides through unchanged. Replay reproduces the cutoff
	// from this count rather than re-deriving it (the K-user-turn
	// heuristic could land differently if the constant changes
	// between the write and the reopen).
	let messages_kept = (messages.len() - cutoff) as u32;

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
		messages_kept,
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

/// Rough token estimate for a rendered string: bytes / 4, the
/// same conventional ratio the runner's `estimate_prompt_tokens`
/// uses. Good enough to keep each summary chunk under the cheap
/// model's window with margin to spare.
fn estimate_tokens(s: &str) -> usize {
	s.len() / 4
}

/// Fraction of the cheap model's context window a single summary
/// call's *input* is allowed to fill. The rest is headroom for
/// the system prompt, the model's own summary output (which can
/// run several thousand tokens), and estimate slop. Conservative
/// on purpose: a summary call that itself 400s is the exact bug
/// this chunking exists to avoid.
const SUMMARY_INPUT_BUDGET: f32 = 0.55;

/// Default budget (in estimated tokens) for one summary call when
/// the cheap model's window is unknown (catalog not fetched). Maps
/// to a conservative 128k-window model at [`SUMMARY_INPUT_BUDGET`].
const SUMMARY_FALLBACK_BUDGET_TOKENS: usize = 70_000;

/// Summarise the older message prefix into a single markdown
/// block, chunking the input so no individual call to the cheap
/// model exceeds its context window.
///
/// The earlier implementation rendered the entire prefix and sent
/// it in one shot. On a long, heavily-cached session that prefix
/// can be far larger than the cheap model's own window (e.g. a
/// 700k-token history summarised by a 200k-window model), so the
/// call 400'd every turn and compaction silently never ran — the
/// session just kept growing past the cap. We now:
///
/// 1. Pack the rendered messages into chunks that each fit the
///    cheap model's window (with headroom for the system prompt
///    and the summary output).
/// 2. Summarise each chunk independently.
/// 3. If there was more than one chunk, fold the partial
///    summaries together with a final pass (recursing if the
///    concatenated partials are themselves too big).
///
/// Returns `None` only when *every* call failed; a partial set of
/// successful chunk summaries still produces a usable result.
async fn summarise_prefix(
	inference: &InferenceClient,
	models: &CoderModels,
	older: &[ChatMessage],
	cancel: &CancellationToken,
) -> Option<String> {
	let window = models.context_window(models.cheap());
	let budget = if window == 0 {
		SUMMARY_FALLBACK_BUDGET_TOKENS
	} else {
		((window as f32 * SUMMARY_INPUT_BUDGET) as usize).max(4_000)
	};

	let rendered: Vec<String> = older.iter().map(render_message_for_summary).collect();
	let chunks = pack_into_chunks(&rendered, budget);
	if chunks.is_empty() {
		return None;
	}

	// Summarise each chunk. Tolerate per-chunk failures: a
	// partial summary is more useful than none.
	let mut partials: Vec<String> = Vec::new();
	let multi = chunks.len() > 1;
	for (i, chunk) in chunks.iter().enumerate() {
		let intro = if multi {
			format!(
				"{PREFIX_INTRO}(This is part {} of {} of the session prefix.)\n\n",
				i + 1,
				chunks.len()
			)
		} else {
			PREFIX_INTRO.to_string()
		};
		match summarise_once(inference, models, &format!("{intro}{chunk}"), cancel).await {
			Some(s) if !s.trim().is_empty() => partials.push(s),
			_ => tracing::warn!(chunk = i, "compaction chunk summary failed; skipping it"),
		}
	}
	if partials.is_empty() {
		return None;
	}
	if partials.len() == 1 {
		return partials.into_iter().next();
	}

	// Fold the partials. If the concatenation is itself too big
	// for one call, recurse — each level shrinks the input.
	let combined = partials.join("\n\n---\n\n");
	if estimate_tokens(&combined) <= budget {
		let prompt = format!(
			"The following are ordered partial summaries of consecutive slices of one coding session. \
Merge them into a single coherent summary, preserving chronology and de-duplicating overlap:\n\n{combined}"
		);
		return summarise_once(inference, models, &prompt, cancel)
			.await
			.or(Some(combined));
	}
	// Too big even combined: wrap each partial back into a
	// pseudo-message and recurse through the same chunker.
	let pseudo: Vec<ChatMessage> = partials.into_iter().map(ChatMessage::user).collect();
	Box::pin(summarise_prefix(inference, models, &pseudo, cancel)).await
}

/// Pack pre-rendered message blocks into chunks that each stay
/// under `budget` estimated tokens. A single block bigger than the
/// budget on its own (a huge tool result / pasted file) goes in
/// its own chunk, truncated to the budget so the summary call
/// can't 400 on it — losing the tail of one giant blob beats never
/// compacting at all. Order is preserved so the chunk summaries
/// stay chronological.
fn pack_into_chunks(blocks: &[String], budget: usize) -> Vec<String> {
	let mut chunks: Vec<String> = Vec::new();
	let mut current = String::new();
	for block in blocks {
		if estimate_tokens(block) > budget {
			if !current.is_empty() {
				chunks.push(std::mem::take(&mut current));
			}
			let max_bytes = budget.saturating_mul(4);
			let mut truncated = block.clone();
			if truncated.len() > max_bytes {
				truncated.truncate(floor_char_boundary(&truncated, max_bytes));
				truncated.push_str("\n[… message truncated for summarisation …]\n");
			}
			chunks.push(truncated);
			continue;
		}
		if estimate_tokens(&current) + estimate_tokens(block) > budget && !current.is_empty() {
			chunks.push(std::mem::take(&mut current));
		}
		current.push_str(block);
	}
	if !current.is_empty() {
		chunks.push(current);
	}
	chunks
}

/// One non-streaming summary call against the cheap model with the
/// fixed [`SUMMARY_SYSTEM_PROMPT`]. Returns `None` on transport
/// error so the caller can decide whether a partial result is
/// still usable.
async fn summarise_once(
	inference: &InferenceClient,
	models: &CoderModels,
	user_content: &str,
	cancel: &CancellationToken,
) -> Option<String> {
	let call = vec![
		ChatMessage::System {
			content: SUMMARY_SYSTEM_PROMPT.to_string(),
		},
		ChatMessage::user(user_content),
	];
	match inference.chat_completion(models.cheap(), &call, &[], cancel).await {
		Ok(r) => r.content,
		Err(err) => {
			tracing::warn!(error = %err, "compaction summary call failed");
			None
		}
	}
}

/// Largest char boundary `<= max` in `s`, so a truncation never
/// splits a UTF-8 sequence. (`str::floor_char_boundary` is still
/// unstable, so we inline the equivalent.)
fn floor_char_boundary(s: &str, max: usize) -> usize {
	if max >= s.len() {
		return s.len();
	}
	let mut i = max;
	while i > 0 && !s.is_char_boundary(i) {
		i -= 1;
	}
	i
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

/// Render one message into the role-labelled block the
/// summarising model ingests. Tool results are included verbatim
/// — they're often the load-bearing artefact of the conversation
/// (the file contents the agent actually read), and dropping them
/// on the floor would make the summary fictionalise.
fn render_message_for_summary(msg: &ChatMessage) -> String {
	let mut out = String::new();
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
				// Note image presence so the summary doesn't claim
				// the user "didn't show me anything" when there
				// were screenshots in the prefix. We can't usefully
				// describe the pixels here (the cheap summary model
				// never saw them), so a count is the honest minimum.
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
	out
}

/// Header that prefixes the first chunk of rendered prefix in a
/// summary call, so the model knows what it's looking at.
const PREFIX_INTRO: &str = "Below is (part of) the prefix of an in-flight coding session that needs to be summarised. \
Each block is one message; roles are explicit.\n\n";

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
	fn pack_into_chunks_splits_when_over_budget() {
		// Three ~equal blocks, budget fits ~2 of them. Expect the
		// packer to start a new chunk rather than overflow.
		let block = "x".repeat(400); // ~100 est tokens each
		let blocks = vec![block.clone(), block.clone(), block.clone()];
		let chunks = pack_into_chunks(&blocks, 150);
		assert!(chunks.len() >= 2, "expected a split, got {} chunk(s)", chunks.len());
		for c in &chunks {
			assert!(
				estimate_tokens(c) <= 150 || c.len() <= 600,
				"a chunk overflowed the budget: {} est tokens",
				estimate_tokens(c)
			);
		}
	}

	#[test]
	fn pack_into_chunks_truncates_oversized_single_block() {
		// One block far larger than the budget must still produce a
		// chunk (truncated) rather than be dropped or 400 later.
		let huge = "y".repeat(10_000); // ~2500 est tokens
		let chunks = pack_into_chunks(&[huge], 100);
		assert_eq!(chunks.len(), 1);
		assert!(chunks[0].contains("message truncated"), "expected truncation marker");
		assert!(estimate_tokens(&chunks[0]) <= 200, "truncated chunk still over budget");
	}

	#[test]
	fn pack_into_chunks_keeps_small_history_as_one() {
		let blocks = vec!["a".repeat(40), "b".repeat(40)];
		let chunks = pack_into_chunks(&blocks, 100_000);
		assert_eq!(chunks.len(), 1);
	}

	#[test]
	fn floor_char_boundary_never_splits_utf8() {
		let s = "héllo wörld";
		for max in 0..=s.len() {
			let b = floor_char_boundary(s, max);
			assert!(s.is_char_boundary(b), "byte {b} is not a char boundary");
			assert!(b <= max);
		}
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
	fn replay_keeps_trailing_messages_from_messages_kept() {
		// Replay-time shape mirroring the real on-disk order: the
		// kept recent turns sit BEFORE the Compaction record (they
		// were persisted as they happened, earlier in the run).
		// Replay rebuilds the full transcript including them, then
		// folds everything EXCEPT the last `messages_kept`. With
		// messages_kept = 2 here we keep the last user/assistant
		// pair, so the result is `[system, summary, u1, a1]`.
		let mut msgs = vec![system("S"), user("u0"), assistant("a0"), user("u1"), assistant("a1")];
		let messages_kept = 2usize;
		let cutoff = msgs.len().saturating_sub(messages_kept).max(1);
		apply_summary_to_messages(&mut msgs, cutoff, "earlier turns: did stuff");
		assert_eq!(msgs.len(), 4);
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
		// The last kept pair rides through untouched.
		assert!(matches!(&msgs[2], ChatMessage::User { content, .. } if content == "u1"));
		assert!(matches!(&msgs[3], ChatMessage::Assistant { content: Some(c), .. } if c == "a1"));
	}

	#[test]
	fn live_apply_then_replay_apply_yield_same_shape() {
		// Two paths must reach the same in-memory `messages`, with
		// the kept turns sitting in their REAL on-disk position —
		// BEFORE the Compaction record, because they were persisted
		// as they happened. Earlier this test cheated by pushing
		// the kept turns AFTER the compaction record; that masked a
		// divergence where replay dropped them. Now both paths fold
		// at the same cutoff (live via `find_cutoff_index`, replay
		// via `messages_kept`) and must be byte-identical.
		//
		// 8 user turns, K=6 → live keeps u2..u7 (12 messages).
		let live = {
			let mut m = vec![system("S")];
			for i in 0..8 {
				m.push(user(&format!("u{i}")));
				m.push(assistant(&format!("a{i}")));
			}
			let cutoff = find_cutoff_index(&m).expect("cutoff");
			apply_summary_to_messages(&mut m, cutoff, "summary text");
			m
		};
		// Replay rebuilds the same full transcript (the kept turns
		// are already on disk before the Compaction record), then
		// folds everything except `messages_kept = 12`.
		let replay = {
			let mut m = vec![system("S")];
			for i in 0..8 {
				m.push(user(&format!("u{i}")));
				m.push(assistant(&format!("a{i}")));
			}
			let messages_kept = 12usize;
			let cutoff = m.len().saturating_sub(messages_kept).max(1);
			apply_summary_to_messages(&mut m, cutoff, "summary text");
			m
		};
		assert_eq!(live.len(), replay.len(), "live and replay diverged in length");
		for (i, (a, b)) in live.iter().zip(replay.iter()).enumerate() {
			let left = serde_json::to_string(a).unwrap();
			let right = serde_json::to_string(b).unwrap();
			assert_eq!(left, right, "mismatch at index {i}: live={left} replay={right}");
		}
	}
}
