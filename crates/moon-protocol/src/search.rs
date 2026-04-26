//! File-name and content search across the active workspace.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, TS, Default)]
#[ts(export)]
pub struct FileSearchOptions {
	/// User query. Whitespace is treated as fuzzy "AND each word matches".
	pub query: String,
	#[serde(default = "default_limit")]
	pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileSearchResult {
	pub path: String,
	/// Match score, higher is better. Used purely for ordering.
	pub score: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, Default)]
#[ts(export)]
pub struct ContentSearchOptions {
	pub query: String,
	#[serde(default)]
	pub case_sensitive: bool,
	#[serde(default)]
	pub regex: bool,
	/// Cap to keep the UI responsive. The first `max_matches` matches are returned.
	#[serde(default = "default_max_matches")]
	pub max_matches: usize,
	/// Cap on number of lines of context per match.
	#[serde(default = "default_max_files")]
	pub max_files: usize,
}

fn default_limit() -> usize {
	50
}
fn default_max_matches() -> usize {
	500
}
fn default_max_files() -> usize {
	1000
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContentSearchHit {
	pub path: String,
	pub line: u64,
	pub column: u64,
	pub line_text: String,
	/// Range of the matched text within `line_text` (UTF-8 byte offsets).
	pub match_start: u32,
	pub match_end: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContentSearchResult {
	pub hits: Vec<ContentSearchHit>,
	/// True when we hit `max_matches` and stopped early.
	pub truncated: bool,
}
