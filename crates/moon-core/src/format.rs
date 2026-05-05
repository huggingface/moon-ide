//! The `RunFormatter` rung of the pre-save pipeline.
//!
//! Translates a lint-staged command (e.g. `prettier --write`) into the
//! tool's stdin/stdout invocation, runs it with a 5s timeout, and
//! returns the formatted text. Saves must always succeed, so any
//! failure here (binary missing, non-zero exit, timeout, non-utf-8
//! stdout) collapses to `None` with a `tracing::warn!`; the caller
//! falls back to the editorconfig-normalised text.
//!
//! See [specs/decisions/0012-format-on-save.md](../../../specs/decisions/0012-format-on-save.md).

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the lint-staged `command` for `abs_file_path` against `text` and
/// return the formatted output. `workspace_root` bounds the upward
/// `node_modules/.bin` walk so we don't escape the workspace looking
/// for tools.
pub async fn run_formatter(
	workspace_root: &Utf8Path,
	abs_file_path: &Utf8Path,
	command: &str,
	text: &str,
) -> Option<String> {
	let parts = parse_command(command);
	let (bin_token, user_args) = parts.split_first()?;
	let bin_name = bin_basename(bin_token);

	let Some(tool) = KnownTool::from_bin_name(bin_name) else {
		warn_once("unsupported", bin_name, || {
			tracing::warn!(tool = bin_name, "format-on-save: unsupported tool; skipping")
		});
		return None;
	};

	let start_dir = abs_file_path.parent().unwrap_or(workspace_root);
	let resolved_bin = resolve_binary(tool, start_dir, workspace_root)?;
	let argv = tool.build_argv(user_args, abs_file_path);

	spawn_and_capture(&resolved_bin, &argv, text).await
}

#[derive(Debug, Clone, Copy)]
enum KnownTool {
	Oxfmt,
	Prettier,
	Rustfmt,
}

impl KnownTool {
	fn from_bin_name(name: &str) -> Option<Self> {
		match name {
			"oxfmt" => Some(Self::Oxfmt),
			"prettier" => Some(Self::Prettier),
			"rustfmt" => Some(Self::Rustfmt),
			_ => None,
		}
	}

	fn binary_name(self) -> &'static str {
		match self {
			Self::Oxfmt => "oxfmt",
			Self::Prettier => "prettier",
			Self::Rustfmt => "rustfmt",
		}
	}

	/// Look in `node_modules/.bin/` first for tools the team installs
	/// via npm. `rustfmt` ships with rustup; PATH-only.
	fn prefers_node_modules(self) -> bool {
		matches!(self, Self::Oxfmt | Self::Prettier)
	}

	/// Translate the lint-staged user args into the tool's stdin-mode
	/// argv. Mode flags that force file-mutation (`--write`, `--check`,
	/// `--list-different`) are stripped — we only run in stdin mode.
	fn build_argv(self, user_args: &[String], abs_path: &Utf8Path) -> Vec<String> {
		let filtered: Vec<String> = user_args.iter().filter(|a| !is_mode_flag(a)).cloned().collect();
		match self {
			Self::Oxfmt => {
				let mut argv = filtered;
				argv.push(format!("--stdin-filepath={abs_path}"));
				argv
			}
			Self::Prettier => {
				let mut argv = filtered;
				argv.push("--stdin-filepath".to_owned());
				argv.push(abs_path.to_string());
				argv
			}
			Self::Rustfmt => {
				let mut argv = vec!["--emit".to_owned(), "stdout".to_owned()];
				argv.extend(filtered);
				argv
			}
		}
	}
}

