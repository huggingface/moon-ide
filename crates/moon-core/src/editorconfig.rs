//! Resolved EditorConfig per file.
//!
//! Walks `.editorconfig` from the file up to the workspace root via
//! `ec4rs`, then overlays moon-ide defaults so an empty workspace still
//! gets sensible behaviour. Resolution is cached per directory because
//! every file in the same directory resolves to the same effective
//! config.
//!
//! Cache invalidation is save-driven: `LocalHost::write_file` clears
//! the cache when the saved file is named `.editorconfig`. There is no
//! filesystem watcher in Phase 1.5; one ships with Phase 5's git
//! integration. External edits (someone else's editor, `git pull`)
//! pick up on the next moon-ide restart.
//!
//! See [specs/editorconfig.md](../../../specs/editorconfig.md).

use camino::{Utf8Path, Utf8PathBuf};
use ec4rs::property as ec_prop;
use moon_protocol::editorconfig::{EditorConfig, EndOfLine, IndentStyle};
use moon_protocol::{MoonError, MoonResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

#[derive(Default)]
pub struct EditorConfigService {
	root: Utf8PathBuf,
	cache: RwLock<HashMap<Utf8PathBuf, EditorConfig>>,
}

impl EditorConfigService {
	pub fn new(root: Utf8PathBuf) -> Self {
		Self {
			root,
			cache: RwLock::new(HashMap::new()),
		}
	}

	/// Effective config for `rel`. `rel` is workspace-relative; absolute
	/// paths are accepted and assumed to live under the workspace root.
	pub async fn for_path(&self, rel: &Utf8Path) -> MoonResult<EditorConfig> {
		let dir = rel.parent().unwrap_or_else(|| Utf8Path::new("")).to_path_buf();

		if let Some(ec) = self.cache.read().await.get(&dir).cloned() {
			return Ok(ec);
		}

		let abs = if rel.is_absolute() {
			PathBuf::from(rel.as_str())
		} else {
			PathBuf::from(self.root.join(rel).as_str())
		};

		let ec = tokio::task::spawn_blocking(move || resolve(&abs))
			.await
			.map_err(|e| MoonError::Internal(format!("editorconfig task: {e}")))?;

		self.cache.write().await.insert(dir, ec.clone());
		Ok(ec)
	}

	/// Drop every cached entry. Cheap because resolution is fast and
	/// the cache fills back lazily as files are touched.
	pub async fn clear(&self) {
		self.cache.write().await.clear();
	}
}

fn resolve(abs: &Path) -> EditorConfig {
	let mut ec = EditorConfig::default();
	let Ok(props) = ec4rs::properties_of(abs) else {
		return ec;
	};

	if let Ok(style) = props.get::<ec_prop::IndentStyle>() {
		ec.indent_style = match style {
			ec_prop::IndentStyle::Tabs => IndentStyle::Tab,
			ec_prop::IndentStyle::Spaces => IndentStyle::Space,
		};
	}

	let tab_width_explicit = match props.get::<ec_prop::TabWidth>() {
		Ok(ec_prop::TabWidth::Value(v)) => u32::try_from(v).ok(),
		Err(_) => None,
	};
	if let Some(v) = tab_width_explicit {
		ec.tab_width = v;
	}

	// indent_size / tab_width cascade per the EditorConfig spec:
	//   - explicit indent_size = N → indent_size = N. If tab_width is
	//     unset, tab_width also = N.
	//   - indent_size = tab → indent_size tracks tab_width.
	//   - unset + indent_style = tab → indent_size = tab_width.
	// Mirrors `ec4rs::Properties::use_fallbacks` but keeps everything
	// as u32 and defers to our own defaults instead of `ec4rs`'s.
	match props.get::<ec_prop::IndentSize>() {
		Ok(ec_prop::IndentSize::Value(v)) => {
			if let Ok(v) = u32::try_from(v) {
				ec.indent_size = v;
				if tab_width_explicit.is_none() {
					ec.tab_width = v;
				}
			}
		}
		Ok(ec_prop::IndentSize::UseTabWidth) => {
			ec.indent_size = ec.tab_width;
		}
		Err(_) => {
			if matches!(ec.indent_style, IndentStyle::Tab) {
				ec.indent_size = ec.tab_width;
			}
		}
	}

	if let Ok(eol) = props.get::<ec_prop::EndOfLine>() {
		ec.end_of_line = Some(match eol {
			ec_prop::EndOfLine::Lf => EndOfLine::Lf,
			ec_prop::EndOfLine::CrLf => EndOfLine::Crlf,
			ec_prop::EndOfLine::Cr => EndOfLine::Cr,
		});
	}

	if let Ok(ec_prop::FinalNewline::Value(v)) = props.get::<ec_prop::FinalNewline>() {
		ec.insert_final_newline = v;
	}

	if let Ok(ec_prop::TrimTrailingWs::Value(v)) = props.get::<ec_prop::TrimTrailingWs>() {
		ec.trim_trailing_whitespace = v;
	}

	if let Ok(charset) = props.get::<ec_prop::Charset>() {
		ec.charset = match charset {
			ec_prop::Charset::Utf8 => "utf-8",
			ec_prop::Charset::Latin1 => "latin1",
			ec_prop::Charset::Utf16Le => "utf-16le",
			ec_prop::Charset::Utf16Be => "utf-16be",
			ec_prop::Charset::Utf8Bom => "utf-8-bom",
		}
		.to_owned();
	}

	if let Ok(max) = props.get::<ec_prop::MaxLineLen>() {
		ec.max_line_length = match max {
			ec_prop::MaxLineLen::Value(v) => u32::try_from(v).ok(),
			ec_prop::MaxLineLen::Off => None,
		};
	}

	ec
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

	fn service(dir: &TempDir) -> EditorConfigService {
		let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
		EditorConfigService::new(root)
	}

	#[tokio::test]
	async fn defaults_when_no_editorconfig() {
		let dir = TempDir::new().unwrap();
		write(&dir, "src/lib.rs", "");
		let svc = service(&dir);
		let ec = svc.for_path(Utf8Path::new("src/lib.rs")).await.unwrap();
		assert_eq!(ec, EditorConfig::default());
	}

	#[tokio::test]
	async fn root_editorconfig_wins() {
		let dir = TempDir::new().unwrap();
		write(
			&dir,
			".editorconfig",
			"root = true\n[*]\nindent_style = space\nindent_size = 4\n",
		);
		write(&dir, "src/lib.rs", "");
		let svc = service(&dir);
		let ec = svc.for_path(Utf8Path::new("src/lib.rs")).await.unwrap();
		assert_eq!(ec.indent_style, IndentStyle::Space);
		assert_eq!(ec.indent_size, 4);
		assert_eq!(ec.tab_width, 4);
	}

	#[tokio::test]
	async fn nested_section_overrides_glob() {
		let dir = TempDir::new().unwrap();
		write(
			&dir,
			".editorconfig",
			"root = true\n[*]\nindent_style = tab\nindent_size = 2\n[*.md]\nindent_style = space\nindent_size = 4\n",
		);
		write(&dir, "README.md", "");
		write(&dir, "src/lib.rs", "");
		let svc = service(&dir);
		let md = svc.for_path(Utf8Path::new("README.md")).await.unwrap();
		assert_eq!(md.indent_style, IndentStyle::Space);
		assert_eq!(md.indent_size, 4);
		let rs = svc.for_path(Utf8Path::new("src/lib.rs")).await.unwrap();
		assert_eq!(rs.indent_style, IndentStyle::Tab);
		assert_eq!(rs.tab_width, 2);
	}

	#[tokio::test]
	async fn cache_clears() {
		let dir = TempDir::new().unwrap();
		write(&dir, ".editorconfig", "root = true\n[*]\nindent_size = 2\n");
		write(&dir, "a.txt", "");
		let svc = service(&dir);
		let first = svc.for_path(Utf8Path::new("a.txt")).await.unwrap();
		assert_eq!(first.indent_size, 2);

		write(&dir, ".editorconfig", "root = true\n[*]\nindent_size = 8\n");

		// Without invalidation we still see the cached value.
		let cached = svc.for_path(Utf8Path::new("a.txt")).await.unwrap();
		assert_eq!(cached.indent_size, 2);

		svc.clear().await;
		let fresh = svc.for_path(Utf8Path::new("a.txt")).await.unwrap();
		assert_eq!(fresh.indent_size, 8);
	}
}
