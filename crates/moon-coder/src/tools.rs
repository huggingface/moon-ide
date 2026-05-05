//! Tool surface dispatched by the agent loop.
//!
//! Phase 6.0 ships read-only tools (`read_file`, `list_dir`, `grep`)
//! plus a host-side `bash`. Mutating tools (`write_file`,
//! `edit_file`) and IDE-native tools (`goto_definition`, `git_*`)
//! land as separate commits in 6.x as concrete need appears — see
//! `specs/coder.md` § Tool surface.
//!
//! Every tool dispatches against the active workspace folder via
//! [`moon_core::WorkspaceHost`] (or a service that takes its root,
//! such as `moon_core::search`). That gives us container-aware
//! routing for free once Phase 2 grows the [`WorkspaceHost`] impl
//! for `ContainerHost`.
//!
//! Per `specs/coder.md` § Error model: tools **throw**. Returning a
//! string like "ERROR: ..." as content confuses the model. Errors
//! become `isError: true` content blocks at the loop layer.

use std::sync::Arc;
use std::time::Duration;

use camino::Utf8Path;
use moon_core::WorkspaceRegistry;
use moon_protocol::search::{ContentSearchHit, ContentSearchOptions};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::CoderError;
use crate::inference::ToolDefinition;

/// Hard cap on `bash` runtime — keeps a runaway tool call from
/// burning the LLM's budget waiting for a hung process. Matches the
/// "single bash per call" pi convention. The agent can chain bash
/// tool calls if it really wants to wait longer.
const BASH_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const BASH_MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// `read_file` returns at most this many bytes. Beyond it we
/// truncate + tell the model so it can iterate (e.g. follow up with
/// `grep` for the part it cares about). Full-file reads are still
/// useful — most source files fit comfortably.
const READ_FILE_MAX_BYTES: usize = 200_000;

/// `bash` stdout/stderr cap. Same rationale as `READ_FILE_MAX_BYTES`
/// — the model doesn't need megabytes of output to reason about a
/// command's outcome.
const BASH_OUTPUT_MAX_BYTES: usize = 64_000;

/// Tools are dispatched by name. The registry holds the JSON-schema
/// descriptors handed to the LLM and a handle to the workspace
/// registry the runtime needs to resolve the active folder.
#[derive(Clone)]
pub struct ToolRegistry {
	workspaces: Arc<WorkspaceRegistry>,
}

impl ToolRegistry {
	pub fn new(workspaces: Arc<WorkspaceRegistry>) -> Self {
		Self { workspaces }
	}

