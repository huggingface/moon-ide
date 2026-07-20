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
/// Note: the user's `include_glob` is deliberately **not** added
/// here. A positive glob in the override set is a *whitelist*
/// match, which per the `ignore` crate's precedence rules
/// short-circuits gitignore filtering — a query scoped to
/// `packages/hub/` would then drag in `packages/hub/node_modules/`.
/// `include_glob` lives in [`build_include_filter`] and is applied
/// via `WalkBuilder::filter_entry` instead, so gitignore still wins.
fn build_overrides(root: &Utf8Path) -> Override {
	let mut builder = OverrideBuilder::new(root.as_std_path());
	let _ = builder.add("!.git/");
	builder.build().unwrap_or_else(|_| Override::empty())
}

/// Compile the user's `include_glob` into a standalone matcher used
/// by `filter_entry`. Returning `None` means "no filter" — either
/// the caller passed nothing or the glob failed to parse (we warn
/// and run unfiltered rather than break the search UI on a typo).
///
/// The matcher is *only* consulted on files; directories are always
/// allowed through so the walker can descend into them and find the
/// files inside (a leaf-level glob like `**/*.svelte` would never
/// match a directory). Gitignore pruning has already happened by
/// the time `filter_entry` runs, so anything ignored upstream stays
/// pruned regardless of what the user typed.
fn build_include_filter(root: &Utf8Path, include_glob: Option<&str>) -> Option<Override> {
	let raw = include_glob?.trim();
	if raw.is_empty() {
		return None;
	}
	let normalised = normalise_include_glob(raw);
	let mut builder = OverrideBuilder::new(root.as_std_path());
	if let Err(err) = builder.add(&normalised) {
		tracing::warn!(
			%err,
			original = raw,
			normalised = %normalised,
			"invalid include_glob; running search without an include filter"
		);
		return None;
	}
	match builder.build() {
		Ok(ov) => Some(ov),
		Err(err) => {
			tracing::warn!(
				%err,
				original = raw,
				normalised = %normalised,
				"failed to build include_glob matcher; running search without an include filter"
			);
			None
		}
	}
}

