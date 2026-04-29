//! End-to-end: a multi-folder workspace renders a dev-only
//! `compose.yaml` that bind-mounts each bound folder under
//! `/workspace/`, while each folder's own compose file is
//! discovered separately by `discover_root_compose` for its
//! per-folder lifecycle.
//!
//! The Phase 2 acceptance line in
//! [`specs/roadmaps/phase-02-containers.md`](../../../specs/roadmaps/phase-02-containers.md)
//! after the workspace-shell-vs-project-services split: "opening
//! moon-landing gives you a workspace shell instantly; the user
//! launches its services from the folder bar with one click".
//! This test pins the seam between discovery + compose
//! generation that the Tauri layer composes.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::{
	discover_root_compose, generate_compose, project_name_for_folder, project_name_for_id, BoundMount,
	ComposeRenderOptions,
};
use tempfile::tempdir;

fn touch(path: &Utf8Path) {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).unwrap();
	}
	fs::write(path, b"# placeholder\n").unwrap();
}

#[test]
fn workspace_compose_is_dev_only_with_one_mount_per_folder() {
	let tmp = tempdir().unwrap();
	let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

	// Two bound folders — the post-2.5 shape. moon-landing carries
	// its own compose, moon-ide does not.
	let landing = base.join("moon-landing");
	let ide = base.join("moon-ide");
	touch(&landing.join("docker-compose.yml"));
	touch(&landing.join("package.json"));
	fs::create_dir_all(ide.as_std_path()).unwrap();
	touch(&ide.join("Cargo.toml"));
	// Noise that should stay out of the per-folder discovery.
	touch(&landing.join("node_modules/lib/docker-compose.yml"));
	touch(&landing.join(".git/HEAD"));

	let project = project_name_for_id("default").unwrap();
	let mounts = vec![
		BoundMount {
			host_path: landing.clone(),
			mount_name: "moon-landing".into(),
		},
		BoundMount {
			host_path: ide.clone(),
			mount_name: "moon-ide".into(),
		},
	];
	let render = generate_compose(ComposeRenderOptions {
		project: &project,
		dev_image: "moon-base:dev",
		bound_mounts: &mounts,
	});

	let yaml = &render.yaml;

	// Workspace shell only — no `include:` block, no project
	// services. Those are managed by the per-folder runner.
	assert!(yaml.contains("name: moon-ws-default"));
	assert!(
		!yaml.contains("include:"),
		"workspace compose must not include project files:\n{yaml}"
	);
	assert!(yaml.contains("image: moon-base:dev"));
	assert!(yaml.contains("working_dir: /workspace"));
	assert!(yaml.contains("shell-service: dev"));
	// Each bound folder lands as a `<host>:/workspace/<name>` mount.
	assert!(yaml.contains(&format!("- {}:/workspace/moon-landing", landing.as_str())));
	assert!(yaml.contains(&format!("- {}:/workspace/moon-ide", ide.as_str())));
}

#[test]
fn per_folder_discovery_picks_root_compose_file() {
	let tmp = tempdir().unwrap();
	let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
	let landing = base.join("moon-landing");
	let ide = base.join("moon-ide");
	touch(&landing.join("docker-compose.yml"));
	fs::create_dir_all(ide.as_std_path()).unwrap();
	touch(&ide.join("Cargo.toml"));

	let landing_compose = discover_root_compose(&landing).expect("moon-landing has a root compose");
	assert_eq!(landing_compose.absolute_path, landing.join("docker-compose.yml"));

	// A folder without a compose file at its root is the common
	// "edit-only" case — the per-folder UI should hide its
	// indicator there.
	assert!(discover_root_compose(&ide).is_none());

	// The compose project name for moon-landing is namespaced
	// under the workspace.
	let project = project_name_for_folder("default", "moon-landing").unwrap();
	assert_eq!(project.as_str(), "moon-ws-default-moon-landing");
}
