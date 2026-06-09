//! `ask_user` — bidirectional prompts the agent raises mid-turn.
//!
//! Unlike every other tool, `ask_user` doesn't compute a result
//! from the workspace: it pauses the turn and waits for the human.
//! The runner emits a normal `tool_call` event (so the panel renders
//! a card), parks a [`oneshot::Sender`] on the session's
//! [`PromptRegistry`], and `await`s the matching receiver. The
//! human resolves it one of two ways:
//!
//! - **Answer the card** — the panel calls `coder_respond_to_prompt`
//!   with their per-question choices, which sends [`PromptOutcome::Answered`].
//! - **Skip and keep typing** — the human ignores the card and just
//!   sends a normal composer message. `Coder::send` notices a prompt
//!   is parked, resolves it with [`PromptOutcome::Skipped`], and the
//!   typed message proceeds as a regular steer/continuation.
//!
//! Abort (Esc / panel close / sign-out) cancels the turn token; the
//! tool's `select!` wakes on cancellation and returns
//! [`CoderError::Aborted`], so there's no half-state for the model
//! to puzzle over.
//!
//! See `specs/coder.md` § Ask user tool.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{oneshot, Mutex};

use crate::inference::ToolDefinition;

/// `ask_user` tool definition. Lives outside the `ToolRegistry`
/// (like [`crate::subagent::task_tool_definition`]) so it's only
/// appended to the **parent** turn's tool list — sub-agents never
/// see it, since pausing for the human only makes sense at the top
/// level where there's a panel + composer to answer through.
pub fn ask_user_tool_definition() -> ToolDefinition {
	ToolDefinition::function(
		"ask_user",
		"Pause and ask the user one or more multiple-choice questions, then wait for their answer before continuing. \
\
You can ask several questions at once; each gets its own set of options. The user is **always** able to type a custom free-form answer instead of (or in addition to) picking an option, and they can also choose to skip the questions entirely and just keep typing in the composer — in which case you'll get a `skipped` result and should continue with whatever they say next. Phrase questions so the listed options cover the common cases but a custom answer still makes sense.",
		json!({
			"type": "object",
			"properties": {
				"questions": {
					"type": "array",
					"description": "One or more questions to ask. Keep it short — usually one or two questions; more than ~4 is overwhelming.",
					"items": {
						"type": "object",
						"properties": {
							"id": {
								"type": "string",
								"description": "Stable identifier you assign so you can match the answer back to the question. Required and must be unique within the call."
							},
							"question": {
								"type": "string",
								"description": "The question text shown to the user."
							},
							"options": {
								"type": "array",
								"description": "Preset answers the user can click. Provide at least 2. The user can always also type a custom answer, so you don't need an explicit \"Other\" option.",
								"items": {
									"type": "object",
									"properties": {
										"id": {
											"type": "string",
											"description": "Stable identifier for this option, unique within the question. Returned in the answer's `selected` list."
										},
										"label": {
											"type": "string",
											"description": "Human-readable button text."
										}
									},
									"required": ["id", "label"]
								}
							},
							"allow_multiple": {
								"type": "boolean",
								"description": "When true, the user can pick several options for this question (checkboxes + a confirm button). Default false (single-select, submits on click).",
								"default": false
							}
						},
						"required": ["id", "question", "options"]
					}
				}
			},
			"required": ["questions"]
		}),
	)
}

/// What the human chose for one question in an `ask_user` prompt.
/// `selected` is the list of option ids they clicked (one for a
/// single-select question, possibly several for multi-select);
/// `free_text` carries a custom "Other…" answer the user typed.
/// Both can be present — a user can tick a preset option and also
/// type extra context. At least one of the two is non-empty for an
/// answered question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswer {
	/// Question id this answer belongs to, matching the `id` the
	/// agent supplied in the tool args.
	pub question_id: String,
	/// Option ids the user selected. Empty when they only typed a
	/// custom answer.
	#[serde(default)]
	pub selected: Vec<String>,
	/// Free-form "Other…" answer. Empty when the user only clicked
	/// preset options.
	#[serde(default)]
	pub free_text: String,
}

/// The structured response the panel sends back via
/// `coder_respond_to_prompt`. One [`QuestionAnswer`] per question the
/// user actually answered — unanswered questions (the user left a
/// question blank) simply don't appear, which the tool result spells
/// out so the model can decide whether the gap matters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
	pub answers: Vec<QuestionAnswer>,
}