fn is_mode_flag(arg: &str) -> bool {
	matches!(arg, "--write" | "--check" | "--list-different")
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

fn resolve_binary(tool: KnownTool, start_dir: &Utf8Path, root: &Utf8Path) -> Option<Utf8PathBuf> {
	let name = tool.binary_name();
	if tool.prefers_node_modules() {
		if let Some(p) = find_node_modules_bin(name, start_dir, root) {
			return Some(p);
		}
	}
	match which::which(name) {
		Ok(p) => match Utf8PathBuf::from_path_buf(p) {
			Ok(p) => Some(p),
			Err(p) => {
				tracing::warn!(path = ?p, tool = name, "format-on-save: tool path is not utf-8; skipping");
				None
			}
		},
		Err(_) => {
			warn_once("missing", name, || {
				tracing::warn!(
					tool = name,
					"format-on-save: tool not found in node_modules/.bin or $PATH; skipping"
				)
			});
			None
		}
	}
}

fn find_node_modules_bin(name: &str, start: &Utf8Path, root: &Utf8Path) -> Option<Utf8PathBuf> {
	let mut current: Option<&Utf8Path> = Some(start);
	while let Some(dir) = current {
		let candidate = dir.join("node_modules").join(".bin").join(name);
		if candidate.exists() {
			return Some(candidate);
		}
		if dir == root {
			break;
		}
		current = dir.parent();
	}
	None
}

async fn spawn_and_capture(bin: &Utf8Path, argv: &[String], text: &str) -> Option<String> {
	let mut cmd = Command::new(bin.as_str());
	cmd
		.args(argv)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());

	let mut child = match cmd.spawn() {
		Ok(c) => c,
		Err(err) => {
			tracing::warn!(bin = %bin, %err, "format-on-save: spawn failed");
			return None;
		}
	};

	// Write stdin in a separate task and read stdout via
	// `wait_with_output` so a tool that streams its output (and would
	// block on a full 64 KiB pipe buffer if stdin and stdout were on the
	// same task) can't deadlock with us.
	let stdin = child.stdin.take();
	let bytes = text.as_bytes().to_vec();
	let writer = tokio::spawn(async move {
		if let Some(mut stdin) = stdin {
			if let Err(err) = stdin.write_all(&bytes).await {
				tracing::warn!(%err, "format-on-save: stdin write failed");
			}
			let _ = stdin.shutdown().await;
		}
	});

	let output = match timeout(FORMAT_TIMEOUT, child.wait_with_output()).await {
		Ok(Ok(o)) => o,
		Ok(Err(err)) => {
			tracing::warn!(%err, "format-on-save: subprocess failed");
			let _ = writer.await;
			return None;
		}
		Err(_) => {
			tracing::warn!(
				timeout_ms = FORMAT_TIMEOUT.as_millis() as u64,
				"format-on-save: tool timed out"
			);
			let _ = writer.await;
			return None;
		}
	};
	let _ = writer.await;

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		tracing::warn!(status = ?output.status, stderr = %stderr.trim(), "format-on-save: tool exited with error");
		return None;
	}

	match String::from_utf8(output.stdout) {
		Ok(s) => Some(s),
		Err(err) => {
			tracing::warn!(%err, "format-on-save: tool stdout was not utf-8");
			None
		}
	}
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
	fn build_argv_oxfmt_appends_stdin_filepath() {
		let argv = KnownTool::Oxfmt.build_argv(&[], Utf8Path::new("/abs/foo.ts"));
		assert_eq!(argv, vec!["--stdin-filepath=/abs/foo.ts".to_owned()]);
	}

	#[test]
	fn build_argv_prettier_strips_write_flag() {
		let user = vec!["--write".to_owned(), "--plugin=foo".to_owned()];
		let argv = KnownTool::Prettier.build_argv(&user, Utf8Path::new("/abs/App.svelte"));
		assert_eq!(
			argv,
			vec![
				"--plugin=foo".to_owned(),
				"--stdin-filepath".to_owned(),
				"/abs/App.svelte".to_owned(),
			]
		);
	}

	#[test]
	fn build_argv_rustfmt_prepends_emit_stdout() {
		let user = vec!["--edition".to_owned(), "2021".to_owned()];
		let argv = KnownTool::Rustfmt.build_argv(&user, Utf8Path::new("/abs/lib.rs"));
		assert_eq!(
			argv,
			vec![
				"--emit".to_owned(),
				"stdout".to_owned(),
				"--edition".to_owned(),
				"2021".to_owned(),
			]
		);
	}

	#[test]
	fn build_argv_strips_check_and_list_different() {
		let user = vec!["--check".to_owned(), "--list-different".to_owned()];
		let argv = KnownTool::Prettier.build_argv(&user, Utf8Path::new("/x/y.ts"));
		assert_eq!(argv, vec!["--stdin-filepath".to_owned(), "/x/y.ts".to_owned()]);
	}

	#[test]
	fn known_tool_from_name() {
		assert!(matches!(KnownTool::from_bin_name("oxfmt"), Some(KnownTool::Oxfmt)));
		assert!(matches!(
			KnownTool::from_bin_name("prettier"),
			Some(KnownTool::Prettier)
		));
		assert!(matches!(KnownTool::from_bin_name("rustfmt"), Some(KnownTool::Rustfmt)));
		assert!(KnownTool::from_bin_name("eslint").is_none());
	}
}
