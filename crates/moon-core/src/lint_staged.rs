//! Resolved lint-staged map per file.
//!
//! Walks from the file's directory upward to the workspace root looking
//! for `.lintstagedrc.json`, then a `package.json` containing a top-level
//! `lint-staged` object. The closest hit wins; we don't merge across
//! levels (matches lint-staged's own cosmiconfig walk). Resolution is
//! cached per directory because every file in the same directory walks
//! the same chain.
//!
//! Cache invalidation is save-driven: `LocalHost::write_file` clears the
//! cache when the saved file's basename is `.lintstagedrc.json` or
//! `package.json`. There is no filesystem watcher in Phase 1.5; one
//! ships with Phase 5's git integration. External edits pick up on the
//! next moon-ide restart.
//!
//! See [specs/decisions/0012-format-on-save.md](../../../specs/decisions/0012-format-on-save.md).

use camino::{Utf8Path, Utf8PathBuf};
use globset::{Glob, GlobMatcher};
use moon_protocol::{MoonError, MoonResult};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Compiled set of `(glob, command)` pairs from a single lint-staged
/// config file. Cheap to clone (one `Arc` bump) so cache hits don't pay
/// for re-parsing.
#[derive(Default, Clone)]
pub struct LintStagedRules {
	/// Directory the config file was found in. `None` for the empty
	/// "no config anywhere" case. Patterns containing `/` resolve
	/// against this directory — same anchoring lint-staged itself
	/// applies via `path.join(cwd, pattern)`.
	config_dir: Option<Utf8PathBuf>,
	rules: Arc<Vec<LintStagedRule>>,
}

struct LintStagedRule {
	/// The original pattern, kept for diagnostics.
	pattern: String,
	matcher: GlobMatcher,
	/// micromatch's `matchBase` switches on whether the pattern contains
	/// a path separator. lint-staged inherits that behaviour, so `*.ts`
	/// matches `foo/bar.ts` but `src/*.ts` only matches files directly
	/// inside `src/`. We mirror it here.
	has_separator: bool,
	commands: Vec<String>,
}

impl LintStagedRules {
	/// First command of the first matching rule for `abs_path`. Returns
	/// `None` when nothing matches; callers treat that as "no formatter
	/// configured for this file" rather than an error.
	pub fn match_command(&self, abs_path: &Path) -> Option<&str> {
		let rel = self
			.config_dir
			.as_ref()
			.and_then(|cd| abs_path.strip_prefix(cd.as_std_path()).ok());
		for rule in self.rules.iter() {
			let hit = if rule.has_separator {
				match &rel {
					Some(r) => rule.matcher.is_match(r),
					None => continue,
				}
			} else {
				let Some(name) = abs_path.file_name() else {
					continue;
				};
				rule.matcher.is_match(name)
			};
			if !hit {
				continue;
			}
			if rule.commands.len() > 1 {
				// lint-staged supports chains (`["eslint --fix", "prettier
				// --write"]`); for our one-input-one-output text pipeline
				// only the first command runs. Documented in ADR 0012.
				tracing::warn!(
					pattern = %rule.pattern,
					count = rule.commands.len(),
					"format-on-save: only the first command in a lint-staged chain runs",
				);
			}
			return rule.commands.first().map(String::as_str);
		}
		None
	}

	pub fn is_empty(&self) -> bool {
		self.rules.is_empty()
	}

	/// Directory the matched config file lives in. `None` for the empty
	/// "no config" case. Callers run the formatter subprocess with this
	/// as its `cwd` so relative arguments inside the lint-staged command
	/// (`--ignore-path ../.prettierignore`, `--config ./prettier.cjs`,
	/// etc.) resolve from the same directory lint-staged itself uses.
	pub fn config_dir(&self) -> Option<&Utf8Path> {
		self.config_dir.as_deref()
	}
}

#[derive(Default)]
pub struct LintStagedService {
	root: Utf8PathBuf,
	cache: RwLock<HashMap<Utf8PathBuf, LintStagedRules>>,
}

impl LintStagedService {
	pub fn new(root: Utf8PathBuf) -> Self {
		Self {
			root,
			cache: RwLock::new(HashMap::new()),
		}
	}

	/// Effective rules for `rel`. `rel` is workspace-relative; absolute
	/// paths are accepted and assumed to live under the workspace root.
	/// Returns an empty `LintStagedRules` (rather than an error) when no
	/// supported config is found, because "no formatter for this file"
	/// is a perfectly valid state.
	pub async fn for_path(&self, rel: &Utf8Path) -> MoonResult<LintStagedRules> {
		let dir = rel.parent().unwrap_or_else(|| Utf8Path::new("")).to_path_buf();

		if let Some(rules) = self.cache.read().await.get(&dir).cloned() {
			return Ok(rules);
		}

		let abs_dir = if dir.is_absolute() {
			dir.clone()
		} else {
			self.root.join(&dir)
		};
		let root = self.root.clone();

		let rules = tokio::task::spawn_blocking(move || resolve(&abs_dir, &root))
			.await
			.map_err(|e| MoonError::Internal(format!("lint-staged task: {e}")))?;

		self.cache.write().await.insert(dir, rules.clone());
		Ok(rules)
	}

