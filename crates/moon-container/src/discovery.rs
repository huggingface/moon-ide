//! Compose-file discovery inside a workspace.
//!
//! moon-ide's "command centre" model is one container per
//! workspace, with the project's own services brought up as
//! siblings via compose `include:` (see ADR 0008). To make that
//! one-click, we need to find the existing compose files
//! automatically — anything moon-landing-shaped (a workspace
//! folder that contains one or more project subdirectories, each
//! with its own `docker-compose.yml`) should Just Work.
//!
//! Scope of the scan
//! -----------------
//!
//! Per bound folder:
//!
//! - The folder root.
//! - Each immediate child directory (depth = 1).
//!
//! That's it. We deliberately do **not** recurse — `node_modules`
//! and `target` would be expensive to walk, and a deeply nested
//! compose file isn't something moon-ide should auto-include
//! anyway: if a user genuinely wants a sub-sub-directory's compose,
//! they can put a top-level `compose.yaml` in their folder that
//! `include:`s it.
//!
//! Multiple bound folders are scanned independently; results are
//! merged in folder-input order, with intra-folder ordering
//! sorted by path. See [`discover_compose_files_for_folders`].
//!
//! What we recognise
//! -----------------
//!
//! The four filenames docker compose uses:
//!
//! | Filename               | Precedence |
//! | ---------------------- | ---------- |
//! | `compose.yaml`         | 1 (highest) |
//! | `compose.yml`          | 2 |
//! | `docker-compose.yaml`  | 3 |
//! | `docker-compose.yml`   | 4 (lowest)  |
//!
//! Override files (`compose.override.yaml`, `compose.dev.yaml`,
//! etc.) are intentionally not picked up — compose itself only
//! auto-loads them when explicitly told to, and we follow the
//! same rule. Per directory we keep one file: the highest-
//! precedence match wins.
//!
//! What we skip
//! ------------
//!
//! - Hidden directories (`.git`, `.cache`, `.cargo`, …) — by
//!   leading-dot convention.
//! - Conventional build artefact directories (`node_modules`,
//!   `target`, `dist`, `build`, `.venv`, …).
//! - Symlinked directories — they can re-enter the workspace
//!   tree, and we don't want to chase loops in a one-shot scan.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};

/// One compose file discovered inside a bound folder.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiscoveredCompose {
	/// Absolute, canonicalised path to the compose file. The
	/// generated `compose.yaml` consumes this verbatim when
	/// building `include:` entries — see [`crate::compose`].
	pub absolute_path: Utf8PathBuf,
	/// Path relative to the folder it was discovered under
	/// (forward slashes). Useful for UI display ("found
	/// `services/api/docker-compose.yml`") but not for include
	/// generation, which uses absolute paths.
	pub relative_path: Utf8PathBuf,
}

/// Result of [`discover_compose_files`].
///
/// The struct shape (rather than a bare `Vec`) is forward-looking:
/// we'll grow per-file metadata (declared services, port forwards,
/// `x-moon` annotations) into this without breaking callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ComposeDiscovery {
	pub files: Vec<DiscoveredCompose>,
}

/// Walk `folder_root` (depth ≤ 1) and return every recognised
/// compose file. Missing or non-directory inputs return an empty
/// discovery — opening a file or a non-existent path is a
/// degenerate case the UI should surface elsewhere, not an error
/// here.
///
/// Output is sorted by `relative_path`, so callers can rely on
/// stable ordering for diffing the generated `compose.yaml`.
pub fn discover_compose_files(folder_root: &Utf8Path) -> ComposeDiscovery {
	let mut by_dir: BTreeMap<Utf8PathBuf, FoundFile> = BTreeMap::new();

	scan_dir(folder_root, folder_root, &mut by_dir);
	for child in immediate_subdirs(folder_root) {
		scan_dir(&child, folder_root, &mut by_dir);
	}

	let files = by_dir
		.into_values()
		.map(|found| DiscoveredCompose {
			absolute_path: found.absolute,
			relative_path: found.relative,
		})
		.collect();

	ComposeDiscovery { files }
}

/// Find the single compose file directly at `folder_root` (no
/// subdirectory recursion).
///
/// Used by the per-folder lifecycle ([`crate::project_compose`]):
/// a folder gets at most one "primary" compose project, anchored
/// at its root. Anything in `subdir/docker-compose.yml` is
/// ignored here — if a user wants to manage a nested compose,
/// they can bind that nested directory as its own workspace
/// folder.
pub fn discover_root_compose(folder_root: &Utf8Path) -> Option<DiscoveredCompose> {
	let mut by_dir: BTreeMap<Utf8PathBuf, FoundFile> = BTreeMap::new();
	scan_dir(folder_root, folder_root, &mut by_dir);
	by_dir.into_values().next().map(|found| DiscoveredCompose {
		absolute_path: found.absolute,
		relative_path: found.relative,
	})
}