/// True if `entry` should be kept under the include filter. Files
/// are kept only when the override whitelists them; directories
/// always survive so the walker can descend (a per-file glob like
/// `**/*.svelte` would otherwise prune every directory and yield
/// zero hits).
fn include_filter_keeps(filter: &Override, entry: &ignore::DirEntry) -> bool {
	let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
	if is_dir {
		return true;
	}
	filter.matched(entry.path(), false).is_whitelist()
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

/// Upper bound on the number of file entries we'll score during a
/// quick-open walk. The two-pass walker that surfaces gitignored
/// top-level files (so `.env`, `.envrc`, … are reachable) can
/// otherwise drag a chunky monorepo if a teammate has, say, a
/// `.cache/` they forgot to ignore. The cap is on *visits*, not on
/// matches — the top-K-by-score filter further down keeps the
/// returned list short. 20k is comfortably above any real
/// workspace we've seen, well below the point where the walk
/// itself slows the palette down.
const FILE_SEARCH_MAX_VISITS: usize = 20_000;

pub fn search_files(root: &Utf8Path, opts: &FileSearchOptions) -> MoonResult<Vec<FileSearchResult>> {
	let query = opts.query.trim().to_lowercase();
	if query.is_empty() {
		return Ok(Vec::new());
	}

	let limit = opts.limit.clamp(1, 500);
	let mut hits: Vec<FileSearchResult> = Vec::new();
	let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

	// Pass 1 — gitignore-respecting walk. Tracks every directory
	// the walker enters; that set becomes the allow-list for the
	// second pass below, so a gitignored *file* like `.env` at
	// the root reaches the user, but a gitignored *folder* like
	// `node_modules/` (and everything underneath it) does not.
	//
	// The walker happens to yield every tracked file along the
	// way, so we collect those into the result list directly
	// rather than re-walking them in pass 2.
	let respectful = WalkBuilder::new(root.as_std_path())
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.overrides(build_overrides(root))
		.build();

	let mut walked_dirs: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
	walked_dirs.insert(root.as_std_path().to_path_buf());

	let mut visits = 0usize;
	for entry in respectful.flatten() {
		visits = visits.saturating_add(1);
		if visits > FILE_SEARCH_MAX_VISITS {
			break;
		}
		let path = entry.path();
		if path.is_dir() {
			walked_dirs.insert(path.to_path_buf());
			continue;
		}
		if !path.is_file() {
			continue;
		}
		score_and_collect(root, path, &query, &mut hits, &mut seen);
	}

	if visits <= FILE_SEARCH_MAX_VISITS {
		// Pass 2 — same walker shape, but with `.gitignore` /
		// `.ignore` switched **off** so files like `.env` /
		// `coverage.xml` / `dist-types.d.ts` show up. The
		// `filter_entry` predicate clamps the walk to directories
		// pass 1 already entered, so we never descend into
		// `node_modules/` (gitignored) even though its parent (the
		// workspace root) was walked. Files directly under the
		// root land in `walked_dirs` via the parent check, so a
		// gitignored top-level file is reachable without dragging
		// the gitignored tree it'd otherwise hide behind.
		let walked_for_filter = walked_dirs.clone();
		let inclusive = WalkBuilder::new(root.as_std_path())
			.hidden(false)
			.git_ignore(false)
			.git_exclude(false)
			.ignore(false)
			.overrides(build_overrides(root))
			.filter_entry(move |entry| {
				let Some(ft) = entry.file_type() else {
					return true;
				};
				if !ft.is_dir() {
					return true;
				}
				walked_for_filter.contains(entry.path())
			})
			.build();

		for entry in inclusive.flatten() {
			visits = visits.saturating_add(1);
			if visits > FILE_SEARCH_MAX_VISITS {
				break;
			}
			let path = entry.path();
			if !path.is_file() {
				continue;
			}
			// Belt-and-braces — `filter_entry` already pruned
			// gitignored subtrees, but check the parent against
			// the allow-list too in case `filter_entry` ever lets
			// a file through whose containing dir wasn't walked.
			let Some(parent) = path.parent() else {
				continue;
			};
			if !walked_dirs.contains(parent) {
				continue;
			}
			score_and_collect(root, path, &query, &mut hits, &mut seen);
		}
	}

	hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.len().cmp(&b.path.len())));
	hits.truncate(limit);
	Ok(hits)
}

