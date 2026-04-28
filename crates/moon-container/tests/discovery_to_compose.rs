//! End-to-end: a workspace that looks like moon-landing's "command
//! centre" shape goes through `discover_compose_files` →
//! `generate_compose` and lands a usable `.moon/compose.yaml`.
//!
//! The Phase 2.0 acceptance line in
//! [`specs/roadmaps/phase-02-containers.md`](../../../specs/roadmaps/phase-02-containers.md)
//! is "opening moon-landing brings up all eleven services with a
//! single 'Set up' click". This test isn't that (it doesn't run
//! Docker), but it pins the wire between the two pure modules
//! that the Tauri lifecycle command will assemble.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::{discover_compose_files, generate_compose, project_name_for, ComposeRenderOptions};
use tempfile::tempdir;

fn touch(path: &Utf8Path) {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).unwrap();
	}
	fs::write(path, b"# placeholder\n").unwrap();
}

#[test]
fn moon_landing_shaped_workspace_renders_includable_compose() {
	let tmp = tempdir().unwrap();
	let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

	// A workspace containing one project subdir with its own
	// compose, plus the assorted noise we expect to be skipped.
	touch(&root.join("moon-landing/docker-compose.yml"));
	touch(&root.join("moon-landing/package.json"));
	touch(&root.join("node_modules/lib/docker-compose.yml"));
	touch(&root.join(".git/HEAD"));

	let discovery = discover_compose_files(&root);
	assert_eq!(discovery.files.len(), 1, "{:?}", discovery.files);
	assert_eq!(discovery.files[0].relative_path, "moon-landing/docker-compose.yml");

	let project = project_name_for(&root);
	let includes: Vec<&Utf8Path> = discovery.files.iter().map(|f| f.relative_path.as_path()).collect();
	let render = generate_compose(ComposeRenderOptions {
		project: &project,
		dev_image: "moon-base:dev",
		include_files: &includes,
	});

	let yaml = &render.yaml;

	assert!(yaml.contains(&format!("name: {project}")));
	assert!(
		yaml.contains("- ../moon-landing/docker-compose.yml"),
		"include path should be expressed relative to .moon/, not the workspace root:\n{yaml}",
	);
	assert!(yaml.contains("image: moon-base:dev"));
	assert!(yaml.contains("shell-service: dev"));
}
