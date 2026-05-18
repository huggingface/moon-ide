//! Per-session todo list maintained by the agent via the
//! `todo_write` tool.
//!
//! Owned by [`crate::runner::Session`]: the agent's plan for the
//! current task lives here, the runner short-circuits the
//! `todo_write` dispatch (instead of sending it through
//! [`crate::tools::ToolRegistry`]) to mutate it, and the canonical
//! list goes back to the model as the tool's result so the loop
//! and the UI agree on state every turn. Persistence rides on
//! [`crate::sessions::SessionRecord::TodosUpdate`] — one record per
//! call, replay-last-wins on session reopen.
//!
//! No enforcement of "only one item in `in_progress` at a time" or
//! similar shape rules: the system prompt asks the model to behave,
//! mirroring Cursor / pi-mono. Mechanical enforcement would turn
//! benign two-flip races into errors for no benefit.

use serde::{Deserialize, Serialize};

/// One entry in the session's todo list.
///
/// `id` is opaque to us — the agent assigns whatever it wants
/// (numeric strings, slugs, uuids) and addresses items by it on
/// follow-up `todo_write` calls. We never look at the contents
/// beyond using it as a map key, so any non-empty `String` is
/// valid; empty strings are rejected at the tool boundary because
/// they'd collapse multiple distinct items into one merge target.
///
/// `status` defaults to `Pending` when the model omits it. The
/// schema still lists it under `properties` (so the model knows
/// the field exists) and the system prompt still encourages
/// explicit status; the default exists solely to swallow the
/// recurrent "model forgets `status` on a freshly-added item"
/// case instead of bouncing it as `CoderError::invalid_args`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
	pub id: String,
	pub content: String,
	#[serde(default)]
	pub status: TodoStatus,
}

/// Status vocabulary mirrors Cursor's `TodoWrite`. `cancelled`
/// covers "decided not to do this after all" so the agent can
/// retire an item without faking a `completed`. The wire form is
/// snake_case (`in_progress`) so prompts written for Cursor /
/// pi-mono carry over verbatim.
///
/// `Default` is `Pending` — the natural state of a freshly-added
/// item — so [`TodoItem`]'s `#[serde(default)]` on `status` falls
/// back to it when the model omits the field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
	#[default]
	Pending,
	InProgress,
	Completed,
	Cancelled,
}