/// Score a single file entry and push it onto `hits` if it scores
/// above the cutoff. `seen` dedupes across the two-pass walk —
/// pass 1 always yields tracked files first, so the pass-2 path
/// never overwrites a higher score with an identical one.
fn score_and_collect(
	root: &Utf8Path,
	path: &std::path::Path,
	query: &str,
	hits: &mut Vec<FileSearchResult>,
	seen: &mut std::collections::HashSet<String>,
) {
	let Ok(rel) = path.strip_prefix(root.as_std_path()) else {
		return;
	};
	let Some(rel_str) = rel.to_str() else {
		return;
	};
	let score = score_file(rel_str, query);
	if score <= FILE_SEARCH_MIN_SCORE {
		return;
	}
	if !seen.insert(rel_str.to_string()) {
		return;
	}
	hits.push(FileSearchResult {
		path: rel_str.to_string(),
		score,
	});
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

	let include_filter = build_include_filter(root, opts.include_glob.as_deref());
	let mut walker_builder = WalkBuilder::new(root.as_std_path());
	walker_builder
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.overrides(build_overrides(root));
	if let Some(filter) = include_filter {
		walker_builder.filter_entry(move |entry| include_filter_keeps(&filter, entry));
	}
	let walker = walker_builder.build();

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

	let include_filter = build_include_filter(root, opts.include_glob.as_deref());
	let mut walker_builder = WalkBuilder::new(root.as_std_path());
	walker_builder
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.overrides(build_overrides(root));
	if let Some(filter) = include_filter {
		walker_builder.filter_entry(move |entry| include_filter_keeps(&filter, entry));
	}
	let walker = walker_builder.build();

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
	fn file_search_surfaces_gitignored_top_level_files() {
		// `.env` (and friends) are routinely gitignored but the
		// user still needs to reach them through Ctrl+P. The
		// two-pass walker lets the file through while the rest of
		// the gitignore stack (notably `node_modules/`) stays
		// suppressed.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".gitignore"), ".env\nnode_modules/\n").unwrap();
		std::fs::write(dir.path().join(".env"), "SECRET=hunter2\n").unwrap();
		std::fs::create_dir_all(dir.path().join("node_modules/lodash")).unwrap();
		std::fs::write(dir.path().join("node_modules/lodash/.env"), "decoy\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "real\n").unwrap();

		let opts = FileSearchOptions {
			query: "env".into(),
			limit: 50,
		};
		let hits = search_files(&root(&dir), &opts).unwrap();
		assert!(
			hits.iter().any(|h| h.path == ".env"),
			"top-level .env should surface: {hits:?}"
		);
		assert!(
			hits.iter().all(|h| !h.path.starts_with("node_modules/")),
			"node_modules subtree must stay suppressed: {hits:?}"
		);
	}

	#[test]
	fn file_search_does_not_descend_into_gitignored_directories() {
		// The two-pass walker still walks the *root* in pass 2,
		// but the `filter_entry` predicate prunes the gitignored
		// `target/` directory before its files reach the scorer.
		// Without this, `cargo`-shaped repos would drown in
		// `target/debug/...` entries.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
		std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
		std::fs::write(dir.path().join("target/debug/needle.txt"), "ignored leaf\n").unwrap();
		std::fs::write(dir.path().join("needle.txt"), "real\n").unwrap();

		let opts = FileSearchOptions {
			query: "needle".into(),
			limit: 50,
		};
		let hits = search_files(&root(&dir), &opts).unwrap();
		assert!(
			hits.iter().all(|h| !h.path.starts_with("target/")),
			"target subtree must stay suppressed: {hits:?}"
		);
		assert!(
			hits.iter().any(|h| h.path == "needle.txt"),
			"sibling tracked file must still surface: {hits:?}"
		);
	}

	#[test]
	fn file_search_deduplicates_across_two_passes() {
		// A tracked file shows up in pass 1 and could theoretically
		// surface again in pass 2; the `seen` set guarantees one
		// entry per relative path. Regression against a future
		// refactor accidentally collecting both passes naively.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".gitignore"), "secret/\n").unwrap();
		std::fs::write(dir.path().join("widget.ts"), "tracked\n").unwrap();

		let opts = FileSearchOptions {
			query: "widget".into(),
			limit: 50,
		};
		let hits = search_files(&root(&dir), &opts).unwrap();
		assert_eq!(hits.iter().filter(|h| h.path == "widget.ts").count(), 1);
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
	fn content_search_skips_worktrees_dir_via_git_exclude() {
		// Worktree-backed coder sessions (ADR 0029) check branches out
		// at `<parent>/.worktrees/<slug>` and hide that directory via
		// a `/.worktrees/` line in the parent's `.git/info/exclude`.
		// A parent-rooted search must not descend into the worktrees —
		// every hit would otherwise show up twice (once per checkout).
		// Wired via `WalkBuilder::git_exclude(true)`; regression
		// against flipping it off or the override set eating it.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git/info")).unwrap();
		std::fs::write(dir.path().join(".git/info/exclude"), "/.worktrees/\n").unwrap();
		std::fs::create_dir_all(dir.path().join(".worktrees/moon-agent-1")).unwrap();
		std::fs::write(
			dir.path().join(".worktrees/moon-agent-1/dupe.txt"),
			"needle in worktree\n",
		)
		.unwrap();
		std::fs::write(dir.path().join("dupe.txt"), "needle in parent\n").unwrap();

		let opts = ContentSearchOptions {
			query: "needle".into(),
			max_matches: 100,
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert!(
			r.hits.iter().all(|h| !h.path.starts_with(".worktrees/")),
			"content search leaked into .worktrees/: {:?}",
			r.hits
		);
		assert!(
			r.hits.iter().any(|h| h.path == "dupe.txt"),
			"parent checkout hit got dropped along with the worktrees filter: {:?}",
			r.hits
		);
	}

	#[test]
	fn file_search_skips_worktrees_dir_via_git_exclude() {
		// Same invariant as the content-search test above, but for the
		// two-pass quick-open walker: pass 2 turns `git_exclude` *off*
		// to surface files like `.env`, and relies on the walked-dirs
		// allow-list to keep excluded directories pruned. A regression
		// there would double every file name in the palette (parent
		// copy + worktree copy).
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git/info")).unwrap();
		std::fs::write(dir.path().join(".git/info/exclude"), "/.worktrees/\n").unwrap();
		std::fs::create_dir_all(dir.path().join(".worktrees/moon-agent-1")).unwrap();
		std::fs::write(dir.path().join(".worktrees/moon-agent-1/widget.ts"), "").unwrap();
		std::fs::write(dir.path().join("widget.ts"), "").unwrap();

		let opts = FileSearchOptions {
			query: "widget".into(),
			limit: 50,
		};
		let hits = search_files(&root(&dir), &opts).unwrap();
		assert!(
			hits.iter().all(|h| !h.path.starts_with(".worktrees/")),
			"file search leaked into .worktrees/: {hits:?}"
		);
		assert_eq!(
			hits.iter().filter(|h| h.path == "widget.ts").count(),
			1,
			"parent copy should surface exactly once: {hits:?}"
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
	fn content_search_include_glob_still_respects_gitignore() {
		// Regression: a bare-path scope like `packages/hub` used
		// to be expanded into a positive override glob, which
		// per the `ignore` crate's precedence rules whitelists
		// the matched path and **short-circuits gitignore**. The
		// search then dragged in `packages/hub/node_modules/`
		// even though the root `.gitignore` had `node_modules/`.
		// The fix moves include filtering off the override
		// matcher and into `filter_entry`, so gitignore pruning
		// still happens upstream.
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
		std::fs::create_dir_all(dir.path().join("packages/hub/src")).unwrap();
		std::fs::create_dir_all(dir.path().join("packages/hub/node_modules/lodash")).unwrap();
		std::fs::write(dir.path().join("packages/hub/src/index.ts"), "needle in src\n").unwrap();
		std::fs::write(
			dir.path().join("packages/hub/node_modules/lodash/index.js"),
			"needle in deps\n",
		)
		.unwrap();

		let opts = ContentSearchOptions {
			query: "needle".into(),
			include_glob: Some("packages/hub".into()),
			max_matches: 100,
			..Default::default()
		};
		let r = search_content(&root(&dir), &opts).unwrap();
		assert!(
			r.hits.iter().all(|h| !h.path.contains("node_modules/")),
			"scoped search leaked into gitignored node_modules/: {:?}",
			r.hits
		);
		assert!(
			r.hits.iter().any(|h| h.path == "packages/hub/src/index.ts"),
			"scoped search dropped the legitimate hit: {:?}",
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
	fn replace_content_include_glob_still_respects_gitignore() {
		// Mirror of `content_search_include_glob_still_respects_gitignore`:
		// a scoped replace must not rewrite files under a
		// gitignored subdirectory just because the user typed a
		// bare-path scope. (The original bug was caused by the
		// override matcher whitelisting paths and bypassing
		// gitignore.)
		let dir = TempDir::new().unwrap();
		std::fs::create_dir_all(dir.path().join(".git")).unwrap();
		std::fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
		std::fs::create_dir_all(dir.path().join("packages/hub/src")).unwrap();
		std::fs::create_dir_all(dir.path().join("packages/hub/node_modules/lodash")).unwrap();
		std::fs::write(dir.path().join("packages/hub/src/index.ts"), "todo\n").unwrap();
		std::fs::write(dir.path().join("packages/hub/node_modules/lodash/index.js"), "todo\n").unwrap();

		let opts = ContentReplaceOptions {
			query: "todo".into(),
			replacement: "done".into(),
			include_glob: Some("packages/hub".into()),
			..Default::default()
		};
		let r = replace_content(&root(&dir), &opts).unwrap();
		assert_eq!(r.files_changed, 1);
		assert_eq!(
			std::fs::read_to_string(dir.path().join("packages/hub/src/index.ts")).unwrap(),
			"done\n"
		);
		assert_eq!(
			std::fs::read_to_string(dir.path().join("packages/hub/node_modules/lodash/index.js")).unwrap(),
			"todo\n",
			"replace leaked into gitignored node_modules/"
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
