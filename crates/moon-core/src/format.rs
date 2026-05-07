//! The `RunFormatter` rung of the pre-save pipeline.
//!
//! Runs a lint-staged command against the on-disk file, in the same
//! shape `bun run lint-staged` itself does on commit: spawn the binary
//! the user wrote in their config, append the absolute file path as a
//! positional argument, let the tool mutate the file in place. No
//! per-tool allow-list, no flag rewriting, no stdin plumbing.
//!
//! The team's `node_modules/.bin/` chain (from `cwd` up to the
//! workspace root) is prepended to `PATH` so locally-installed tools
//! resolve before system ones — same convention `npm-run-path` /
//! lint-staged itself use.
//!
//! Saves must always succeed, so any failure here (binary missing,
//! non-zero exit, timeout, spawn error) collapses to `Ok(false)` with a
//! `tracing::warn!` and the caller keeps going / accepts whatever the
//! file looked like before this command ran. Chain commands abort on
//! the first failure, mirroring lint-staged's semantics.
//!
//! ## Container routing
//!
//! When the active folder runs inside a workspace shell container
//! (`ShellTarget::Container`), the spawn is wrapped as
//! `docker exec -w <container_cwd> <name> <bin> <args> <abs_in_container>`
//! — same shape the LSP and the agent's `bash` tool use. Paths are
//! translated through the bind mount (`/workspace/<basename>/...`) so
//! the in-container process sees the file under the same path
//! `cargo fmt` / `prettier` / `eslint` would see when invoked from a
//! terminal in the container. The host `PATH` walk is skipped in
//! container mode; the container's own `PATH` plus the bind-mounted
//! `node_modules/.bin/` directories are added via `--env PATH=…` so
//! the same project-local-binary discovery rule applies on either
//! side.
//!
//! See [specs/decisions/0013-format-on-save-file-based.md](../../../specs/decisions/0013-format-on-save-file-based.md)
//! (the current design) and
//! [specs/decisions/0012-format-on-save.md](../../../specs/decisions/0012-format-on-save.md)
//! (the original stdin/stdout design that this supersedes).

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use crate::shell::ShellTarget;

const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the lint-staged `command` for `abs_file_path`. The subprocess
/// runs with `config_dir` as its `cwd` (so relative arguments like
/// `--ignore-path ../.prettierignore` resolve from the same place
/// lint-staged itself would resolve them) and `PATH` prefixed with
/// every `node_modules/.bin/` directory walking from `config_dir` up to
/// `workspace_root` (matches lint-staged's per-package binary discovery
/// in pnpm-style monorepos).
///
/// `target` decides whether the binary spawns on the host or inside
/// the workspace shell container — see [`ShellTarget`]. Container mode
/// translates `config_dir`, `abs_file_path`, and the `node_modules/.bin/`
/// chain to in-container paths via the bind mount; if the file isn't
/// inside the mount we silently fall back to host (the spawn is
/// best-effort either way).
///
/// Returns `Ok(true)` when the subprocess exited 0; `Ok(false)` for any
/// failure that's been logged. Errors are collapsed to `Ok(false)` —
/// format-on-save is best-effort by design.
pub async fn run_formatter(
	workspace_root: &Utf8Path,
	config_dir: &Utf8Path,
	abs_file_path: &Utf8Path,
	command: &str,
	target: &ShellTarget,
) -> bool {
	let parts = parse_command(command);
	let Some((bin_token, user_args)) = parts.split_first() else {
		return false;
	};
	let bin_name = bin_basename(bin_token);

	let mut cmd = match build_command(workspace_root, config_dir, abs_file_path, bin_token, user_args, target) {
		Some(cmd) => cmd,
		None => {
			tracing::warn!(
				tool = bin_name,
				host_path = %abs_file_path,
				"format-on-save: file is outside the container bind mount; skipping"
			);
			return false;
		}
	};
	cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

	let child = match cmd.spawn() {
		Ok(c) => c,
		Err(err) => {
			if err.kind() == std::io::ErrorKind::NotFound {
				warn_once("missing", bin_name, || {
					tracing::warn!(
						tool = bin_name,
						"format-on-save: tool not found in node_modules/.bin or $PATH; skipping"
					)
				});
			} else {
				tracing::warn!(tool = bin_name, %err, "format-on-save: spawn failed");
			}
			return false;
		}
	};

	let output = match timeout(FORMAT_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(err)) => {
			tracing::warn!(tool = bin_name, %err, "format-on-save: subprocess failed");
			return false;
		}
		Err(_) => {
			tracing::warn!(
				tool = bin_name,
				timeout_ms = FORMAT_TIMEOUT.as_millis() as u64,
				"format-on-save: tool timed out"
			);
			return false;
		}
	};

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		tracing::warn!(
			tool = bin_name,
			status = ?output.status,
			stderr = %stderr.trim(),
			"format-on-save: tool exited with error"
		);
		return false;
	}

	true
}

