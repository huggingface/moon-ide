//! Resolved EditorConfig for a single file. The host walks .editorconfig
//! up from the file to the workspace root and returns the result —
//! callers don't traverse the cascade themselves.
//!
//! See [specs/editorconfig.md](../../../specs/editorconfig.md).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum IndentStyle {
	Tab,
	Space,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum EndOfLine {
	Lf,
	Crlf,
	Cr,
}

/// Fully resolved editorconfig for one file. Defaults reflect moon-ide
/// house style (tabs at width 2, lf, trim trailing whitespace, final
/// newline) — when no `.editorconfig` is present these are what the
/// editor and the pre-save pipeline use. The defaults match what oxfmt /
/// prettier / rustfmt produce for this repo, so typing in moon-ide-on-
/// moon-ide stays consistent until the format hook fires.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct EditorConfig {
	pub indent_style: IndentStyle,
	pub indent_size: u32,
	pub tab_width: u32,
	pub end_of_line: Option<EndOfLine>,
	pub insert_final_newline: bool,
	pub trim_trailing_whitespace: bool,
	pub charset: String,
	pub max_line_length: Option<u32>,
}

impl Default for EditorConfig {
	fn default() -> Self {
		Self {
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
}
