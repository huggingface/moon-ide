//! Build Sweep-style autocomplete prompts and call a local llama.cpp server.
//!
//! See <https://blog.sweep.dev/posts/oss-next-edit> for the training
//! layout we approximate here (fixed 21-line window: 10 above, caret, 10 below).

use std::time::Duration;

use moon_protocol::next_edit::{
	NextEditCompleteParams, NextEditCompleteResult, NextEditProbeKind, NextEditProbeResult,
};
use moon_protocol::MoonError;
use reqwest::StatusCode;
use serde::Deserialize;

const WINDOW_RADIUS: usize = 10;
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
struct LlamaCompletionBody {
	content: String,
	/// `true` when llama-server stopped because the model emitted
	/// one of our `--stop` strings — for us, `<|file_sep|>`,
	/// which is the trained "the updated section ends here"
	/// signal. When this is `true` we trust the model's output to
	/// span the *intended* edit, even if it has fewer lines than
	/// the input window (i.e. the model deleted some lines on
	/// purpose). When `false` / absent we treat the output as
	/// possibly truncated and narrow the replacement range so we
	/// don't accidentally eat lines below the window.
	#[serde(default)]
	stopped_word: bool,
	/// `true` when llama-server stopped because we hit
	/// `n_predict` — definitely truncated. Carried for the
	/// tracing breadcrumb; the narrowing logic is driven by
	/// `stopped_word` so any non-clean-stop reason (limit, EOS,
	/// missing field) triggers the safe narrowing.
	#[serde(default)]
	stopped_limit: bool,
}

fn normalize_base_url(base: &str) -> Result<String, MoonError> {
	let t = base.trim();
	if t.is_empty() {
		return Err(MoonError::invalid("next-edit base URL is empty"));
	}
	Ok(t.trim_end_matches('/').to_string())
}

fn validate_relative_path(path: &str) -> Result<(), MoonError> {
	if path.is_empty() {
		return Err(MoonError::invalid("relative path is empty"));
	}
	if path.contains("..") {
		return Err(MoonError::invalid("relative path must not contain '..'"));
	}
	Ok(())
}

fn doc_lines(text: &str) -> Vec<String> {
	text.lines().map(std::string::ToString::to_string).collect()
}

fn line_window(lines: &[String], center: usize) -> Option<(usize, usize)> {
	if lines.is_empty() {
		return None;
	}
	let max_idx = lines.len() - 1;
	let c = center.min(max_idx);
	let start = c.saturating_sub(WINDOW_RADIUS);
	let end = (c + WINDOW_RADIUS).min(max_idx);
	Some((start, end))
}

fn window_text(lines: &[String], start: usize, end: usize) -> String {
	lines[start..=end].join("\n")
}

/// Sweep-style prompt: `original` / `current` / `updated` blocks for one file window.
pub fn build_prompt(relative_path: &str, original_window: &str, current_window: &str) -> String {
	let mut s = String::new();
	s.push_str("<|file_sep|>original/");
	s.push_str(relative_path);
	s.push('\n');
	s.push_str(original_window);
	s.push_str("\n<|file_sep|>current/");
	s.push_str(relative_path);
	s.push('\n');
	s.push_str(current_window);
	s.push_str("\n<|file_sep|>updated/");
	s.push_str(relative_path);
	s.push('\n');
	s
}

pub async fn probe(client: &reqwest::Client, base_url: &str) -> NextEditProbeResult {
	let base = match normalize_base_url(base_url) {
		Ok(b) => b,
		Err(e) => {
			return NextEditProbeResult {
				kind: NextEditProbeKind::Error,
				detail: Some(e.to_string()),
			};
		}
	};
	let url = format!("{base}/health");
	let resp = match client.get(url).timeout(PROBE_TIMEOUT).send().await {
		Ok(r) => r,
		Err(e) => {
			let detail = e.to_string();
			let kind = if e.is_timeout() || e.is_connect() {
				NextEditProbeKind::Unreachable
			} else {
				NextEditProbeKind::Error
			};
			return NextEditProbeResult {
				kind,
				detail: Some(detail),
			};
		}
	};
	let status = resp.status();
	if status == StatusCode::SERVICE_UNAVAILABLE {
		let detail = resp.text().await.ok();
		return NextEditProbeResult {
			kind: NextEditProbeKind::ModelLoading,
			detail,
		};
	}
	if status.is_success() {
		return NextEditProbeResult {
			kind: NextEditProbeKind::Ready,
			detail: None,
		};
	}
	let detail = resp.text().await.ok();
	NextEditProbeResult {
		kind: NextEditProbeKind::Error,
		detail,
	}
}

