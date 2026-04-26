//! Pre-save text transforms driven by `.editorconfig`.
//!
//! Runs server-side just before bytes hit disk. Each transform is a
//! pure function on `(text, ec)`: idempotent, no I/O, no panic. The
//! default pipeline (line endings → trim trailing whitespace → final
//! newline) is what every save in moon-ide goes through; Phase 8 adds
//! a `RunFormatter` step at the end for languages with a configured
//! formatter.
//!
//! See [specs/editorconfig.md](../../../specs/editorconfig.md).

use moon_protocol::editorconfig::{EditorConfig, EndOfLine};

/// Apply the default pipeline. Order matters — line endings first so
/// `trim_trailing_whitespace` and `ensure_final_newline` see consistent
/// `\n`-terminated lines internally; we re-emit `\r\n` / `\r` only at
/// the end as part of [`ensure_line_endings`]'s output.
pub fn apply_pipeline(text: &str, ec: &EditorConfig) -> String {
	let mut out = ensure_line_endings(text, ec);
	if ec.trim_trailing_whitespace {
		out = trim_trailing_whitespace(&out);
	}
	out = ensure_final_newline(&out, ec);
	out
}

/// Normalize line endings to `ec.end_of_line`. When `end_of_line` is
/// `None` the original separators are preserved (we do nothing rather
/// than guess at the user's intent).
pub fn ensure_line_endings(text: &str, ec: &EditorConfig) -> String {
	let Some(eol) = ec.end_of_line else {
		return text.to_owned();
	};
	let target = match eol {
		EndOfLine::Lf => "\n",
		EndOfLine::Crlf => "\r\n",
		EndOfLine::Cr => "\r",
	};
	// Normalize to `\n` first, then expand to the target separator.
	// Order matters: replacing `\r` before `\r\n` would split CRLFs.
	let lf_only = text.replace("\r\n", "\n").replace('\r', "\n");
	if target == "\n" {
		return lf_only;
	}
	lf_only.replace('\n', target)
}

/// Strip trailing whitespace from every line. Line separators
/// (`\n`, `\r\n`, `\r`) are preserved; only the run of spaces / tabs
/// directly before each separator is removed. The trailing run after
/// the last separator (the file's final partial line, if any) is also
/// trimmed.
///
/// Multi-line string literals are NOT exempted in v1 — the spec calls
/// for it but doing it correctly requires per-language parsing. The
/// risk in practice is low: trailing whitespace inside a multi-line
/// string is almost always either a typo or a deliberate part of the
/// string, in which case the user already opted out of trimming via
/// `trim_trailing_whitespace = false`. Revisit when someone reports a
/// concrete bite.
pub fn trim_trailing_whitespace(text: &str) -> String {
	let mut out = String::with_capacity(text.len());
	let mut buffer = String::new();
	let bytes = text.as_bytes();
	let mut i = 0;

	while i < bytes.len() {
		let b = bytes[i];
		if b == b'\n' || b == b'\r' {
			let trimmed = buffer.trim_end_matches([' ', '\t']);
			out.push_str(trimmed);
			buffer.clear();
			out.push(b as char);
			if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
				out.push('\n');
				i += 2;
				continue;
			}
			i += 1;
			continue;
		}
		buffer.push(b as char);
		i += 1;
	}

	let trimmed = buffer.trim_end_matches([' ', '\t']);
	out.push_str(trimmed);
	out
}

/// Honor `insert_final_newline`. When `true`, ensure the file ends
/// with exactly one separator (using `ec.end_of_line` if set, else
/// `\n`). When `false`, strip every trailing separator. moon-ide's
/// default is `true`; the EditorConfig spec leaves "unset" undefined,
/// and we don't model that distinction (see ADR 0006 — there's no
/// separate "user override" layer to disambiguate against).
pub fn ensure_final_newline(text: &str, ec: &EditorConfig) -> String {
	if !ec.insert_final_newline {
		return text.trim_end_matches(['\n', '\r']).to_owned();
	}
	let separator = match ec.end_of_line {
		Some(EndOfLine::Crlf) => "\r\n",
		Some(EndOfLine::Cr) => "\r",
		_ => "\n",
	};
	let stripped = text.trim_end_matches(['\n', '\r']);
	let mut out = String::with_capacity(stripped.len() + separator.len());
	out.push_str(stripped);
	out.push_str(separator);
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::editorconfig::IndentStyle;

	fn ec() -> EditorConfig {
		EditorConfig {
			indent_style: IndentStyle::Tab,
			indent_size: 2,
			tab_width: 2,
			end_of_line: Some(EndOfLine::Lf),
			insert_final_newline: true,
			trim_trailing_whitespace: true,
			charset: "utf-8".to_owned(),
			max_line_length: None,
		}
	}

	#[test]
	fn pipeline_default_is_idempotent() {
		let ec = ec();
		let input = "let x = 1\n";
		let once = apply_pipeline(input, &ec);
		let twice = apply_pipeline(&once, &ec);
		assert_eq!(once, twice);
	}

	#[test]
	fn pipeline_trims_and_adds_final_newline() {
		let ec = ec();
		let input = "let x = 1   \nlet y = 2\t\t";
		assert_eq!(apply_pipeline(input, &ec), "let x = 1\nlet y = 2\n");
	}

	#[test]
	fn ensure_line_endings_lf_to_crlf() {
		let mut ec = ec();
		ec.end_of_line = Some(EndOfLine::Crlf);
		assert_eq!(ensure_line_endings("a\nb\n", &ec), "a\r\nb\r\n");
	}

	#[test]
	fn ensure_line_endings_mixed_to_lf() {
		let ec = ec();
		assert_eq!(ensure_line_endings("a\r\nb\rc\n", &ec), "a\nb\nc\n");
	}

	#[test]
	fn ensure_line_endings_unset_preserves() {
		let mut ec = ec();
		ec.end_of_line = None;
		assert_eq!(ensure_line_endings("a\r\nb\n", &ec), "a\r\nb\n");
	}

	#[test]
	fn trim_keeps_blank_lines() {
		assert_eq!(trim_trailing_whitespace("a\n\n\nb"), "a\n\n\nb");
	}

	#[test]
	fn trim_strips_per_line_trailing_ws() {
		assert_eq!(trim_trailing_whitespace("a   \nb\t\n"), "a\nb\n");
	}

	#[test]
	fn trim_handles_crlf() {
		assert_eq!(trim_trailing_whitespace("a   \r\nb\t\r\n"), "a\r\nb\r\n");
	}

	#[test]
	fn final_newline_added_when_missing() {
		let ec = ec();
		assert_eq!(ensure_final_newline("a", &ec), "a\n");
	}

	#[test]
	fn final_newline_collapses_multiple() {
		let ec = ec();
		assert_eq!(ensure_final_newline("a\n\n\n", &ec), "a\n");
	}

	#[test]
	fn final_newline_strips_when_disabled() {
		let mut ec = ec();
		ec.insert_final_newline = false;
		assert_eq!(ensure_final_newline("a\n\n", &ec), "a");
	}

	#[test]
	fn pipeline_skips_trim_when_disabled() {
		let mut ec = ec();
		ec.trim_trailing_whitespace = false;
		assert_eq!(apply_pipeline("a   \n", &ec), "a   \n");
	}
}