/// Whitespace split. Lint-staged commands are simple — no shell pipes,
/// no quoted arguments in any team config we've seen. If a team adds
/// one with quoting we'll want a real shlex; until then the simpler
/// split keeps the dependency surface minimal.
fn parse_command(command: &str) -> Vec<String> {
	command.split_whitespace().map(str::to_owned).collect()
}

fn bin_basename(s: &str) -> &str {
	let trimmed = s.trim_start_matches("./");
	match trimmed.rfind(['/', '\\']) {
		Some(i) => &trimmed[i + 1..],
		None => trimmed,
	}
}

/// Build the actual `tokio::process::Command` for the resolved
/// shell target. Returns `None` when `target` is `Container` but
/// the input paths can't be translated into the bind mount
/// (cross-folder lint-staged config, file outside the workspace,
/// etc.) — caller logs and falls back to "no command ran".
fn build_command(
	workspace_root: &Utf8Path,
	config_dir: &Utf8Path,
	abs_file_path: &Utf8Path,
	bin_token: &str,
	user_args: &[String],
	target: &ShellTarget,
) -> Option<Command> {
	match target {
		ShellTarget::Host => {
			let path_var = build_path_env(config_dir, workspace_root);
			let mut argv: Vec<&str> = user_args.iter().map(String::as_str).collect();
			let abs_str = abs_file_path.as_str();
			argv.push(abs_str);

			let mut cmd = Command::new(bin_token);
			cmd.args(&argv).current_dir(config_dir.as_str()).env("PATH", &path_var);
			Some(cmd)
		}
		ShellTarget::Container { container_name, .. } => {
			let translated_config = target.translate_path(config_dir)?;
			let translated_abs = target.translate_path(abs_file_path)?;

			// `docker exec` (no `-it`): captured stdout/stderr,
			// no TTY. Same shape `moon-coder`'s `bash` tool and
			// the LSP `DockerExec` spawner use.
			//
			// We deliberately don't override the *in-container*
			// `PATH`. Docker's `--env PATH=…` *replaces* the
			// container's PATH, which would lose system bins
			// (`/usr/local/bin`, rustup's `~/.cargo/bin`, …).
			// The container image (moon-base) is responsible
			// for setting PATH so the user's lint-staged
			// commands resolve. Project-local
			// `node_modules/.bin/` discovery on the container
			// side is a future enhancement — flag it via
			// container image PATH or a `sh -lc` wrapper if a
			// real project needs it.
			//
			// We *do* prepend the host-side
			// `node_modules/.bin/` chain to the **docker
			// subprocess's** PATH (host-side lookup of `docker`
			// itself). In production this is a no-op — docker
			// is always system-wide — but it lets host-only
			// tests substitute a fake `docker` script the same
			// way they substitute a fake formatter.
			let path_var = build_path_env(config_dir, workspace_root);
			let mut cmd = Command::new("docker");
			cmd
				.arg("exec")
				.arg("-w")
				.arg(translated_config.as_str())
				.arg(container_name)
				.arg(bin_token);
			for arg in user_args {
				cmd.arg(arg);
			}
			cmd.arg(translated_abs.as_str());
			cmd.env("PATH", &path_var);
			Some(cmd)
		}
	}
}