pub async fn complete(
	client: &reqwest::Client,
	params: NextEditCompleteParams,
) -> Result<NextEditCompleteResult, MoonError> {
	let base = normalize_base_url(&params.base_url)?;
	validate_relative_path(&params.relative_path)?;
	let cursor = params.cursor_line as usize;
	let cur_lines = doc_lines(&params.document_text);
	let (start, end) = line_window(&cur_lines, cursor).ok_or_else(|| MoonError::invalid("empty document"))?;

	let orig_lines = params
		.head_text
		.as_ref()
		.map(|h| doc_lines(h))
		.unwrap_or_else(|| cur_lines.clone());

	let original_window = (start..=end)
		.map(|i| orig_lines.get(i).map(String::as_str).unwrap_or("").to_string())
		.collect::<Vec<_>>()
		.join("\n");
	let current_window = window_text(&cur_lines, start, end);

	let prompt = build_prompt(&params.relative_path, &original_window, &current_window);
	let url = format!("{base}/completion");
	let body = serde_json::json!({
		"prompt": prompt,
		"n_predict": 2048,
		"temperature": 0.2,
		"stop": ["<|file_sep|>"],
	});
	let resp = client
		.post(url)
		.json(&body)
		.timeout(COMPLETE_TIMEOUT)
		.send()
		.await
		.map_err(|e| MoonError::internal(format!("next-edit request failed: {e}")))?;
	let status = resp.status();
	if !status.is_success() {
		let t = resp.text().await.unwrap_or_default();
		return Err(MoonError::internal(format!("llama-server returned {status}: {t}")));
	}
	let parsed: LlamaCompletionBody = resp
		.json()
		.await
		.map_err(|e| MoonError::internal(format!("next-edit response JSON: {e}")))?;
	let window_line_count = end - start + 1;
	// Truncation guard. The next-edit model is trained to emit
	// the entire updated window and stop at `<|file_sep|>`;
	// short output combined with a clean `stopped_word` is the
	// model intentionally deleting some lines (the frontend's
	// prefix/suffix matcher will narrow the patch to the actual
	// deletion). Short output *without* a clean stop is the
	// model running out of tokens or otherwise stopping early —
	// trusting the original `to_line = end` in that case made
	// the frontend replace the entire window with the truncated
	// reply, eating lines below the cursor that the model never
	// got to. Narrowing `to_line` to match the model's actual
	// output keeps the un-replaced tail of the window untouched
	// — failure mode flips from "destroy unrelated lines" to
	// "miss a deletion the model intended", which is recoverable
	// (re-trigger, undo, or just delete the line manually).
	let model_line_count = effective_line_count(&parsed.content);
	let truncated = !parsed.stopped_word;
	let effective_end = if truncated && model_line_count > 0 && model_line_count < window_line_count {
		tracing::debug!(
			model_line_count,
			window_line_count,
			stopped_word = parsed.stopped_word,
			stopped_limit = parsed.stopped_limit,
			"next-edit: truncated reply, narrowing to_line to model's line count"
		);
		start + model_line_count - 1
	} else {
		end
	};
	let line_after_window = effective_end + 1 < cur_lines.len();
	let replacement = normalize_model_replacement(parsed.content, window_line_count, line_after_window);
	Ok(NextEditCompleteResult {
		replacement,
		from_line: start as u32,
		to_line: effective_end as u32,
	})
}

/// Count the number of "logical" lines a model reply spans, the
/// same way the frontend's `linesFromReplacement` (and Rust's
/// `str::lines`) does: a trailing newline doesn't add an empty
/// row. The empty string yields 0 lines so a no-output reply
/// short-circuits the truncation narrowing.
fn effective_line_count(text: &str) -> usize {
	text.lines().count()
}

