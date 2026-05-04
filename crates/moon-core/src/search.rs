//! File-name and ripgrep-backed content search.
//!
//! For the local host we use the same `ignore` walker that ripgrep uses, so we
//! respect `.gitignore` semantics by default. Content search uses BurntSushi's
//! `grep-*` crates rather than shelling out — fewer moving parts, identical
//! behavior, and works from inside the agent process the same way.

use camino::Utf8Path;
use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{sinks::UTF8, Searcher};
use ignore::WalkBuilder;
use moon_protocol::search::{
	ContentSearchHit, ContentSearchOptions, ContentSearchResult, FileSearchOptions, FileSearchResult,
};
use moon_protocol::{MoonError, MoonResult};

/// Minimum score below which file-name candidates are dropped from the result list.
const FILE_SEARCH_MIN_SCORE: i64 = 0;

pub fn search_files(root: &Utf8Path, opts: &FileSearchOptions) -> MoonResult<Vec<FileSearchResult>> {
	let query = opts.query.trim().to_lowercase();
	if query.is_empty() {
		return Ok(Vec::new());
	}

	let limit = opts.limit.clamp(1, 500);
	let mut hits: Vec<FileSearchResult> = Vec::new();

	let walker = WalkBuilder::new(root.as_std_path())
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.build();

	for entry in walker.flatten() {
		let path = entry.path();
		if !path.is_file() {
			continue;
		}
		let rel = match path.strip_prefix(root.as_std_path()) {
			Ok(p) => p,
			Err(_) => continue,
		};
		let rel_str = match rel.to_str() {
			Some(s) => s,
			None => continue,
		};
		let score = score_file(rel_str, &query);
		if score <= FILE_SEARCH_MIN_SCORE {
			continue;
		}
		hits.push(FileSearchResult {
			path: rel_str.to_string(),
			score,
		});
	}

	// Highest score first; ties broken by shorter path.
	hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.len().cmp(&b.path.len())));
	hits.truncate(limit);
	Ok(hits)
}

/// A small fuzzy-ish scorer. Not as good as a real fuzzy matcher, but it
/// matches whole-word and prefix-of-basename strongly so common cases feel
/// fast. We can swap in `nucleo-matcher` later without changing call sites.
fn score_file(path: &str, query: &str) -> i64 {
	let path_lower = path.to_lowercase();
	let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);

	let mut score = 0i64;

	if path_lower == query {
		return 1_000_000;
	}
	if basename == query {
		score += 100_000;
	}
	if basename.starts_with(query) {
		score += 30_000;
	}
	if path_lower.contains(query) {
		score += 10_000;
	}

	// Each query word found as a contiguous substring scores; non-contiguous
	// letter sequences score weakly.
	for word in query.split_whitespace() {
		if path_lower.contains(word) {
			score += 1_000;
		} else if has_chars_in_order(&path_lower, word) {
			score += 50;
		} else {
			return 0;
		}
	}

	score -= path.len() as i64; // shorter wins on ties
	score
}

fn has_chars_in_order(haystack: &str, needle: &str) -> bool {
	let mut iter = haystack.chars();
	needle.chars().all(|c| iter.any(|h| h == c))
}