	/// Drop every cached entry. Mirrors `EditorConfigService::clear`;
	/// hosts call it when a `.lintstagedrc.json` or `package.json` is
	/// saved through moon-ide.
	pub async fn clear(&self) {
		self.cache.write().await.clear();
	}
}

fn resolve(start: &Utf8Path, root: &Utf8Path) -> LintStagedRules {
	let mut current: Option<&Utf8Path> = Some(start);
	while let Some(dir) = current {
		if let Some(rules) = try_load_dir(dir) {
			return rules;
		}
		if dir == root {
			break;
		}
		current = dir.parent();
	}
	LintStagedRules::default()
}

fn try_load_dir(dir: &Utf8Path) -> Option<LintStagedRules> {
	let lintstagedrc = dir.join(".lintstagedrc.json");
	if lintstagedrc.exists() {
		return Some(load_json_file(lintstagedrc.as_std_path(), dir).unwrap_or_else(|err| {
			tracing::warn!(path = %lintstagedrc, %err, "format-on-save: failed to parse .lintstagedrc.json; skipping");
			LintStagedRules::default()
		}));
	}

	let pkg = dir.join("package.json");
	if pkg.exists() {
		match load_package_json(pkg.as_std_path(), dir) {
			Ok(Some(rules)) => return Some(rules),
			Ok(None) => {}
			Err(err) => {
				tracing::warn!(path = %pkg, %err, "format-on-save: failed to parse package.json#lint-staged; skipping");
				return Some(LintStagedRules::default());
			}
		}
	}

	// Surface unsupported variants once per directory so the user knows
	// lint-staged would honour them but moon-ide doesn't. Not a hard
	// stop — a JSON config higher in the tree is still useful.
	for unsupported in [
		".lintstagedrc.js",
		".lintstagedrc.cjs",
		".lintstagedrc.mjs",
		".lintstagedrc.yaml",
		".lintstagedrc.yml",
		".lintstagedrc",
	] {
		let path = dir.join(unsupported);
		if path.exists() {
			tracing::warn!(
				path = %path,
				"format-on-save: lint-staged config variant is unsupported (JSON only); skipping",
			);
		}
	}

	None
}

fn load_json_file(path: &Path, config_dir: &Utf8Path) -> Result<LintStagedRules, String> {
	let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
	let value: Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
	parse_value(&value, config_dir)
}

fn load_package_json(path: &Path, config_dir: &Utf8Path) -> Result<Option<LintStagedRules>, String> {
	let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
	let value: Value = serde_json::from_str(&text).map_err(|e| format!("parse: {e}"))?;
	let Some(map) = value.get("lint-staged") else {
		return Ok(None);
	};
	parse_value(map, config_dir).map(Some)
}