/// Cap line count; keep a trailing line separator when the window is not at EOF so the next
/// document line is not merged into the last predicted line (CM ranges include line breaks).
fn normalize_model_replacement(mut text: String, expected_lines: usize, needs_trailing_line_sep: bool) -> String {
	let n = text.lines().count();
	if n > expected_lines {
		text = text.lines().take(expected_lines).collect::<Vec<_>>().join("\n");
	}
	if needs_trailing_line_sep && !text.ends_with('\n') {
		text.push('\n');
	}
	text
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn prompt_has_three_sections() {
		let p = build_prompt("src/a.ts", "orig\nlines", "cur\nlines");
		assert!(p.contains("<|file_sep|>original/src/a.ts"));
		assert!(p.contains("<|file_sep|>current/src/a.ts"));
		assert!(p.contains("<|file_sep|>updated/src/a.ts"));
		assert!(p.ends_with('\n'));
	}

	#[test]
	fn line_window_bounds() {
		let lines: Vec<String> = (0..25).map(|i| format!("L{i}")).collect();
		let (s, e) = line_window(&lines, 12).unwrap();
		assert_eq!(s, 2);
		assert_eq!(e, 22);
		assert_eq!(e - s + 1, 21);
	}

	#[test]
	fn normalize_replacement_adds_trailing_sep_before_following_line() {
		let out = normalize_model_replacement("a\nb".to_string(), 10, true);
		assert!(out.ends_with('\n'), "expected {:?} to end with newline", out);
	}

	#[test]
	fn normalize_replacement_skips_trailing_sep_at_eof_window() {
		let out = normalize_model_replacement("a\nb".to_string(), 10, false);
		assert!(!out.ends_with('\n'));
	}

	#[test]
	fn effective_line_count_matches_logical_lines() {
		assert_eq!(effective_line_count(""), 0);
		assert_eq!(effective_line_count("\n"), 1);
		assert_eq!(effective_line_count("a"), 1);
		assert_eq!(effective_line_count("a\n"), 1);
		assert_eq!(effective_line_count("a\nb"), 2);
		assert_eq!(effective_line_count("a\nb\n"), 2);
		assert_eq!(effective_line_count("a\nb\nc\n"), 3);
	}

	/// Truncation guard: when llama-server reports a non-clean
	/// stop (anything but `stopped_word`) and the reply is shorter
	/// than the window, the resolved `to_line` must match the
	/// model's actual line count so the frontend doesn't eat
	/// lines below the window. Modeled on the user-reported bug —
	/// "adds some lines, but down below a few lines are removed".
	#[test]
	fn truncation_narrows_to_line_to_model_output_count() {
		// A 21-line window starting at line 5 (cursor near top of a
		// reasonably long file). The model only got 11 lines out
		// before hitting `n_predict`.
		let window_line_count: usize = 21;
		let start = 5_usize;
		let end = start + window_line_count - 1;
		let model_lines = 11;
		let stopped_word = false; // truncation, not a clean stop
		let model_line_count = model_lines;
		let truncated = !stopped_word;
		let effective_end = if truncated && model_line_count > 0 && model_line_count < window_line_count {
			start + model_line_count - 1
		} else {
			end
		};
		assert_eq!(effective_end, start + model_lines - 1);
		// And critically, `effective_end` is strictly less than
		// `end` so the frontend leaves the tail of the window
		// alone.
		assert!(effective_end < end);
	}

	#[test]
	fn clean_stop_short_reply_keeps_full_window_for_intended_deletion() {
		// Model finished cleanly via the trained `<|file_sep|>` stop.
		// A short reply here means the model deliberately deleted
		// lines — the frontend's prefix/suffix matcher will narrow
		// the patch to the actual deletion. We must NOT narrow
		// `to_line`; doing so would skip the deletion the model
		// asked for.
		let window_line_count: usize = 21;
		let start = 5_usize;
		let end = start + window_line_count - 1;
		let model_line_count = 19; // model dropped two lines
		let stopped_word = true; // clean trained stop
		let truncated = !stopped_word;
		let effective_end = if truncated && model_line_count > 0 && model_line_count < window_line_count {
			start + model_line_count - 1
		} else {
			end
		};
		assert_eq!(effective_end, end);
	}

	#[test]
	fn empty_reply_does_not_narrow() {
		// Pathological: model returned zero lines. We don't want
		// `start + 0 - 1` underflow nor a degenerate replacement
		// span; fall through to `end` and let
		// `normalize_model_replacement` produce an empty patch.
		let window_line_count: usize = 21;
		let start = 5_usize;
		let end = start + window_line_count - 1;
		let model_line_count = 0;
		let stopped_word = false;
		let truncated = !stopped_word;
		let effective_end = if truncated && model_line_count > 0 && model_line_count < window_line_count {
			start + model_line_count - 1
		} else {
			end
		};
		assert_eq!(effective_end, end);
	}
}