pub fn search_content(root: &Utf8Path, opts: &ContentSearchOptions) -> MoonResult<ContentSearchResult> {
	let query = opts.query.trim();
	if query.is_empty() {
		return Ok(ContentSearchResult {
			hits: Vec::new(),
			truncated: false,
		});
	}

	let pattern = if opts.regex {
		query.to_string()
	} else {
		regex_syntax::escape(query)
	};

	let matcher = if opts.case_sensitive {
		RegexMatcher::new(&pattern)
	} else {
		RegexMatcher::new(&format!("(?i){pattern}"))
	}
	.map_err(|e| MoonError::invalid(format!("invalid regex: {e}")))?;

	let max_matches = opts.max_matches.clamp(1, 10_000);

	let mut hits = Vec::new();
	let mut truncated = false;

	let walker = WalkBuilder::new(root.as_std_path())
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.build();

	'outer: for entry in walker.flatten() {
		let path = entry.path();
		if !path.is_file() {
			continue;
		}

		let rel = match path.strip_prefix(root.as_std_path()) {
			Ok(p) => p,
			Err(_) => continue,
		};
		let rel_str = match rel.to_str() {
			Some(s) => s.to_string(),
			None => continue,
		};

		let mut searcher = Searcher::new();
		let mut local_hits: Vec<ContentSearchHit> = Vec::new();

		let path_for_hit = rel_str.clone();
		let matcher_for_sink = matcher.clone();
		let sink = UTF8(|line, line_text| {
			let trimmed = line_text.trim_end_matches(['\n', '\r']);
			// Find the first match position on this line for column reporting.
			let (m_start, m_end) = find_first_match(&matcher_for_sink, trimmed.as_bytes()).unwrap_or((0, 0));
			local_hits.push(ContentSearchHit {
				path: path_for_hit.clone(),
				line,
				column: m_start as u64 + 1,
				line_text: trimmed.to_string(),
				match_start: m_start,
				match_end: m_end,
			});
			Ok(local_hits.len() < max_matches)
		});

		if let Err(err) = searcher.search_path(&matcher, path, sink) {
			tracing::debug!(?err, ?path, "content search skipped file");
			continue;
		}

		for hit in local_hits {
			if hits.len() >= max_matches {
				truncated = true;
				break 'outer;
			}
			hits.push(hit);
		}
	}

	Ok(ContentSearchResult { hits, truncated })
}

fn find_first_match(matcher: &RegexMatcher, line: &[u8]) -> Option<(u32, u32)> {
	matcher
		.find(line)
		.ok()
		.flatten()
		.map(|m: grep_matcher::Match| (m.start() as u32, m.end() as u32))
}

#[cfg(test)]
mod tests {
	use super::*;
	use camino::Utf8PathBuf;
	use tempfile::TempDir;

	fn root(dir: &TempDir) -> Utf8PathBuf {
		Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap()
	}

	#[test]
	fn file_search_finds_matching_basenames() {
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("src")).unwrap();
		std::fs::write(dir.path().join("src/Welcome.ts"), "").unwrap();
		std::fs::write(dir.path().join("src/other.ts"), "").unwrap();

		let opts = FileSearchOptions {
			query: "wel".into(),
			limit: 10,
		};
		let hits = search_files(&root(&dir), &opts).unwrap();
		assert!(!hits.is_empty());
		assert_eq!(hits[0].path, "src/Welcome.ts");
	}

	#[test]
	fn content_search_finds_text() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "hello world\nfoo bar\n").unwrap();
		std::fs::write(dir.path().join("b.txt"), "another HELLO\n").unwrap();

		let opts = ContentSearchOptions {
			query: "hello".into(),
			case_sensitive: false,
			regex: false,
			max_matches: 100,
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.hits.len(), 2);
		assert!(!r.truncated);
	}

	#[test]
	fn content_search_visits_files_past_old_default_cap() {
		// Regression: a previous default `max_files = 1000` silently
		// bailed out of the walk before reaching anything past the
		// 1000th file in walk order, surfacing as "no results" on
		// any real-sized workspace. Plant a target match well past
		// that boundary and assert it still surfaces.
		let dir = TempDir::new().unwrap();
		for i in 0..1500 {
			std::fs::write(dir.path().join(format!("noise-{i:04}.txt")), "lorem ipsum\n").unwrap();
		}
		std::fs::write(dir.path().join("zzz-target.txt"), "ReadRepoContent\n").unwrap();

		let opts = ContentSearchOptions {
			query: "ReadRepoContent".into(),
			case_sensitive: false,
			regex: false,
			max_matches: 500,
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.hits.len(), 1);
		assert_eq!(r.hits[0].path, "zzz-target.txt");
		assert!(!r.truncated);
	}
}
