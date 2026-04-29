//! End-to-end: a workspace that looks like moon-landing's "command
//! centre" shape goes through `discover_compose_files_for_folders`
//! → `generate_compose` and lands a usable `compose.yaml`.
//!
//! The Phase 2.0 acceptance line in
//! [`specs/roadmaps/phase-02-containers.md`](../../../specs/roadmaps/phase-02-containers.md)
//! is "opening moon-landing brings up all eleven services with a
//! single 'Set up' click". This test isn't that (it doesn't run
//! Docker), but it pins the wire between the two pure modules
//! that the Tauri lifecycle command will assemble.

use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::{
	discover_compose_files_for_folders, generate_compose, project_name_for_id, BoundMount, ComposeRenderOptions,
};
use tempfile::tempdir;

fn touch(path: &Utf8Path) {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).unwrap();
	}
	fs::write(path, b"# placeholder\n").unwrap();
}

#[test]
fn multi_folder_workspace_renders_includable_compose() {
	let tmp = tempdir().unwrap();
	let base = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

	// Two bound folders — the post-2.5 shape. moon-landing carries
	// its own compose; moon-ide doesn't (it's just a sibling
	// project).
	let landing = base.join("moon-landing");
	let ide = base.join("moon-ide");
	touch(&landing.join("docker-compose.yml"));
	touch(&landing.join("package.json"));
	fs::create_dir_all(ide.as_std_path()).unwrap();
	touch(&ide.join("Cargo.toml"));
	// Noise that should stay out of the discovery.
	touch(&landing.join("node_modules/lib/docker-compose.yml"));
	touch(&landing.join(".git/HEAD"));

	let discovery = discover_compose_files_for_folders([&landing, &ide]);
	assert_eq!(discovery.files.len(), 1, "{:?}", discovery.files);
	assert_eq!(discovery.files[0].absolute_path, landing.join("docker-compose.yml"));

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
	let includes: Vec<&Utf8Path> = discovery.files.iter().map(|f| f.absolute_path.as_path()).collect();
	let render = generate_compose(ComposeRenderOptions {
		project: &project,
		dev_image: "moon-base:dev",
		bound_mounts: &mounts,
		include_files: &includes,
	});

	let yaml = &render.yaml;

	assert!(yaml.contains("name: moon-ws-default"));
	assert!(
		yaml.contains(&format!("- {}", landing.join("docker-compose.yml").as_str())),
		"include should use absolute paths now:\n{yaml}",
	);
	// Each bound folder lands as a `<host>:/workspace/<name>` mount.
	assert!(yaml.contains(&format!("- {}:/workspace/moon-landing", landing.as_str())));
	assert!(yaml.contains(&format!("- {}:/workspace/moon-ide", ide.as_str())));
	assert!(yaml.contains("image: moon-base:dev"));
	assert!(yaml.contains("working_dir: /workspace"));
	assert!(yaml.contains("shell-service: dev"));
}
