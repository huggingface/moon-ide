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
	ContentReplaceError, ContentReplaceOptions, ContentReplaceResult, ContentSearchHit, ContentSearchOptions,
	ContentSearchResult, FileSearchOptions, FileSearchResult,
};
use moon_protocol::{MoonError, MoonResult};
use regex::Regex;

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
///
/// `include_glob`, when provided, layers an *inclusion* override on
/// top: anything not matching the user's glob is filtered out at
/// walk time (the `ignore` crate short-circuits whole subtrees, so
/// the speedup is real on `target/`-laden repos). Invalid globs are
/// dropped with a `tracing::warn!` and the search runs unfiltered —
/// breaking the search UI on a typo is worse than silently widening
/// the scope.
fn build_overrides(root: &Utf8Path, include_glob: Option<&str>) -> Override {
	let mut builder = OverrideBuilder::new(root.as_std_path());
	let _ = builder.add("!.git/");
	if let Some(raw) = include_glob {
		let trimmed = raw.trim();
		if !trimmed.is_empty() {
			let normalised = normalise_include_glob(trimmed);
			if let Err(err) = builder.add(&normalised) {
				tracing::warn!(
					%err,
					original = trimmed,
					normalised = %normalised,
					"invalid include_glob; running search without an include filter"
				);
			}
		}
	}
	builder.build().unwrap_or_else(|_| Override::empty())
}

/// Convert the user's "scope to a path" input into a gitignore-style
/// pattern the `ignore` crate accepts. Patterns containing a glob
/// metacharacter (`*`, `?`, `[`, `]`) or a `!` negation are passed
/// through verbatim — the user already knows what they're doing. A
/// bare path (`src/lib`, `crates/moon-coder/`) is expanded to
/// `<path>/**` so it actually matches files *under* that directory
/// rather than only a sibling literally named that string.
fn normalise_include_glob(raw: &str) -> String {
	let has_glob = raw.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b']' | b'!'));
	if has_glob {
		return raw.to_string();
	}
	let trimmed = raw.trim_end_matches('/');
	format!("{trimmed}/**")
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
		.overrides(build_overrides(root, None))
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

	let raw_pattern = if opts.regex {
		query.to_string()
	} else {
		regex_syntax::escape(query)
	};
	// `\b` word boundaries wrap the *final* pattern, after the user's
	// regex / escape has been applied. That way `whole_word=true` stays
	// composable: in plain mode it word-bounds the literal; in regex
	// mode it word-bounds the user's pattern (`\bfoo|bar\b` is the
	// caller's call to make if they want grouping, but `(?:...)`
	// isn't worth automating from the toggle).
	let bounded_pattern = if opts.whole_word {
		format!(r"\b(?:{raw_pattern})\b")
	} else {
		raw_pattern
	};

	let matcher = if opts.case_sensitive {
		RegexMatcher::new(&bounded_pattern)
	} else {
		RegexMatcher::new(&format!("(?i){bounded_pattern}"))
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
		.overrides(build_overrides(root, opts.include_glob.as_deref()))
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

/// Mass-replace across every file the search walker would visit.
/// Plain-text mode escapes both the pattern *and* the replacement
/// so `$` / `\` / backrefs in the replacement string are literal;
/// regex mode lets `Regex::replace_all` expand `$1` / `${name}` /
/// `$$` per its standard rules. Each file is read, replaced, and
/// — only if the bytes actually change — written back atomically
/// via `std::fs::write` (which is a single `O_TRUNC` + `write`,
/// good enough for the IDE's "I just did a refactor" use case;
/// fancier crash-safety isn't worth the complexity yet).
///
/// We deliberately do **not** consult open editor buffers here:
/// the surface area at the FS layer is small and predictable, and
/// the existing file-watcher pipeline will pick the new bytes up.
/// Callers that care about unsaved buffers (the search panel) gate
/// the action UI-side.
pub fn replace_content(root: &Utf8Path, opts: &ContentReplaceOptions) -> MoonResult<ContentReplaceResult> {
	let query = opts.query.trim();
	if query.is_empty() {
		return Ok(ContentReplaceResult {
			files_changed: 0,
			replacements: 0,
			errors: Vec::new(),
		});
	}

	let (pattern, replacement) = build_replace_pattern(query, &opts.replacement, opts.regex, opts.whole_word);

	let regex = build_replace_regex(&pattern, opts.case_sensitive)?;
	// `grep-regex`'s matcher is the path we already use for the
	// preview search — keeping it in the walk loop means "skip
	// files that don't contain a match" stays a single, cheap
	// scan over the bytes (no per-line UTF-8 conversion). Only
	// the files that actually need a rewrite enter the read /
	// edit / write path below.
	let prefilter = if opts.case_sensitive {
		RegexMatcher::new(&pattern)
	} else {
		RegexMatcher::new(&format!("(?i){pattern}"))
	}
	.map_err(|e| MoonError::invalid(format!("invalid regex: {e}")))?;

	let mut files_changed = 0u32;
	let mut total_replacements = 0u32;
	let mut errors: Vec<ContentReplaceError> = Vec::new();

	let walker = WalkBuilder::new(root.as_std_path())
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.overrides(build_overrides(root, opts.include_glob.as_deref()))
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
			Some(s) => s.to_string(),
			None => continue,
		};

		if !file_contains_match(&prefilter, path) {
			continue;
		}

		let original = match std::fs::read_to_string(path) {
			Ok(s) => s,
			Err(err) => {
				// Non-UTF-8 / unreadable files. The prefilter
				// flagged a byte match, but we can't safely
				// rewrite without a UTF-8 view, so skip with an
				// entry the UI can surface.
				errors.push(ContentReplaceError {
					path: rel_str,
					message: format!("read failed: {err}"),
				});
				continue;
			}
		};

		let (replaced_text, n) = regex_replace_all_counted(&regex, &original, &replacement);
		if n == 0 || replaced_text == original {
			continue;
		}

		if let Err(err) = std::fs::write(path, replaced_text.as_bytes()) {
			errors.push(ContentReplaceError {
				path: rel_str,
				message: format!("write failed: {err}"),
			});
			continue;
		}

		files_changed = files_changed.saturating_add(1);
		total_replacements = total_replacements.saturating_add(n as u32);
	}

	Ok(ContentReplaceResult {
		files_changed,
		replacements: total_replacements,
		errors,
	})
}