/// How a parked prompt got resolved.
#[derive(Debug, Clone)]
pub enum PromptOutcome {
	/// The user answered the card. Carries their structured choices.
	Answered(PromptResponse),
	/// The user ignored the card and sent a normal composer message
	/// instead — they chose to skip the questions and keep driving.
	/// The tool returns a "skipped" result and the typed message
	/// proceeds as a steer.
	Skipped,
}

/// Per-session registry of in-flight `ask_user` prompts.
///
/// Only one prompt can be parked per session at a time: the loop is
/// single-turn-at-a-time and `ask_user` blocks the turn until it
/// resolves. We still key by `tool_call_id` so a stale resolve
/// (the panel double-clicks, or a response races the abort) targets
/// the exact call and a mismatched id is a no-op rather than
/// resolving the wrong prompt.
#[derive(Default)]
pub struct PromptRegistry {
	pending: Mutex<HashMap<String, oneshot::Sender<PromptOutcome>>>,
}

impl PromptRegistry {
	/// Park a sender under `tool_call_id` and hand back the matching
	/// receiver for the tool to `await`. Replaces any existing entry
	/// for the same id (shouldn't happen — one prompt at a time —
	/// but keeps the map self-healing if it does).
	pub async fn register(&self, tool_call_id: impl Into<String>) -> oneshot::Receiver<PromptOutcome> {
		let (tx, rx) = oneshot::channel();
		self.pending.lock().await.insert(tool_call_id.into(), tx);
		rx
	}

	/// Resolve the prompt registered under `tool_call_id`. Returns
	/// `true` when a matching parked sender was found and fired,
	/// `false` when there was nothing to resolve (already answered,
	/// aborted, or unknown id).
	pub async fn resolve(&self, tool_call_id: &str, outcome: PromptOutcome) -> bool {
		let Some(tx) = self.pending.lock().await.remove(tool_call_id) else {
			return false;
		};
		// Receiver dropped (turn aborted out from under us) is fine —
		// the tool already woke on cancellation in that case.
		tx.send(outcome).is_ok()
	}

	/// Resolve **any** single parked prompt with [`PromptOutcome::Skipped`].
	/// Used by `Coder::send`'s skip path, where the caller has the
	/// session but not the specific tool-call id. Returns `true`
	/// when a prompt was parked and got skipped.
	pub async fn skip_any(&self) -> bool {
		let mut pending = self.pending.lock().await;
		let Some(id) = pending.keys().next().cloned() else {
			return false;
		};
		let tx = pending.remove(&id).expect("key just observed");
		tx.send(PromptOutcome::Skipped).is_ok()
	}

	/// `true` when at least one prompt is currently parked. Cheap
	/// probe used by `send` to decide between the skip path and the
	/// normal steer path.
	pub async fn has_pending(&self) -> bool {
		!self.pending.lock().await.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn resolve_fires_the_parked_receiver_once() {
		let reg = PromptRegistry::default();
		let rx = reg.register("call-1").await;
		assert!(reg.has_pending().await);
		assert!(
			reg
				.resolve(
					"call-1",
					PromptOutcome::Answered(PromptResponse {
						answers: vec![QuestionAnswer {
							question_id: "q1".into(),
							selected: vec!["a".into()],
							free_text: String::new(),
						}],
					}),
				)
				.await
		);
		assert!(!reg.has_pending().await);
		match rx.await {
			Ok(PromptOutcome::Answered(r)) => assert_eq!(r.answers[0].question_id, "q1"),
			other => panic!("expected answered, got {other:?}"),
		}
		// A second resolve for the same id is a no-op.
		assert!(!reg.resolve("call-1", PromptOutcome::Skipped).await);
	}

	#[tokio::test]
	async fn skip_any_resolves_the_only_parked_prompt() {
		let reg = PromptRegistry::default();
		let rx = reg.register("call-7").await;
		assert!(reg.skip_any().await);
		assert!(!reg.has_pending().await);
		assert!(matches!(rx.await, Ok(PromptOutcome::Skipped)));
		// Nothing left to skip.
		assert!(!reg.skip_any().await);
	}

	#[tokio::test]
	async fn resolve_unknown_id_is_a_noop() {
		let reg = PromptRegistry::default();
		let _rx = reg.register("call-real").await;
		assert!(!reg.resolve("call-bogus", PromptOutcome::Skipped).await);
		assert!(reg.has_pending().await);
	}
}