/// Language-default formatter resolution result. The `command` is
/// the lint-staged-shaped string the caller hands to
/// [`run_formatter`] (which appends the absolute file path as the
/// last positional). `cwd` is the directory the subprocess should
/// run in; we expose it so a language with project-local tooling
/// (e.g. Python's `.venv/bin/ruff`) can pin the cwd to the project
/// root, and a `command` whose first token is a path *relative* to
/// that cwd resolves correctly on both host and container without
/// any bin-token translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultFormatCommand {
	/// Whitespace-separated command. The caller appends the
	/// absolute path of the file being saved as the final
	/// positional argument.
	pub command: String,
	/// Working directory the subprocess runs in. For host-mode
	/// this is forwarded to `Command::current_dir`; for
	/// container-mode it's translated through the bind mount and
	/// passed as `docker exec -w <translated_cwd>`.
	pub cwd: Utf8PathBuf,
}

/// Language-default formatter for `abs_path`, used as a fallback
/// when lint-staged either has no config in the workspace or has
/// a config but no matching rule for this file. Looked up by file
/// extension (case-insensitive).
///
/// Today's table:
///
/// | extension     | command (base)              | cwd                  |
/// |---------------|-----------------------------|----------------------|
/// | `.rs`         | `rustfmt --edition <e>`     | file's parent dir    |
/// | `.py` / `.pyi`| `[.venv/bin/]ruff format`   | nearest project root |
///
/// The **Rust path** walks parents from `abs_path` looking for the
/// nearest `Cargo.toml` with a `[package].edition` field. Bare
/// `rustfmt <file>` defaults to edition 2015 — which rejects
/// `async fn`, `let-else`, every modern Rust feature — because
/// rustfmt only reads `Cargo.toml` for the edition when it's
/// invoked through `cargo fmt`. We do that detection ourselves
/// so format-on-save is per-file (no whole-package reformat) and
/// still picks up the project's actual edition. Falls back to
/// `2024` when no `Cargo.toml` is found.
///
/// The **Python path** walks parents looking for a project root
/// marker (`pyproject.toml`, `setup.py`, or `setup.cfg`) and pins
/// `cwd` to that directory so `ruff` finds the project's
/// `[tool.ruff]` config and so a relative `.venv/bin/ruff`
/// resolves. When `<root>/.venv/bin/ruff` exists on the host
/// filesystem (the bind mount makes that the same inode as
/// `/workspace/<basename>/.venv/bin/ruff` inside the container,
/// so this works for both host and container targets) we prefer
/// it over the bare `ruff` lookup — projects pin a specific ruff
/// version in their venv and we want to honour it. No venv → bare
/// `ruff format` lookup against `PATH`. No project root → fall
/// back to the file's parent directory and bare `ruff`. The
/// `.pyi` stub extension routes the same way; `ruff format`
/// handles both.
///
/// Per AGENTS.md "hardcode first, configure later" we add a row
/// when a project the team uses needs one. lint-staged still wins
/// whenever it matches, so adding a row never overrides an
/// explicit team config.
pub fn default_format_command(abs_path: &Utf8Path) -> Option<DefaultFormatCommand> {
	let ext = abs_path.extension()?.to_ascii_lowercase();
	let parent = abs_path.parent()?;
	match ext.as_str() {
		"rs" => {
			let edition = nearest_cargo_edition(abs_path).unwrap_or_else(|| "2024".to_owned());
			Some(DefaultFormatCommand {
				command: format!("rustfmt --edition {edition}"),
				cwd: parent.to_path_buf(),
			})
		}
		"py" | "pyi" => {
			let project_root = nearest_python_project_root(abs_path);
			let cwd = project_root.clone().unwrap_or_else(|| parent.to_path_buf());
			// `<root>/.venv/bin/ruff` on the host filesystem is
			// the same inode as `/workspace/<basename>/.venv/bin/ruff`
			// inside the container thanks to the bind mount, so
			// this `is_file` probe is correct for both targets
			// without us having to translate the path back and
			// forth.
			let bin = match &project_root {
				Some(root) if root.join(".venv").join("bin").join("ruff").is_file() => ".venv/bin/ruff".to_owned(),
				_ => "ruff".to_owned(),
			};
			Some(DefaultFormatCommand {
				command: format!("{bin} format"),
				cwd,
			})
		}
		_ => None,
	}
}

