//! Telling the model which terminals are open, through the
//! conversation instead of the tool descriptions.
//!
//! The list rides in the user's prompt as an `<open_terminals>`
//! element of its trailing `<context>` block (the envelope the
//! composer already uses for code selections and terminal pastes, and
//! the one the panel hides), and only when it differs from the last
//! one this conversation carries. Stored with the message, it never
//! changes again, so the request prefix stays byte-stable: the prompt
//! cache keeps hitting and preserved thinking stays valid (ADR 0110).
//!
//! "Last announced" is read back from the messages rather than kept
//! as session state, so a reloaded session doesn't repeat itself and
//! a compacted one, whose announcement was summarised away, gets a
//! fresh one.

use crate::inference::ChatMessage;

const OPEN: &str = "<open_terminals>\n";
const CLOSE: &str = "\n</open_terminals>";
const CONTEXT_OPEN: &str = "<context>";
const CONTEXT_CLOSE: &str = "</context>";

/// Body of the block when the project has no terminal open. Also
/// what a conversation with no block at all is taken to have said.
const NONE: &str = "(none)";

/// `prompt` with an `<open_terminals>` block appended, or `None` when
/// `inventory` (the project's open terminals, `None` for none) is
/// what `messages` already says.
pub(crate) fn announce(prompt: &str, messages: &[ChatMessage], inventory: Option<&str>) -> Option<String> {
	let current = inventory.unwrap_or(NONE);
	if last_announced(messages).unwrap_or(NONE) == current {
		return None;
	}
	Some(append_context_element(prompt, &format!("{OPEN}{current}{CLOSE}")))
}

fn last_announced(messages: &[ChatMessage]) -> Option<&str> {
	messages.iter().rev().find_map(|message| {
		let ChatMessage::User { content, .. } = message else {
			return None;
		};
		let start = content.rfind(OPEN)? + OPEN.len();
		let len = content[start..].find(CLOSE)?;
		Some(&content[start..start + len])
	})
}

/// Add `element` to the prompt's trailing `<context>` block, creating
/// the block when the prompt has none. Same shape as the composer's
/// `renderPromptWithAttachments`.
fn append_context_element(prompt: &str, element: &str) -> String {
	let trimmed = prompt.trim_end();
	if let Some(head) = trimmed.strip_suffix(CONTEXT_CLOSE) {
		if head.contains(CONTEXT_OPEN) {
			return format!("{head}{element}\n{CONTEXT_CLOSE}");
		}
	}
	if trimmed.is_empty() {
		return format!("{CONTEXT_OPEN}\n{element}\n{CONTEXT_CLOSE}");
	}
	format!("{trimmed}\n\n{CONTEXT_OPEN}\n{element}\n{CONTEXT_CLOSE}")
}

#[cfg(test)]
mod tests {
	use super::*;

	const DEV: &str = "- `t1` — container shell in `/workspace/proj` (running)";
	const DEV_EXITED: &str = "- `t1` — container shell in `/workspace/proj` (exited 1)";

	fn history(prompts: &[&str]) -> Vec<ChatMessage> {
		prompts.iter().map(|prompt| ChatMessage::user(*prompt)).collect()
	}

	#[test]
	fn nothing_is_said_while_no_terminal_was_ever_open() {
		assert_eq!(announce("hi", &[], None), None);
		assert_eq!(announce("again", &history(&["hi"]), None), None);
	}

	#[test]
	fn the_list_is_announced_once_and_again_only_when_it_changes() {
		let first = announce("start the tests", &[], Some(DEV)).expect("first sight of a terminal");
		assert_eq!(
			first,
			format!("start the tests\n\n<context>\n<open_terminals>\n{DEV}\n</open_terminals>\n</context>")
		);

		let messages = history(&[&first, "unrelated follow-up"]);
		assert_eq!(announce("and now?", &messages, Some(DEV)), None);

		let changed = announce("and now?", &messages, Some(DEV_EXITED)).expect("state changed");
		assert!(changed.contains(DEV_EXITED));
	}

	#[test]
	fn closing_the_last_terminal_is_announced() {
		let first = announce("hi", &[], Some(DEV)).unwrap();
		let messages = history(&[&first]);
		let closed = announce("next", &messages, None).expect("the list emptied");
		assert!(closed.ends_with("<context>\n<open_terminals>\n(none)\n</open_terminals>\n</context>"));

		let messages = history(&[&first, &closed]);
		assert_eq!(announce("more", &messages, None), None);
	}

	#[test]
	fn the_block_joins_the_composers_context_block() {
		let prompt =
			"explain\n\n<context>\n<code_selection path=\"a.rs\" lines=\"1-2\">\nfn a() {}\n</code_selection>\n</context>";
		let out = announce(prompt, &[], Some(DEV)).unwrap();
		assert_eq!(out.matches("<context>").count(), 1);
		assert!(out.ends_with(&format!(
			"</code_selection>\n<open_terminals>\n{DEV}\n</open_terminals>\n</context>"
		)));
	}

	#[test]
	fn a_prompt_with_no_prose_is_just_the_block() {
		let out = announce("", &[], Some(DEV)).unwrap();
		assert!(out.starts_with("<context>\n<open_terminals>"));
	}
}
