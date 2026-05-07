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
use ignore::overrides::{Override, OverrideBuilder};
use ignore::WalkBuilder;
use moon_protocol::search::{
	ContentSearchHit, ContentSearchOptions, ContentSearchResult, FileSearchOptions, FileSearchResult,
};
use moon_protocol::{MoonError, MoonResult};

/// Minimum score below which file-name candidates are dropped from the result list.
const FILE_SEARCH_MIN_SCORE: i64 = 0;

/// Build the per-walk override set: skip `.git/` explicitly so the
/// search results don't drown in pack files / object blobs / log
/// chatter. ripgrep gets this for free because its default
/// `hidden(true)` filters every dotdir; we set `hidden(false)` to
/// surface dotfiles like `.editorconfig`, so we have to add `.git`
/// back as an explicit exclusion.
///
/// `.gitignore` (`node_modules/`, `target/`, `dist/`, …) and
/// `.git/info/exclude` are still respected via
/// `WalkBuilder::git_ignore(true)` / `git_exclude(true)` — this
/// override only patches the one case those features can't cover.
fn build_overrides(root: &Utf8Path) -> Override {
	let mut builder = OverrideBuilder::new(root.as_std_path());
	let _ = builder.add("!.git/");
	builder.build().unwrap_or_else(|_| Override::empty())
}

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
		.overrides(build_overrides(root))
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
		.overrides(build_overrides(root))
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
	fn file_search_skips_dot_git_directory() {
		// `.git/` is not gitignored (it's *the* git store), and we
		// run with `hidden(false)` so dotfiles like `.editorconfig`
		// surface — the explicit override is the only thing keeping
		// search out of pack files / log blobs / refs.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git/logs")).unwrap();
		std::fs::write(dir.path().join(".git/logs/HEAD"), "fakelog\n").unwrap();
		std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "# real file\n").unwrap();

		let opts = FileSearchOptions {
			query: "head".into(),
			limit: 50,
		};
		let hits = search_files(&root(&dir), &opts).unwrap();
		assert!(
			hits.iter().all(|h| !h.path.starts_with(".git/")),
			"file search leaked into .git/: {hits:?}"
		);
	}

	#[test]
	fn content_search_skips_dot_git_directory() {
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git/logs")).unwrap();
		std::fs::write(dir.path().join(".git/logs/HEAD"), "needle-in-git\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "needle-in-readme\n").unwrap();

		let opts = ContentSearchOptions {
			query: "needle".into(),
			case_sensitive: false,
			regex: false,
			max_matches: 100,
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert!(
			r.hits.iter().all(|h| !h.path.starts_with(".git/")),
			"content search leaked into .git/: {:?}",
			r.hits
		);
		// The readme hit should still come through — we only want
		// `.git/` filtered, not all dotfiles.
		assert!(
			r.hits.iter().any(|h| h.path == "README.md"),
			"readme hit got dropped along with the git filter: {:?}",
			r.hits
		);
	}

	#[test]
	fn content_search_respects_gitignore() {
		// `.gitignore` exclusions (`node_modules/`, `target/`, …)
		// are wired via `WalkBuilder::git_ignore(true)`. Regression
		// against a future change accidentally flipping it off — a
		// `node_modules/`-laden workspace would otherwise drown the
		// results in dependency code. The `ignore` crate only
		// honours `.gitignore` when there's a real `.git/` at or
		// above the search root, so we `git init` (no commits
		// needed — just the metadata directory).
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
		std::fs::create_dir_all(dir.path().join("node_modules/lodash")).unwrap();
		std::fs::write(
			dir.path().join("node_modules/lodash/index.js"),
			"function unique() {}\n",
		)
		.unwrap();
		std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
		std::fs::write(dir.path().join("target/debug/note.txt"), "unique build artefact\n").unwrap();
		std::fs::write(dir.path().join("src.txt"), "unique value\n").unwrap();

		let opts = ContentSearchOptions {
			query: "unique".into(),
			case_sensitive: false,
			regex: false,
			max_matches: 100,
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert!(
			r.hits.iter().all(|h| !h.path.starts_with("node_modules/")),
			"content search leaked into node_modules/: {:?}",
			r.hits
		);
		assert!(
			r.hits.iter().all(|h| !h.path.starts_with("target/")),
			"content search leaked into target/: {:?}",
			r.hits
		);
		assert!(
			r.hits.iter().any(|h| h.path == "src.txt"),
			"unignored hit got dropped along with the gitignore filter: {:?}",
			r.hits
		);
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