/// Returns `(pattern, replacement)` ready to feed to
/// [`Regex::replace_all`]. In plain-text mode the pattern is
/// regex-escaped (so the query is treated literally) and the
/// replacement is run through [`regex::Regex::replace_all`]'s
/// "no expansion" escape (`$` → `$$`, `\` is already literal in
/// the replacement language) so the user's typed text doesn't
/// accidentally trigger backref expansion when they were just
/// trying to type a `$` sign.
fn build_replace_pattern(query: &str, replacement: &str, regex: bool, whole_word: bool) -> (String, String) {
	let raw = if regex {
		query.to_string()
	} else {
		regex_syntax::escape(query)
	};
	let pattern = if whole_word { format!(r"\b(?:{raw})\b") } else { raw };
	let replacement = if regex {
		replacement.to_string()
	} else {
		replacement.replace('$', "$$")
	};
	(pattern, replacement)
}

fn build_replace_regex(pattern: &str, case_sensitive: bool) -> MoonResult<Regex> {
	let body = if case_sensitive {
		pattern.to_string()
	} else {
		format!("(?i){pattern}")
	};
	Regex::new(&body).map_err(|e| MoonError::invalid(format!("invalid regex: {e}")))
}

fn file_contains_match(matcher: &RegexMatcher, path: &std::path::Path) -> bool {
	// Reuse the grep-searcher pipeline used by `search_content` so
	// the "is there a hit?" decision is byte-identical to what the
	// preview UI just showed. `found` flips on the first match and
	// the sink returns `false` to short-circuit the rest of the
	// file — for big files with an early match this is much
	// cheaper than reading the whole thing twice.
	let mut found = false;
	let sink = UTF8(|_line, _line_text| {
		found = true;
		Ok(false)
	});
	let mut searcher = Searcher::new();
	let _ = searcher.search_path(matcher, path, sink);
	found
}

