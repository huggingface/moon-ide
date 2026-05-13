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
	/// When true, the query only matches at word boundaries. Stacks with
	/// `regex`: in regex mode we wrap the pattern with `\b…\b`; in plain
	/// mode we escape first, then wrap. Mirrors VS Code's `Aa | \b | .*`
	/// toggle trio in the search input.
	#[serde(default)]
	pub whole_word: bool,
	/// Restrict the walk to paths matching this filter (relative to the
	/// workspace root, gitignore-style globs). `None` / empty means
	/// "search everything". A bare path like `src/lib` is normalised to
	/// `src/lib/**` so users don't have to remember glob syntax for the
	/// common "scope to a subdirectory" case; anything containing a
	/// glob metacharacter (`*`, `?`, `[`, `]`, `!`) is passed through
	/// to the `ignore` crate's `OverrideBuilder` verbatim, so users
	/// who do know globs can write `**/*.svelte` or `!**/snapshots/**`
	/// and have it Just Work.
	#[serde(default)]
	pub include_glob: Option<String>,
	/// Cap to keep the UI responsive. The first `max_matches` matches are
	/// returned and the rest is reported back via `truncated = true`.
	#[serde(default = "default_max_matches")]
	pub max_matches: usize,
}

fn default_limit() -> usize {
	50
}
fn default_max_matches() -> usize {
	500
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