/// Scan every bound folder and concatenate their discoveries.
///
/// Per-folder ordering is preserved (so the resulting `include:`
/// list mirrors the order folders were added to the workspace);
/// intra-folder ordering remains sorted by relative path.
/// Duplicates across folders aren't deduplicated — if two bound
/// folders happen to be parent/child of each other, both get
/// listed and compose flags the duplicate include itself.
pub fn discover_compose_files_for_folders<I, P>(folder_roots: I) -> ComposeDiscovery
where
	I: IntoIterator<Item = P>,
	P: AsRef<Utf8Path>,
{
	let mut files = Vec::new();
	for root in folder_roots {
		let mut sub = discover_compose_files(root.as_ref());
		files.append(&mut sub.files);
	}
	ComposeDiscovery { files }
}

struct FoundFile {
	absolute: Utf8PathBuf,
	relative: Utf8PathBuf,
	precedence: u8,
}

const COMPOSE_FILENAMES: &[(&str, u8)] = &[
	("compose.yaml", 1),
	("compose.yml", 2),
	("docker-compose.yaml", 3),
	("docker-compose.yml", 4),
];

fn scan_dir(dir: &Utf8Path, workspace_root: &Utf8Path, by_dir: &mut BTreeMap<Utf8PathBuf, FoundFile>) {
	for (filename, precedence) in COMPOSE_FILENAMES {
		let candidate = dir.join(filename);
		if !candidate.is_file() {
			continue;
		}
		let Some(rel) = relative_to(workspace_root, &candidate) else {
			continue;
		};
		// Per directory, keep the highest-precedence file (lowest
		// numeric value); ignore the rest.
		let entry = by_dir.entry(dir.to_path_buf()).or_insert_with(|| FoundFile {
			absolute: candidate.clone(),
			relative: rel.clone(),
			precedence: *precedence,
		});
		if *precedence < entry.precedence {
			entry.absolute = candidate;
			entry.relative = rel;
			entry.precedence = *precedence;
		}
	}
}

/// `node_modules`, `target`, etc. — directories where finding a
/// compose file would be a false positive.
const SKIP_DIR_NAMES: &[&str] = &[
	"node_modules",
	"target",
	"dist",
	"build",
	".next",
	".venv",
	"venv",
	"__pycache__",
];

fn immediate_subdirs(root: &Utf8Path) -> Vec<Utf8PathBuf> {
	let Ok(read_dir) = std::fs::read_dir(root) else {
		return Vec::new();
	};
	let mut subdirs = Vec::new();
	for entry in read_dir.flatten() {
		let Ok(meta) = entry.metadata() else {
			continue;
		};
		// Symlinks may point back into the tree; follow nothing in
		// a one-shot discovery.
		if meta.file_type().is_symlink() || !meta.is_dir() {
			continue;
		}
		let Some(name_os) = entry.file_name().to_str().map(str::to_owned) else {
			continue;
		};
		if name_os.starts_with('.') || SKIP_DIR_NAMES.contains(&name_os.as_str()) {
			continue;
		}
		let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
			continue;
		};
		subdirs.push(path);
	}
	subdirs.sort();
	subdirs
}

fn relative_to(base: &Utf8Path, target: &Utf8Path) -> Option<Utf8PathBuf> {
	target.strip_prefix(base).ok().map(|rel| rel.to_path_buf())
}

#[cfg(test)]
mod tests {
	use std::fs;

	use camino::Utf8PathBuf;
	use tempfile::tempdir;

	use super::*;