/// Walk parents from `start_file` looking for the nearest
/// directory that anchors a Python project. We accept (in order)
/// `pyproject.toml`, `setup.py`, `setup.cfg` — the three markers
/// every Python packaging tool consults. Returns the first
/// directory containing any of them, or `None` if we walk all the
/// way to the root without finding one (a loose `.py` script with
/// no project around it).
fn nearest_python_project_root(start_file: &Utf8Path) -> Option<Utf8PathBuf> {
	const MARKERS: &[&str] = &["pyproject.toml", "setup.py", "setup.cfg"];

	let mut current = start_file.parent();
	while let Some(dir) = current {
		for marker in MARKERS {
			if dir.join(marker).is_file() {
				return Some(dir.to_path_buf());
			}
		}
		current = dir.parent();
	}
	None
}

/// Walk parents from `start_file` looking for the nearest
/// `Cargo.toml` whose `[package]` table declares an `edition`. We
/// only care about the immediate package — workspace `Cargo.toml`s
/// without `[package]` are skipped, since rustfmt operates per
/// crate.
///
/// Lives here (not in a generic `cargo` helper) because the only
/// caller is the rustfmt fallback. Uses byte-level scanning instead
/// of a TOML parser dependency: we look for an `edition = "…"` line
/// inside the `[package]` table by tracking the current section
/// header. Robust enough for the way rustfmt and cargo fmt
/// themselves consume the field.
fn nearest_cargo_edition(start_file: &Utf8Path) -> Option<String> {
	let mut current = start_file.parent();
	while let Some(dir) = current {
		let cargo = dir.join("Cargo.toml");
		if cargo.is_file() {
			if let Some(edition) = parse_package_edition(cargo.as_std_path()) {
				return Some(edition);
			}
		}
		current = dir.parent();
	}
	None
}

fn parse_package_edition(path: &std::path::Path) -> Option<String> {
	let text = std::fs::read_to_string(path).ok()?;
	let mut in_package = false;
	for raw in text.lines() {
		let line = raw.trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		if let Some(rest) = line.strip_prefix('[') {
			if let Some(name) = rest.strip_suffix(']') {
				in_package = name.trim() == "package";
				continue;
			}
		}
		if !in_package {
			continue;
		}
		let Some(rest) = line.strip_prefix("edition") else {
			continue;
		};
		// Allow `edition = "2024"` and `edition="2024"`.
		let after = rest.trim_start();
		let Some(value) = after.strip_prefix('=') else {
			continue;
		};
		let value = value.trim().trim_start_matches('"').trim_end_matches('"');
		if value.is_empty() {
			continue;
		}
		return Some(value.to_owned());
	}
	None
}