/// `Regex::replace_all` doesn't expose a count of substitutions
/// without a second pass. We do the second pass cheaply via
/// `find_iter` and lean on `replace_all` for the actual rewrite —
/// both are linear in the input length, and we only do them for
/// files the prefilter has already confirmed contain ≥1 match.
fn regex_replace_all_counted(re: &Regex, hay: &str, replacement: &str) -> (String, usize) {
	let count = re.find_iter(hay).count();
	let rewritten = re.replace_all(hay, replacement).into_owned();
	(rewritten, count)
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
			max_matches: 100,
			..Default::default()
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
			max_matches: 100,
			..Default::default()
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
			max_matches: 100,
			..Default::default()
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
	fn content_search_whole_word_filters_substring_hits() {
		// `whole_word: true` should match `print` standalone but not
		// `println` / `imprinted`. Both case-sensitive and case-
		// insensitive paths must respect the boundary.
		let dir = TempDir::new().unwrap();
		std::fs::write(
			dir.path().join("a.txt"),
			"print here\nprintln also here\nimprinted again\n",
		)
		.unwrap();

		let opts = ContentSearchOptions {
			query: "print".into(),
			whole_word: true,
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert_eq!(
			r.hits.len(),
			1,
			"whole-word should drop println / imprinted: {:?}",
			r.hits
		);
		assert_eq!(r.hits[0].line, 1);
	}

	#[test]
	fn content_search_whole_word_composes_with_regex_pattern() {
		// In regex mode the user's pattern gets wrapped in `\b(?:..)\b`,
		// so `foo|bar` with whole-word matches `foo` and `bar` as
		// words but not `foobar`.
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "foo bar\nfoobar\n").unwrap();

		let opts = ContentSearchOptions {
			query: "foo|bar".into(),
			regex: true,
			whole_word: true,
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		// Line 1 has two whole-word matches (`foo` then `bar`); line 2
		// has zero. We collect at most one hit per line in the current
		// sink shape, so the assertion is "line 1 only".
		assert!(
			r.hits.iter().all(|h| h.line == 1),
			"whole-word regex leaked into foobar: {:?}",
			r.hits
		);
		assert!(
			!r.hits.is_empty(),
			"whole-word regex dropped the real matches: {:?}",
			r.hits
		);
	}

	#[test]
	fn content_search_include_glob_scopes_to_subdirectory() {
		// Bare path scoping is the common case users actually reach
		// for: "search only inside crates/moon-core/". We normalise it
		// to `<path>/**` so a hit in a sibling directory drops out.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("crates/moon-core")).unwrap();
		std::fs::create_dir_all(dir.path().join("crates/moon-protocol")).unwrap();
		std::fs::write(dir.path().join("crates/moon-core/foo.rs"), "needle here\n").unwrap();
		std::fs::write(dir.path().join("crates/moon-protocol/bar.rs"), "needle there\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "needle outside\n").unwrap();

		let opts = ContentSearchOptions {
			query: "needle".into(),
			include_glob: Some("crates/moon-core".into()),
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert!(
			r.hits.iter().all(|h| h.path.starts_with("crates/moon-core/")),
			"include_glob leaked into siblings: {:?}",
			r.hits
		);
		assert!(
			!r.hits.is_empty(),
			"include_glob dropped every legitimate hit: {:?}",
			r.hits
		);
	}

	#[test]
	fn content_search_include_glob_passes_explicit_globs_through() {
		// `**/*.svelte` (or any pattern with a glob metacharacter)
		// goes to the override builder verbatim — the user knows
		// what they want.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("src")).unwrap();
		std::fs::write(dir.path().join("src/Foo.svelte"), "needle in svelte\n").unwrap();
		std::fs::write(dir.path().join("src/foo.ts"), "needle in ts\n").unwrap();

		let opts = ContentSearchOptions {
			query: "needle".into(),
			include_glob: Some("**/*.svelte".into()),
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert!(
			r.hits.iter().all(|h| h.path.ends_with(".svelte")),
			"include_glob `**/*.svelte` should only match .svelte: {:?}",
			r.hits
		);
		assert_eq!(r.hits.len(), 1);
	}

	#[test]
	fn content_search_invalid_include_glob_falls_back_to_unfiltered() {
		// Rather than failing the search outright, a bad glob (e.g.
		// an unterminated bracket expression) should warn and run the
		// search without an include filter. Breaking the search UI
		// on a typo is the worse failure mode.
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
		let opts = ContentSearchOptions {
			query: "needle".into(),
			include_glob: Some("[".into()),
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.hits.len(), 1);
	}

	#[test]
	fn replace_content_rewrites_matching_files_in_place() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "foo bar foo\nbaz\n").unwrap();
		std::fs::write(dir.path().join("b.txt"), "no match here\n").unwrap();

		let opts = ContentReplaceOptions {
			query: "foo".into(),
			replacement: "qux".into(),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(r.replacements, 2);
		assert!(r.errors.is_empty());
		assert_eq!(
			std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
			"qux bar qux\nbaz\n"
		);
		assert_eq!(
			std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
			"no match here\n"
		);
	}

	#[test]
	fn replace_content_skips_no_op_writes() {
		// If the replacement equals the matched text, we should not
		// touch the file at all — `files_changed` stays at 0 so the
		// UI doesn't claim work happened when the bytes on disk
		// would be identical.
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "alpha beta\n").unwrap();
		let mtime_before = std::fs::metadata(dir.path().join("a.txt")).unwrap().modified().unwrap();
		std::thread::sleep(std::time::Duration::from_millis(10));

		let opts = ContentReplaceOptions {
			query: "alpha".into(),
			replacement: "alpha".into(),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 0);
		assert_eq!(r.replacements, 0);
		let mtime_after = std::fs::metadata(dir.path().join("a.txt")).unwrap().modified().unwrap();
		assert_eq!(mtime_before, mtime_after, "no-op replace must not touch mtime");
	}

	#[test]
	fn replace_content_plain_text_treats_dollar_sign_literally() {
		// In plain-text mode the user types `$1` to mean the
		// literal string `$1` — not a regex backreference. The
		// pre-escape (`$` → `$$`) in `build_replace_pattern` is
		// what makes this work; the test guards the contract from
		// future regressions.
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();

		let opts = ContentReplaceOptions {
			query: "before".into(),
			replacement: "$1 after".into(),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(r.replacements, 1);
		assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "$1 after\n");
	}

	#[test]
	fn replace_content_regex_mode_expands_backreferences() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();

		let opts = ContentReplaceOptions {
			query: r"(\w+) (\w+)".into(),
			replacement: "$2 $1".into(),
			regex: true,
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(r.replacements, 1);
		assert_eq!(
			std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
			"world hello\n"
		);
	}

	#[test]
	fn replace_content_respects_whole_word_toggle() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "print here\nprintln also\nimprinted\n").unwrap();

		let opts = ContentReplaceOptions {
			query: "print".into(),
			replacement: "log".into(),
			whole_word: true,
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(r.replacements, 1);
		assert_eq!(
			std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
			"log here\nprintln also\nimprinted\n"
		);
	}

	#[test]
	fn replace_content_respects_include_glob_scope() {
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join("src")).unwrap();
		std::fs::create_dir_all(dir.path().join("vendor")).unwrap();
		std::fs::write(dir.path().join("src/a.txt"), "todo\n").unwrap();
		std::fs::write(dir.path().join("vendor/b.txt"), "todo\n").unwrap();

		let opts = ContentReplaceOptions {
			query: "todo".into(),
			replacement: "done".into(),
			include_glob: Some("src".into()),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(std::fs::read_to_string(dir.path().join("src/a.txt")).unwrap(), "done\n");
		assert_eq!(
			std::fs::read_to_string(dir.path().join("vendor/b.txt")).unwrap(),
			"todo\n"
		);
	}

	#[test]
	fn replace_content_skips_dot_git_directory() {
		// `.git/` is excluded for the same reason `search_content`
		// skips it: we never want a refactor to rewrite pack files
		// or refs. Mirror of `content_search_skips_dot_git_directory`.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git/logs")).unwrap();
		std::fs::write(dir.path().join(".git/logs/HEAD"), "needle\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "needle\n").unwrap();

		let opts = ContentReplaceOptions {
			query: "needle".into(),
			replacement: "haystack".into(),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(
			std::fs::read_to_string(dir.path().join(".git/logs/HEAD")).unwrap(),
			"needle\n"
		);
		assert_eq!(
			std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
			"haystack\n"
		);
	}

	#[test]
	fn replace_content_empty_query_is_a_no_op() {
		// `query.trim().is_empty()` short-circuits the walker — no
		// reads, no writes, no errors. The "what if I leave the
		// search box blank?" panic case.
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("a.txt"), "anything\n").unwrap();
		let opts = ContentReplaceOptions {
			query: "   ".into(),
			replacement: "x".into(),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 0);
		assert_eq!(r.replacements, 0);
		assert!(r.errors.is_empty());
		assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "anything\n");
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
			max_matches: 500,
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.hits.len(), 1);
		assert_eq!(r.hits[0].path, "zzz-target.txt");
		assert!(!r.truncated);
	}
}