	fn touch(path: &Utf8Path) {
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).unwrap();
		}
		fs::write(path, b"# placeholder\n").unwrap();
	}

	fn root(dir: &tempfile::TempDir) -> Utf8PathBuf {
		Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()
	}

	#[test]
	fn empty_workspace_yields_empty_discovery() {
		let tmp = tempdir().unwrap();
		let result = discover_compose_files(&root(&tmp));
		assert!(result.files.is_empty());
	}

	#[test]
	fn workspace_root_compose_is_picked_up() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("docker-compose.yml"));
		let result = discover_compose_files(&root);
		assert_eq!(result.files.len(), 1);
		assert_eq!(result.files[0].relative_path, "docker-compose.yml");
	}

	#[test]
	fn moon_landing_shape_finds_subdir_compose() {
		// The acceptance scenario: a "command-centre" workspace
		// holding a single project subdirectory with its own
		// compose. moon-landing is the canonical example.
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("moon-landing/docker-compose.yml"));
		touch(&root.join("moon-landing/package.json"));

		let result = discover_compose_files(&root);
		assert_eq!(result.files.len(), 1);
		assert_eq!(result.files[0].relative_path, "moon-landing/docker-compose.yml");
	}

	#[test]
	fn multiple_subdirs_all_picked_up_and_sorted() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("zeta/docker-compose.yml"));
		touch(&root.join("alpha/compose.yaml"));
		touch(&root.join("middle/compose.yml"));

		let result = discover_compose_files(&root);
		let rels: Vec<_> = result.files.iter().map(|f| f.relative_path.as_str()).collect();
		assert_eq!(
			rels,
			vec!["alpha/compose.yaml", "middle/compose.yml", "zeta/docker-compose.yml",],
		);
	}

	#[test]
	fn precedence_picks_compose_yaml_over_docker_compose_yml() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("svc/compose.yaml"));
		touch(&root.join("svc/docker-compose.yml"));

		let result = discover_compose_files(&root);
		assert_eq!(result.files.len(), 1);
		assert_eq!(result.files[0].relative_path, "svc/compose.yaml");
	}

	#[test]
	fn does_not_descend_past_depth_one() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("a/b/docker-compose.yml"));
		assert!(discover_compose_files(&root).files.is_empty());
	}

	#[test]
	fn skips_hidden_and_artefact_dirs() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		// Each of these would be a false-positive include if we
		// walked it.
		touch(&root.join(".git/docker-compose.yml"));
		touch(&root.join(".cargo/docker-compose.yml"));
		touch(&root.join("node_modules/lib/docker-compose.yml"));
		touch(&root.join("target/debug/docker-compose.yml"));
		touch(&root.join("dist/docker-compose.yml"));

		assert!(discover_compose_files(&root).files.is_empty());
	}

	#[test]
	fn does_not_follow_symlinked_subdirs() {
		// Symlinks could point back into the workspace and create
		// loops; the contract is "skip any symlinked directory".
		// On platforms without symlink support this test trivially
		// passes by virtue of the branch never being taken.
		#[cfg(unix)]
		{
			let tmp = tempdir().unwrap();
			let root = root(&tmp);
			touch(&root.join("real/docker-compose.yml"));
			std::os::unix::fs::symlink(root.join("real").as_std_path(), root.join("link").as_std_path()).unwrap();

			let result = discover_compose_files(&root);
			assert_eq!(result.files.len(), 1);
			assert_eq!(result.files[0].relative_path, "real/docker-compose.yml");
		}
	}

	#[test]
	fn non_existent_root_returns_empty() {
		let path = Utf8PathBuf::from("/definitely/not/here/moon-x");
		assert!(discover_compose_files(&path).files.is_empty());
	}

	#[test]
	fn discover_root_compose_picks_root_only() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("docker-compose.yml"));
		// Subdirectory compose deliberately ignored by the
		// per-folder helper.
		touch(&root.join("svc/compose.yaml"));

		let found = discover_root_compose(&root).expect("root compose found");
		assert_eq!(found.relative_path, "docker-compose.yml");
		assert_eq!(found.absolute_path, root.join("docker-compose.yml"));
	}

	#[test]
	fn discover_root_compose_returns_none_when_only_subdir_has_one() {
		let tmp = tempdir().unwrap();
		let root = root(&tmp);
		touch(&root.join("svc/compose.yaml"));
		assert!(discover_root_compose(&root).is_none());
	}

	#[test]
	fn discover_root_compose_returns_none_for_missing_path() {
		let path = Utf8PathBuf::from("/definitely/not/here/moon-x");
		assert!(discover_root_compose(&path).is_none());
	}

	#[test]
	fn multi_folder_discovery_concatenates_in_input_order() {
		let tmp = tempdir().unwrap();
		let base = root(&tmp);
		let landing = base.join("moon-landing");
		let infra = base.join("infra");
		touch(&landing.join("docker-compose.yml"));
		touch(&infra.join("compose.yaml"));
		// Bind a third folder with no compose file at all — the
		// helper should tolerate it.
		let docs = base.join("docs");
		std::fs::create_dir_all(&docs).unwrap();

		let result = discover_compose_files_for_folders([&landing, &infra, &docs]);
		let abs: Vec<_> = result
			.files
			.iter()
			.map(|f| f.absolute_path.as_str().to_owned())
			.collect();
		assert_eq!(
			abs,
			vec![
				landing.join("docker-compose.yml").as_str().to_owned(),
				infra.join("compose.yaml").as_str().to_owned(),
			],
		);
	}
}