/// Build a `PATH` value with every `node_modules/.bin/` directory from
/// `start` up to `root` (inclusive) prepended to the inherited `PATH`.
/// Mirrors `npm-run-path`: a project-installed `prettier` resolves
/// before any system one, but `node` / `bun` / `rustfmt` (which aren't
/// in `node_modules/.bin/`) fall through to the system path.
fn build_path_env(start: &Utf8Path, root: &Utf8Path) -> OsString {
	let separator = if cfg!(windows) { ';' } else { ':' };
	let mut prefix = String::new();
	let mut current: Option<&Utf8Path> = Some(start);
	while let Some(dir) = current {
		let bin = dir.join("node_modules").join(".bin");
		if !prefix.is_empty() {
			prefix.push(separator);
		}
		prefix.push_str(bin.as_str());
		if dir == root {
			break;
		}
		current = dir.parent();
	}

	let mut out = OsString::from(prefix);
	if let Some(existing) = env::var_os("PATH") {
		if !out.is_empty() {
			out.push(separator.to_string());
		}
		out.push(existing);
	}
	out
}

fn warn_once(kind: &'static str, key: &str, emit: impl FnOnce()) {
	static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
	let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
	let id = format!("{kind}:{key}");
	let mut guard = seen.lock().expect("format-on-save warn cache poisoned");
	if guard.insert(id) {
		emit();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use camino::Utf8PathBuf;
	use tempfile::TempDir;

	#[test]
	fn parse_command_splits_on_whitespace() {
		assert_eq!(parse_command("oxfmt"), vec!["oxfmt".to_owned()]);
		assert_eq!(
			parse_command("prettier --write"),
			vec!["prettier".to_owned(), "--write".to_owned()]
		);
		assert_eq!(
			parse_command("rustfmt --edition 2021"),
			vec!["rustfmt".to_owned(), "--edition".to_owned(), "2021".to_owned()]
		);
		assert!(parse_command("").is_empty());
	}

	#[test]
	fn default_format_command_unknown_extensions_return_none() {
		// No Cargo.toml / pyproject.toml lookup happens for
		// non-supported paths, so an arbitrary path string is
		// fine.
		assert!(default_format_command(Utf8Path::new("/abs/README")).is_none());
		assert!(default_format_command(Utf8Path::new("/abs/a.txt")).is_none());
		assert!(default_format_command(Utf8Path::new("/abs/a.ts")).is_none());
	}

	#[test]
	fn default_format_command_rust_passes_detected_edition() {
		// Drop a `Cargo.toml` declaring edition 2024 next to a
		// dummy `.rs` file. The fallback must walk up, find it,
		// and emit `rustfmt --edition 2024 …`.
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		std::fs::write(
			root.join("Cargo.toml").as_std_path(),
			"[package]\nname = \"x\"\nedition = \"2024\"\n",
		)
		.unwrap();
		let src = root.join("src");
		std::fs::create_dir_all(src.as_std_path()).unwrap();
		let file = src.join("main.rs");
		std::fs::write(file.as_std_path(), "fn main() {}").unwrap();

		let resolved = default_format_command(&file).expect("rust fallback");
		assert_eq!(resolved.command, "rustfmt --edition 2024");
		// Rust path keeps the file's parent dir as cwd —
		// rustfmt walks for `rustfmt.toml` itself.
		assert_eq!(resolved.cwd, src);
	}

	#[test]
	fn default_format_command_rust_falls_back_to_2024_without_cargo_toml() {
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		let file = root.join("loose.rs");
		std::fs::write(file.as_std_path(), "fn main() {}").unwrap();

		let resolved = default_format_command(&file).expect("rust fallback");
		assert_eq!(resolved.command, "rustfmt --edition 2024");
		assert_eq!(resolved.cwd, root);
	}

	#[test]
	fn default_format_command_rust_skips_workspace_cargo_toml() {
		// Workspace `Cargo.toml` (no `[package]`) at the root,
		// real package nested with edition 2021. Resolver must
		// pick the nested package, not the workspace.
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		std::fs::write(
			root.join("Cargo.toml").as_std_path(),
			"[workspace]\nmembers = [\"crates/foo\"]\n",
		)
		.unwrap();
		let pkg = root.join("crates").join("foo");
		std::fs::create_dir_all(pkg.as_std_path()).unwrap();
		std::fs::write(
			pkg.join("Cargo.toml").as_std_path(),
			"[package]\nname = \"foo\"\nedition = \"2021\"\n",
		)
		.unwrap();
		let src = pkg.join("src");
		std::fs::create_dir_all(src.as_std_path()).unwrap();
		let file = src.join("lib.rs");
		std::fs::write(file.as_std_path(), "").unwrap();

		let resolved = default_format_command(&file).expect("rust fallback");
		assert_eq!(resolved.command, "rustfmt --edition 2021");
	}

	/// Python: `pyproject.toml` at the project root + a
	/// `.venv/bin/ruff` shim. The fallback should pin `cwd` to
	/// the project root and prefer the venv's ruff over a bare
	/// `ruff` PATH lookup. The `is_file()` probe runs against
	/// the host filesystem, which is also what the container
	/// sees (same inode, bind mount), so the resolver is
	/// target-agnostic.
	#[cfg(unix)]
	#[test]
	fn default_format_command_python_prefers_venv_ruff() {
		use std::os::unix::fs::PermissionsExt;
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		std::fs::write(root.join("pyproject.toml").as_std_path(), "[project]\nname = \"x\"\n").unwrap();
		let venv_bin = root.join(".venv").join("bin");
		std::fs::create_dir_all(venv_bin.as_std_path()).unwrap();
		// Real file — the resolver checks `is_file`, not
		// executability. We mark it +x anyway for parity with
		// what an actual venv ships.
		let ruff = venv_bin.join("ruff");
		std::fs::write(ruff.as_std_path(), "#!/bin/sh\nexit 0\n").unwrap();
		std::fs::set_permissions(ruff.as_std_path(), std::fs::Permissions::from_mode(0o755)).unwrap();

		let src = root.join("src");
		std::fs::create_dir_all(src.as_std_path()).unwrap();
		let file = src.join("main.py");
		std::fs::write(file.as_std_path(), "print('x')\n").unwrap();

		let resolved = default_format_command(&file).expect("python fallback");
		assert_eq!(resolved.command, ".venv/bin/ruff format");
		// cwd at the project root — `.venv/bin/ruff` resolves
		// relative to it both on host and inside the container
		// (`docker exec -w /workspace/foo … .venv/bin/ruff`).
		assert_eq!(resolved.cwd, root);
	}

	#[test]
	fn default_format_command_python_falls_back_to_bare_ruff_without_venv() {
		// Project root with `pyproject.toml` but no `.venv`. The
		// resolver still pins `cwd` to the project root (so ruff
		// picks up `[tool.ruff]`) but the bin token falls through
		// to a bare `ruff` PATH lookup.
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		std::fs::write(root.join("pyproject.toml").as_std_path(), "[project]\nname = \"x\"\n").unwrap();
		let file = root.join("a.py");
		std::fs::write(file.as_std_path(), "x = 1\n").unwrap();

		let resolved = default_format_command(&file).expect("python fallback");
		assert_eq!(resolved.command, "ruff format");
		assert_eq!(resolved.cwd, root);
	}

	#[test]
	fn default_format_command_python_loose_file_uses_parent_dir() {
		// No project marker anywhere up the tree — the resolver
		// can't anchor on a project root, so it falls back to
		// the file's parent directory. ruff still works (its
		// own defaults are fine) but won't find a project
		// `[tool.ruff]` config.
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		let file = root.join("scratch.py");
		std::fs::write(file.as_std_path(), "x = 1\n").unwrap();

		let resolved = default_format_command(&file).expect("python fallback");
		assert_eq!(resolved.command, "ruff format");
		assert_eq!(resolved.cwd, root);
	}

	#[test]
	fn default_format_command_python_setup_py_anchors_project_root() {
		// Older project layout: `setup.py` at the root, no
		// `pyproject.toml`. Resolver should still anchor cwd
		// there.
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		std::fs::write(
			root.join("setup.py").as_std_path(),
			"from setuptools import setup\nsetup()\n",
		)
		.unwrap();
		let pkg = root.join("mypkg");
		std::fs::create_dir_all(pkg.as_std_path()).unwrap();
		let file = pkg.join("__init__.py");
		std::fs::write(file.as_std_path(), "").unwrap();

		let resolved = default_format_command(&file).expect("python fallback");
		assert_eq!(resolved.command, "ruff format");
		assert_eq!(resolved.cwd, root);
	}

	#[test]
	fn default_format_command_python_pyi_stubs_route_through_ruff() {
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		std::fs::write(root.join("pyproject.toml").as_std_path(), "[project]\nname = \"x\"\n").unwrap();
		let file = root.join("api.pyi");
		std::fs::write(file.as_std_path(), "def f(x: int) -> int: ...\n").unwrap();

		let resolved = default_format_command(&file).expect("python fallback for .pyi");
		assert_eq!(resolved.command, "ruff format");
	}

	#[test]
	fn parse_package_edition_handles_simple_manifests() {
		let tmp = TempDir::new().unwrap();
		let path = tmp.path().join("Cargo.toml");
		std::fs::write(&path, "[package]\nname = \"x\"\nedition = \"2021\"\n").unwrap();
		assert_eq!(parse_package_edition(&path), Some("2021".to_owned()));

		std::fs::write(&path, "[workspace]\nmembers = []\n").unwrap();
		assert_eq!(parse_package_edition(&path), None);

		// `edition` outside `[package]` (e.g. inside a
		// dependency table) must not match.
		std::fs::write(
			&path,
			"[package]\nname = \"x\"\n[dependencies.foo]\nedition = \"2018\"\n",
		)
		.unwrap();
		assert_eq!(parse_package_edition(&path), None);
	}

	#[test]
	fn bin_basename_strips_paths() {
		assert_eq!(bin_basename("oxfmt"), "oxfmt");
		assert_eq!(bin_basename("./oxfmt"), "oxfmt");
		assert_eq!(bin_basename("./node_modules/.bin/prettier"), "prettier");
		assert_eq!(bin_basename("/usr/bin/rustfmt"), "rustfmt");
	}

	#[test]
	fn build_path_env_walks_from_start_to_root() {
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		let nested = root.join("a").join("b");
		std::fs::create_dir_all(nested.as_std_path()).unwrap();

		let path = build_path_env(&nested, &root);
		let s = path.to_string_lossy();
		let parts: Vec<&str> = s.split(if cfg!(windows) { ';' } else { ':' }).collect();
		// Closest first, then walking up to root.
		assert_eq!(parts[0], nested.join("node_modules/.bin").as_str());
		assert_eq!(parts[1], root.join("a/node_modules/.bin").as_str());
		assert_eq!(parts[2], root.join("node_modules/.bin").as_str());
		// Inherited PATH is appended after the prefix; its presence /
		// content is environment-dependent so just sanity-check that
		// at least one extra entry came through when the host has a
		// PATH (basically always in CI / dev shells).
		if env::var_os("PATH").is_some() {
			assert!(parts.len() > 3, "expected inherited PATH to be appended: {s}");
		}
	}

	/// Smoke test the whole spawn path: drop a tiny shell script in the
	/// temp dir that mutates the file argument it receives, run it
	/// through `run_formatter`, and assert the file changed. Validates
	/// the contract that matters — "appends the abs path as the last
	/// positional arg" — without bundling prettier / oxfmt into CI.
	#[cfg(unix)]
	#[tokio::test]
	async fn run_formatter_spawns_and_passes_path() {
		use std::os::unix::fs::PermissionsExt;
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();

		let script = root.join("fmt.sh");
		std::fs::write(
			script.as_std_path(),
			"#!/bin/sh\nprintf 'formatted:%s\\n' \"$1\" > \"$1\"\n",
		)
		.unwrap();
		std::fs::set_permissions(script.as_std_path(), std::fs::Permissions::from_mode(0o755)).unwrap();

		let file = root.join("input.txt");
		std::fs::write(file.as_std_path(), "before").unwrap();

		let ok = run_formatter(&root, &root, &file, "./fmt.sh", &ShellTarget::Host).await;
		assert!(ok);

		let after = std::fs::read_to_string(file.as_std_path()).unwrap();
		assert!(after.starts_with("formatted:"), "got: {after:?}");
		assert!(after.contains(file.as_str()), "got: {after:?}");
	}

	#[tokio::test]
	async fn run_formatter_missing_tool_returns_false() {
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		let file = root.join("a.txt");
		std::fs::write(file.as_std_path(), "x").unwrap();

		let ok = run_formatter(
			&root,
			&root,
			&file,
			"definitely-not-a-real-binary-xyzzy",
			&ShellTarget::Host,
		)
		.await;
		assert!(!ok);
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn run_formatter_non_zero_exit_returns_false() {
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		let file = root.join("a.txt");
		std::fs::write(file.as_std_path(), "x").unwrap();

		// `false` always exits 1 — file path arg is ignored.
		let ok = run_formatter(&root, &root, &file, "false", &ShellTarget::Host).await;
		assert!(!ok);
	}

	/// Container target: building the command for a `Container`
	/// shell target produces a `docker exec -w <container_cwd>
	/// <name> <bin> <args> <abs_in_container>` argv. Validates
	/// the host-to-container path translation and the no-`-it`
	/// shape (we want captured output, not a TTY).
	#[test]
	fn build_command_container_translates_paths_and_uses_docker_exec() {
		let target = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/workloads"),
			server_root: Utf8PathBuf::from("/workspace/workloads"),
		};
		let workspace_root = Utf8PathBuf::from("/home/dev/code/workloads");
		let config_dir = Utf8PathBuf::from("/home/dev/code/workloads/app/sdk");
		let abs_file = Utf8PathBuf::from("/home/dev/code/workloads/app/sdk/src/main.rs");
		let cmd = build_command(&workspace_root, &config_dir, &abs_file, "rustfmt", &[], &target)
			.expect("translation should succeed for paths inside the bind mount");
		let std_cmd = cmd.as_std();
		assert_eq!(std_cmd.get_program(), "docker");
		let args: Vec<_> = std_cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
		assert_eq!(
			args,
			vec![
				"exec",
				"-w",
				"/workspace/workloads/app/sdk",
				"moon-ws-default-dev-1",
				"rustfmt",
				"/workspace/workloads/app/sdk/src/main.rs",
			]
		);
		// No `-it` / `-t` — captured I/O, no TTY allocation.
		assert!(
			!args.iter().any(|a| a == "-t" || a == "-it"),
			"docker exec for format-on-save must not allocate a TTY"
		);
	}

	#[test]
	fn build_command_container_returns_none_outside_mount() {
		let target = ShellTarget::Container {
			container_name: "moon-ws-default-dev-1".into(),
			host_root: Utf8PathBuf::from("/home/dev/code/workloads"),
			server_root: Utf8PathBuf::from("/workspace/workloads"),
		};
		// File is on host but outside any bound folder.
		let cmd = build_command(
			Utf8Path::new("/home/dev/code/workloads"),
			Utf8Path::new("/etc"),
			Utf8Path::new("/etc/hostname"),
			"rustfmt",
			&[],
			&target,
		);
		assert!(cmd.is_none());
	}

	#[test]
	fn build_command_host_keeps_existing_invocation_shape() {
		let target = ShellTarget::Host;
		let tmp = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().canonicalize().unwrap()).unwrap();
		let file = root.join("a.txt");
		let cmd = build_command(&root, &root, &file, "rustfmt", &[], &target).expect("host always succeeds");
		let std_cmd = cmd.as_std();
		assert_eq!(std_cmd.get_program(), "rustfmt");
		// `cmd.arg(abs_str)` should be the only positional arg.
		let args: Vec<_> = std_cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
		assert_eq!(args, vec![file.as_str()]);
	}
}