fn parse_value(value: &Value, config_dir: &Utf8Path) -> Result<LintStagedRules, String> {
	let map = value
		.as_object()
		.ok_or_else(|| "lint-staged config must be an object".to_owned())?;
	let mut rules = Vec::with_capacity(map.len());
	for (pattern, cmds) in map {
		let commands: Vec<String> = match cmds {
			Value::String(s) => vec![s.clone()],
			Value::Array(arr) => arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
			_ => continue,
		};
		if commands.is_empty() {
			continue;
		}
		let glob = match Glob::new(pattern) {
			Ok(g) => g,
			Err(err) => {
				tracing::warn!(pattern = %pattern, %err, "format-on-save: invalid glob; skipping");
				continue;
			}
		};
		rules.push(LintStagedRule {
			has_separator: pattern.contains('/'),
			matcher: glob.compile_matcher(),
			pattern: pattern.clone(),
			commands,
		});
	}
	Ok(LintStagedRules {
		config_dir: Some(config_dir.to_path_buf()),
		rules: Arc::new(rules),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempfile::TempDir;

	fn write(dir: &TempDir, rel: &str, contents: &str) {
		let path = dir.path().join(rel);
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent).unwrap();
		}
		std::fs::write(path, contents).unwrap();
	}

	fn service(dir: &TempDir) -> LintStagedService {
		let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
		LintStagedService::new(root)
	}

	fn abs(dir: &TempDir, rel: &str) -> std::path::PathBuf {
		dir.path().canonicalize().unwrap().join(rel)
	}

	#[tokio::test]
	async fn empty_when_no_config() {
		let dir = TempDir::new().unwrap();
		write(&dir, "src/lib.rs", "");
		let svc = service(&dir);
		let rules = svc.for_path(Utf8Path::new("src/lib.rs")).await.unwrap();
		assert!(rules.is_empty());
		assert!(rules.match_command(&abs(&dir, "src/lib.rs")).is_none());
	}

	#[tokio::test]
	async fn lintstagedrc_basename_match() {
		let dir = TempDir::new().unwrap();
		write(
			&dir,
			".lintstagedrc.json",
			r#"{
				"*.{ts,tsx,js}": "oxfmt",
				"*.svelte": "prettier --write",
				"*.rs": "rustfmt --edition 2021"
			}"#,
		);
		write(&dir, "src/deep/foo.ts", "");
		write(&dir, "src/App.svelte", "");
		write(&dir, "Cargo.toml", "");
		let svc = service(&dir);

		let ts = svc.for_path(Utf8Path::new("src/deep/foo.ts")).await.unwrap();
		assert_eq!(ts.match_command(&abs(&dir, "src/deep/foo.ts")), Some("oxfmt"));

		let svelte = svc.for_path(Utf8Path::new("src/App.svelte")).await.unwrap();
		assert_eq!(
			svelte.match_command(&abs(&dir, "src/App.svelte")),
			Some("prettier --write"),
		);

		let toml = svc.for_path(Utf8Path::new("Cargo.toml")).await.unwrap();
		assert!(toml.match_command(&abs(&dir, "Cargo.toml")).is_none());
	}

	#[tokio::test]
	async fn package_json_lint_staged_field() {
		let dir = TempDir::new().unwrap();
		write(
			&dir,
			"package.json",
			r#"{
				"name": "x",
				"lint-staged": {
					"*.ts": "oxfmt"
				}
			}"#,
		);
		write(&dir, "a.ts", "");
		let svc = service(&dir);
		let rules = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert_eq!(rules.match_command(&abs(&dir, "a.ts")), Some("oxfmt"));
	}

	#[tokio::test]
	async fn lintstagedrc_wins_over_package_json() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".lintstagedrc.json", r#"{ "*.ts": "from-rc" }"#);
		write(
			&dir,
			"package.json",
			r#"{ "name": "x", "lint-staged": { "*.ts": "from-pkg" } }"#,
		);
		write(&dir, "a.ts", "");
		let svc = service(&dir);
		let rules = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert_eq!(rules.match_command(&abs(&dir, "a.ts")), Some("from-rc"));
	}

	#[tokio::test]
	async fn closest_dir_wins_no_merge() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".lintstagedrc.json", r#"{ "*.ts": "outer" }"#);
		write(&dir, "nested/.lintstagedrc.json", r#"{ "*.js": "inner" }"#);
		write(&dir, "nested/a.ts", "");
		write(&dir, "nested/b.js", "");

		let svc = service(&dir);
		// For nested/*.ts the inner wins by virtue of being closest, even
		// though it doesn't list `*.ts` — there's no merge across levels.
		let ts_rules = svc.for_path(Utf8Path::new("nested/a.ts")).await.unwrap();
		assert!(ts_rules.match_command(&abs(&dir, "nested/a.ts")).is_none());

		let js_rules = svc.for_path(Utf8Path::new("nested/b.js")).await.unwrap();
		assert_eq!(js_rules.match_command(&abs(&dir, "nested/b.js")), Some("inner"));
	}

	#[tokio::test]
	async fn separator_pattern_anchors_to_path() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".lintstagedrc.json", r#"{ "src/*.ts": "oxfmt" }"#);
		write(&dir, "src/a.ts", "");
		write(&dir, "other/b.ts", "");
		let svc = service(&dir);

		let src = svc.for_path(Utf8Path::new("src/a.ts")).await.unwrap();
		assert_eq!(src.match_command(&abs(&dir, "src/a.ts")), Some("oxfmt"));

		let other = svc.for_path(Utf8Path::new("other/b.ts")).await.unwrap();
		assert!(other.match_command(&abs(&dir, "other/b.ts")).is_none());
	}

	#[tokio::test]
	async fn array_command_first_wins() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".lintstagedrc.json", r#"{ "*.ts": ["oxfmt", "echo done"] }"#);
		write(&dir, "a.ts", "");
		let svc = service(&dir);
		let rules = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert_eq!(rules.match_command(&abs(&dir, "a.ts")), Some("oxfmt"));
	}

	#[tokio::test]
	async fn cache_clears() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".lintstagedrc.json", r#"{ "*.ts": "first" }"#);
		write(&dir, "a.ts", "");
		let svc = service(&dir);
		let one = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert_eq!(one.match_command(&abs(&dir, "a.ts")), Some("first"));

		write(&dir, ".lintstagedrc.json", r#"{ "*.ts": "second" }"#);
		let cached = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert_eq!(cached.match_command(&abs(&dir, "a.ts")), Some("first"));

		svc.clear().await;
		let fresh = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert_eq!(fresh.match_command(&abs(&dir, "a.ts")), Some("second"));
	}

	#[tokio::test]
	async fn malformed_json_yields_empty_rules() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".lintstagedrc.json", "not json");
		write(&dir, "a.ts", "");
		let svc = service(&dir);
		let rules = svc.for_path(Utf8Path::new("a.ts")).await.unwrap();
		assert!(rules.is_empty());
	}
}