/// Apply an incoming `todo_write` payload to `current` and return
/// the canonical list the model should see as the tool result.
///
/// Two modes, picked by the call's `merge` flag:
///
/// - `merge = false` (default): `incoming` replaces the list
///   wholesale. This is how the agent starts a fresh plan or
///   wipes a stale one (`incoming = []` clears).
/// - `merge = true`: items are matched by `id` and updated in
///   place; ids in `incoming` that aren't present in `current`
///   are appended. Items in `current` that aren't mentioned in
///   `incoming` are kept untouched — there's no "implicit delete"
///   in merge mode. Order in the output list is `current`-order
///   first, then any newly-appended ids in `incoming`-order.
///
/// The function is pure and never errors — input validation
/// (empty ids, duplicate ids) lives at the tool boundary so the
/// model gets a structured `CoderError::invalid_args` response;
/// this helper is a small piece of plumbing the runner and the
/// unit tests both call.
pub fn merge_todos(current: &[TodoItem], incoming: Vec<TodoItem>, merge: bool) -> Vec<TodoItem> {
	if !merge {
		return incoming;
	}
	let mut out: Vec<TodoItem> = current.to_vec();
	for item in incoming {
		if let Some(existing) = out.iter_mut().find(|t| t.id == item.id) {
			existing.content = item.content;
			existing.status = item.status;
		} else {
			out.push(item);
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn item(id: &str, content: &str, status: TodoStatus) -> TodoItem {
		TodoItem {
			id: id.to_string(),
			content: content.to_string(),
			status,
		}
	}

	/// `merge=false` is wholesale replacement — the entire prior
	/// list is discarded regardless of whether ids overlap.
	#[test]
	fn replace_drops_prior_items() {
		let current = vec![
			item("1", "old", TodoStatus::InProgress),
			item("2", "stale", TodoStatus::Pending),
		];
		let incoming = vec![item("3", "fresh", TodoStatus::Pending)];
		let after = merge_todos(&current, incoming, false);
		assert_eq!(after, vec![item("3", "fresh", TodoStatus::Pending)]);
	}

	/// `merge=false` with an empty payload is the documented way
	/// to clear the list without starting a new session.
	#[test]
	fn replace_with_empty_clears_the_list() {
		let current = vec![item("1", "leftover", TodoStatus::Completed)];
		let after = merge_todos(&current, vec![], false);
		assert!(after.is_empty());
	}

	/// `merge=true` updates existing ids in place — content and
	/// status both flip, prior order is preserved.
	#[test]
	fn merge_updates_existing_ids_in_place() {
		let current = vec![
			item("a", "first", TodoStatus::Pending),
			item("b", "second", TodoStatus::InProgress),
		];
		let incoming = vec![item("b", "second updated", TodoStatus::Completed)];
		let after = merge_todos(&current, incoming, true);
		assert_eq!(
			after,
			vec![
				item("a", "first", TodoStatus::Pending),
				item("b", "second updated", TodoStatus::Completed),
			]
		);
	}

	/// `merge=true` with an unknown id appends to the end. Order:
	/// existing ids stay first (in their prior positions), new ids
	/// follow in the incoming order.
	#[test]
	fn merge_appends_unknown_ids() {
		let current = vec![item("a", "first", TodoStatus::Pending)];
		let incoming = vec![
			item("b", "second", TodoStatus::Pending),
			item("c", "third", TodoStatus::Pending),
		];
		let after = merge_todos(&current, incoming, true);
		assert_eq!(
			after,
			vec![
				item("a", "first", TodoStatus::Pending),
				item("b", "second", TodoStatus::Pending),
				item("c", "third", TodoStatus::Pending),
			]
		);
	}

	/// Items not mentioned in a `merge=true` payload are kept
	/// untouched. There's no "implicit delete" — the agent has to
	/// either `merge=false` with the new shape or transition the
	/// item to `Cancelled` to retire it.
	#[test]
	fn merge_leaves_unmentioned_items_alone() {
		let current = vec![
			item("a", "first", TodoStatus::InProgress),
			item("b", "second", TodoStatus::Pending),
		];
		let incoming = vec![item("a", "first done", TodoStatus::Completed)];
		let after = merge_todos(&current, incoming, true);
		assert_eq!(
			after,
			vec![
				item("a", "first done", TodoStatus::Completed),
				item("b", "second", TodoStatus::Pending),
			]
		);
	}

	/// `merge=true` with an empty incoming list is a no-op (the
	/// "clear" path goes through `merge=false`). Important: this
	/// stops the agent from accidentally wiping its plan with a
	/// stray empty merge call.
	#[test]
	fn merge_empty_is_noop() {
		let current = vec![item("a", "first", TodoStatus::InProgress)];
		let after = merge_todos(&current, vec![], true);
		assert_eq!(after, current);
	}

	/// Status round-trips through serde with the snake_case wire
	/// form the model emits (`in_progress`, not `InProgress`).
	#[test]
	fn status_serializes_snake_case() {
		assert_eq!(
			serde_json::to_string(&TodoStatus::InProgress).unwrap(),
			"\"in_progress\""
		);
		assert_eq!(
			serde_json::from_str::<TodoStatus>("\"in_progress\"").unwrap(),
			TodoStatus::InProgress
		);
	}

	/// Models occasionally omit `status` when adding a fresh item
	/// (intent is always "pending — I just added it"). The field
	/// is now `#[serde(default)]` so the parse succeeds with the
	/// natural default rather than bouncing the call back as an
	/// `invalid_args` error the model has to retry past.
	#[test]
	fn item_defaults_status_to_pending_when_missing() {
		let json = r#"{"id":"a","content":"new task"}"#;
		let parsed: TodoItem = serde_json::from_str(json).unwrap();
		assert_eq!(
			parsed,
			TodoItem {
				id: "a".into(),
				content: "new task".into(),
				status: TodoStatus::Pending,
			}
		);
	}

	#[test]
	fn item_keeps_explicit_status_when_present() {
		let json = r#"{"id":"a","content":"working","status":"in_progress"}"#;
		let parsed: TodoItem = serde_json::from_str(json).unwrap();
		assert_eq!(parsed.status, TodoStatus::InProgress);
	}
}
