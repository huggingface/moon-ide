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
//! See [specs/decisions/0013-format-on-save-file-based.md](../../../specs/decisions/0013-format-on-save-file-based.md)
//! (the current design) and
//! [specs/decisions/0012-format-on-save.md](../../../specs/decisions/0012-format-on-save.md)
//! (the original stdin/stdout design that this supersedes).

use camino::Utf8Path;
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the lint-staged `command` for `abs_file_path`. The subprocess
/// runs with `config_dir` as its `cwd` (so relative arguments like
/// `--ignore-path ../.prettierignore` resolve from the same place
/// lint-staged itself would resolve them) and `PATH` prefixed with
/// every `node_modules/.bin/` directory walking from `config_dir` up to
/// `workspace_root` (matches lint-staged's per-package binary discovery
/// in pnpm-style monorepos).
///
/// Returns `Ok(true)` when the subprocess exited 0; `Ok(false)` for any
/// failure that's been logged. Errors are collapsed to `Ok(false)` —
/// format-on-save is best-effort by design.
pub async fn run_formatter(
	workspace_root: &Utf8Path,
	config_dir: &Utf8Path,
	abs_file_path: &Utf8Path,
	command: &str,
) -> bool {
	let parts = parse_command(command);
	let Some((bin_token, user_args)) = parts.split_first() else {
		return false;
	};
	let bin_name = bin_basename(bin_token);

	let mut argv: Vec<&str> = user_args.iter().map(String::as_str).collect();
	let abs_str = abs_file_path.as_str();
	argv.push(abs_str);

	let path_var = build_path_env(config_dir, workspace_root);

	let mut cmd = Command::new(bin_token);
	cmd
		.args(&argv)
		.current_dir(config_dir.as_str())
		.env("PATH", &path_var)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());

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

		let ok = run_formatter(&root, &root, &file, "./fmt.sh").await;
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

		let ok = run_formatter(&root, &root, &file, "definitely-not-a-real-binary-xyzzy").await;
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
		let ok = run_formatter(&root, &root, &file, "false").await;
		assert!(!ok);
	}
}