	/// Tool definitions to advertise to the model on every chat call.
	pub fn definitions(&self) -> Vec<ToolDefinition> {
		vec![
			ToolDefinition::function(
				"read_file",
				"Read the contents of a file inside the active workspace folder. Returns the file's text. Refuses paths outside the workspace.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Workspace-relative path to the file."
						}
					},
					"required": ["path"]
				}),
			),
			ToolDefinition::function(
				"list_dir",
				"List the immediate contents of a directory inside the active workspace folder. Returns one entry per line in `kind  name` form.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Workspace-relative path. Use \".\" for the workspace root.",
							"default": "."
						}
					}
				}),
			),
			ToolDefinition::function(
				"grep",
				"Regex search across the workspace folder, gitignore-aware. Returns one match per line in `path:line: match` form.",
				json!({
					"type": "object",
					"properties": {
						"pattern": {
							"type": "string",
							"description": "Rust-syntax regular expression."
						},
						"case_sensitive": {
							"type": "boolean",
							"description": "Match case-sensitively. Defaults to false (smart-case off)."
						},
						"max_matches": {
							"type": "integer",
							"description": "Stop after this many matches. Defaults to 200."
						}
					},
					"required": ["pattern"]
				}),
			),
			ToolDefinition::function(
				"bash",
				"Run a shell command in the active workspace folder. Returns stdout, stderr, exit_code. Times out after 120s by default.",
				json!({
					"type": "object",
					"properties": {
						"cmd": {
							"type": "string",
							"description": "Shell command, executed via `sh -lc <cmd>`."
						},
						"timeout_ms": {
							"type": "integer",
							"description": "Soft timeout in milliseconds. Capped at 600000 (10 minutes)."
						}
					},
					"required": ["cmd"]
				}),
			),
		]
	}

	pub async fn dispatch(&self, name: &str, args: &Value, cancel: &CancellationToken) -> Result<Value, CoderError> {
		match name {
			"read_file" => self.read_file(args).await,
			"list_dir" => self.list_dir(args).await,
			"grep" => self.grep(args).await,
			"bash" => self.bash(args, cancel).await,
			other => Err(CoderError::UnknownTool(other.to_string())),
		}
	}

	async fn read_file(&self, args: &Value) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct ReadFileArgs {
			path: String,
		}
		let parsed: ReadFileArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("read_file", err.to_string()))?;
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let result = folder.host.read_file(Utf8Path::new(&parsed.path)).await?;
		if result.is_binary {
			return Err(CoderError::tool_failed("read_file", "binary file"));
		}
		let (text, truncated) = if result.text.len() > READ_FILE_MAX_BYTES {
			let cut = clamp_to_char_boundary(&result.text, READ_FILE_MAX_BYTES);
			(result.text[..cut].to_string(), true)
		} else {
			(result.text, false)
		};
		Ok(json!({
			"path": parsed.path,
			"content": text,
			"truncated": truncated,
			"mtime_ms": result.mtime_ms,
		}))
	}

	async fn list_dir(&self, args: &Value) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct ListDirArgs {
			#[serde(default = "default_dot")]
			path: String,
		}
		fn default_dot() -> String {
			".".into()
		}
		let parsed: ListDirArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("list_dir", err.to_string()))?;
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let entries = folder.host.read_dir(Utf8Path::new(&parsed.path)).await?;
		let mut out = String::new();
		for e in &entries {
			let kind = match e.kind {
				moon_protocol::fs::EntryKind::Dir => "dir ",
				moon_protocol::fs::EntryKind::File => "file",
				moon_protocol::fs::EntryKind::Symlink => "link",
				moon_protocol::fs::EntryKind::Other => "?   ",
			};
			out.push_str(kind);
			out.push(' ');
			out.push_str(&e.name);
			out.push('\n');
		}
		Ok(json!({
			"path": parsed.path,
			"entries": out,
			"count": entries.len(),
		}))
	}

	async fn grep(&self, args: &Value) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct GrepArgs {
			pattern: String,
			#[serde(default)]
			case_sensitive: bool,
			#[serde(default)]
			max_matches: Option<u32>,
		}
		let parsed: GrepArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("grep", err.to_string()))?;
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		// We don't have a `WorkspaceHost::content_search` method yet —
		// the existing `moon_core::search::search_content` is a free
		// function that takes a `Utf8Path` root. For local hosts that
		// matches the active folder; container-aware routing arrives
		// when `WorkspaceHost` grows a `content_search` trait method
		// (Phase 6.x or sooner if `RemoteHost` lands first).
		let root = camino::Utf8PathBuf::from(folder.folder.path.clone());
		let options = ContentSearchOptions {
			query: parsed.pattern.clone(),
			case_sensitive: parsed.case_sensitive,
			regex: true,
			max_matches: parsed.max_matches.unwrap_or(200) as usize,
		};
		let result = tokio::task::spawn_blocking(move || moon_core::search::search_content(&root, &options))
			.await
			.map_err(|err| CoderError::Internal(format!("grep join error: {err}")))??;
		let formatted = format_grep_hits(&result.hits);
		Ok(json!({
			"pattern": parsed.pattern,
			"matches": formatted,
			"count": result.hits.len(),
			"truncated": result.truncated,
		}))
	}

	async fn bash(&self, args: &Value, cancel: &CancellationToken) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct BashArgs {
			cmd: String,
			#[serde(default)]
			timeout_ms: Option<u64>,
		}
		let parsed: BashArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("bash", err.to_string()))?;
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let timeout = parsed
			.timeout_ms
			.map(Duration::from_millis)
			.unwrap_or(BASH_DEFAULT_TIMEOUT)
			.min(BASH_MAX_TIMEOUT);

		let mut command = tokio::process::Command::new("sh");
		// Container-aware routing arrives in 6.2 (it'll route through
		// `WorkspaceHost::spawn` once that exists). For now, bash
		// always runs on the host — fine for the team's day-1 usage
		// where the workspace is host-mounted.
		command
			.arg("-lc")
			.arg(&parsed.cmd)
			.current_dir(folder.folder.path.as_str())
			.kill_on_drop(true)
			.stdin(std::process::Stdio::null())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped());

		let child = command
			.spawn()
			.map_err(|err| CoderError::tool_failed("bash", format!("spawn failed: {err}")))?;

		let output = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			result = tokio::time::timeout(timeout, child.wait_with_output()) => result,
		};

		let output = match output {
			Ok(Ok(o)) => o,
			Ok(Err(err)) => return Err(CoderError::tool_failed("bash", err.to_string())),
			Err(_) => {
				return Err(CoderError::tool_failed(
					"bash",
					format!("timed out after {} ms", timeout.as_millis()),
				));
			}
		};

		let stdout = truncate_bytes(&output.stdout, BASH_OUTPUT_MAX_BYTES);
		let stderr = truncate_bytes(&output.stderr, BASH_OUTPUT_MAX_BYTES);
		Ok(json!({
			"cmd": parsed.cmd,
			"exit_code": output.status.code(),
			"stdout": stdout,
			"stderr": stderr,
		}))
	}
}

fn clamp_to_char_boundary(s: &str, max: usize) -> usize {
	if s.len() <= max {
		return s.len();
	}
	let mut idx = max;
	while idx > 0 && !s.is_char_boundary(idx) {
		idx -= 1;
	}
	idx
}

fn format_grep_hits(hits: &[ContentSearchHit]) -> String {
	let mut out = String::new();
	for hit in hits {
		// `path:line: line_text` — same shape as `grep -n`. Trim the
		// matched line to keep individual hits short; the model still
		// gets enough surrounding context to decide whether to read
		// the file.
		let trimmed = hit.line_text.trim_end_matches('\n');
		out.push_str(&hit.path);
		out.push(':');
		out.push_str(&hit.line.to_string());
		out.push_str(": ");
		out.push_str(trimmed);
		out.push('\n');
	}
	out
}

fn truncate_bytes(bytes: &[u8], max: usize) -> String {
	if bytes.len() <= max {
		return String::from_utf8_lossy(bytes).into_owned();
	}
	let cut = clamp_to_char_boundary_bytes(bytes, max);
	let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
	s.push_str("\n[...output truncated]");
	s
}

fn clamp_to_char_boundary_bytes(bytes: &[u8], max: usize) -> usize {
	if bytes.len() <= max {
		return bytes.len();
	}
	let mut idx = max;
	while idx > 0 {
		match std::str::from_utf8(&bytes[..idx]) {
			Ok(_) => return idx,
			Err(_) => idx -= 1,
		}
	}
	0
}
