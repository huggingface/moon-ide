//! Tool surface dispatched by the agent loop.
//!
//! Phase 6.2 adds `write_file` and `edit_file` on top of the 6.0
//! read-only set (`read_file`, `list_dir`, `grep`, `bash`). The
//! agent can now create new files, overwrite existing ones, and do
//! surgical exact-string edits without going through `bash`. IDE-
//! native tools (`goto_definition`, `git_*`) and container-aware
//! `bash` (via `WorkspaceHost::spawn`) land in later sub-phases as
//! concrete need appears — see `specs/coder.md` § Tool surface.
//!
//! Every tool dispatches against the active workspace folder via
//! [`moon_core::WorkspaceHost`] (or a service that takes its root,
//! such as `moon_core::search`). That gives us container-aware
//! routing for free once Phase 2 grows the [`WorkspaceHost`] impl
//! for `ContainerHost` *and* `WorkspaceHost::spawn` exists.
//!
//! Per `specs/coder.md` § Error model: tools **throw**. Returning a
//! string like "ERROR: ..." as content confuses the model. Errors
//! become `isError: true` content blocks at the loop layer.

use std::sync::Arc;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use moon_core::{WorkspaceFolderEntry, WorkspaceRegistry};
use moon_protocol::search::{ContentSearchHit, ContentSearchOptions};
use moon_protocol::workspace::HostKind;
use moon_terminal::{container_name_for_workspace, TerminalTarget};
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
			ToolDefinition::function(
				"write_file",
				"Overwrite a file with new content (or create it if missing). Use for new files or whole-file rewrites; prefer `edit_file` for surgical changes inside a large file. The file's parent directory must already exist. Throws on path-traversal attempts outside the workspace folder.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Workspace-relative path. Created if it does not exist."
						},
						"content": {
							"type": "string",
							"description": "Full file contents. Whatever you pass becomes the file verbatim — include the trailing newline if you want one."
						}
					},
					"required": ["path", "content"]
				}),
			),
			ToolDefinition::function(
				"edit_file",
				"Replace an exact substring inside a file. `find` must match the file *exactly* (including whitespace and line endings) and must be unique unless `occurrence` is given. To insert text, set `find` to the line you want to insert before/after and include it in `replace`. To delete, set `replace` to an empty string. Failure throws — when it does, retry with more surrounding context in `find` so the match becomes unique.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Workspace-relative path. The file must already exist."
						},
						"find": {
							"type": "string",
							"description": "Exact substring to locate. No regex; whitespace is significant."
						},
						"replace": {
							"type": "string",
							"description": "Replacement text. Pass an empty string to delete the matched span."
						},
						"occurrence": {
							"type": "integer",
							"description": "1-based index of which match to replace when `find` matches multiple times. Omit to require exactly one match."
						}
					},
					"required": ["path", "find", "replace"]
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
			"write_file" => self.write_file(args).await,
			"edit_file" => self.edit_file(args).await,
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

	async fn write_file(&self, args: &Value) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct WriteFileArgs {
			path: String,
			content: String,
		}
		let parsed: WriteFileArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("write_file", err.to_string()))?;
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let result = folder
			.host
			.write_file(Utf8Path::new(&parsed.path), &parsed.content)
			.await?;
		Ok(json!({
			"path": parsed.path,
			"bytes_written": parsed.content.len(),
			"mtime_ms": result.mtime_ms,
		}))
	}

	async fn edit_file(&self, args: &Value) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct EditFileArgs {
			path: String,
			find: String,
			replace: String,
			#[serde(default)]
			occurrence: Option<usize>,
		}
		let parsed: EditFileArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("edit_file", err.to_string()))?;
		if parsed.find.is_empty() {
			return Err(CoderError::invalid_args("edit_file", "`find` must not be empty"));
		}
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		let path = Utf8Path::new(&parsed.path);
		let original = folder.host.read_file(path).await?;
		if original.is_binary {
			return Err(CoderError::tool_failed("edit_file", "binary file"));
		}

		let matches: Vec<usize> = byte_offsets_of(&original.text, &parsed.find);
		let target_idx = match (matches.len(), parsed.occurrence) {
			(0, _) => {
				return Err(CoderError::tool_failed(
					"edit_file",
					format!("`find` not found in {}", parsed.path),
				));
			}
			(1, None | Some(1)) => matches[0],
			(_, None) => {
				return Err(CoderError::tool_failed(
					"edit_file",
					format!(
						"`find` matched {} times in {}; pass `occurrence` (1-based) or include more surrounding context",
						matches.len(),
						parsed.path
					),
				));
			}
			(n, Some(idx)) if idx == 0 || idx > n => {
				return Err(CoderError::tool_failed(
					"edit_file",
					format!("occurrence {idx} out of range — `find` matched {n} times"),
				));
			}
			// `idx >= 1` and `idx <= n` here, so the subtraction can't
			// underflow. `matches[idx - 1]` is always in bounds.
			(_, Some(idx)) => matches[idx - 1],
		};

		let mut new_text = String::with_capacity(original.text.len() - parsed.find.len() + parsed.replace.len());
		new_text.push_str(&original.text[..target_idx]);
		new_text.push_str(&parsed.replace);
		new_text.push_str(&original.text[target_idx + parsed.find.len()..]);

		let result = folder.host.write_file(path, &new_text).await?;
		Ok(json!({
			"path": parsed.path,
			"replaced_at_byte": target_idx,
			"bytes_written": new_text.len(),
			"mtime_ms": result.mtime_ms,
			"occurrence": parsed.occurrence.unwrap_or(1),
			"total_matches": matches.len(),
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

		let (mut command, target_kind) = self.build_bash_command(&folder, &parsed.cmd).await?;
		command
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
			"target": target_kind,
			"exit_code": output.status.code(),
			"stdout": stdout,
			"stderr": stderr,
		}))
	}

	/// Build the platform-correct `bash` command for the active
	/// folder. Local folders run `sh -lc <cmd>` directly with the
	/// folder as `cwd`; devcontainer folders go through `docker exec
	/// -w <container_cwd> <name> sh -lc <cmd>` against the workspace
	/// shell container that compose / `moon-container` already
	/// brought up. Same convention used by `moon-terminal` (terminals)
	/// and `crates/moon-core/src/lsp/spawn.rs` (LSP servers); we
	/// reuse the helpers in `moon_terminal::target` for the
	/// container-name + in-container-cwd derivation so all three
	/// stay in lockstep.
	///
	/// Returns the wire-friendly target label (`"host"` /
	/// `"container"`) so the tool result and the panel header
	/// indicator pip read the same value.
	async fn build_bash_command(
		&self,
		folder: &WorkspaceFolderEntry,
		cmd: &str,
	) -> Result<(tokio::process::Command, &'static str), CoderError> {
		match folder.folder.host {
			HostKind::Local => {
				let mut command = tokio::process::Command::new("sh");
				command.arg("-lc").arg(cmd).current_dir(folder.folder.path.as_str());
				Ok((command, BASH_TARGET_HOST))
			}
			HostKind::Devcontainer => {
				let workspace_id = self.workspaces.workspace_id().await;
				let container_name = container_name_for_workspace(&workspace_id);
				// Fall back to `/workspace` if the host path has no
				// basename — same fallback `moon-terminal` uses for
				// pathological inputs (`/`). Devcontainer folders
				// nearly always have one, so this is belt-and-braces.
				let container_cwd = TerminalTarget::container_cwd_for_folder(Utf8Path::new(&folder.folder.path))
					.unwrap_or_else(|| Utf8PathBuf::from("/workspace"));
				// `docker exec` (no `-it`): we want captured
				// stdout/stderr, not a TTY. Terminals get `-it`;
				// the bash tool doesn't.
				let mut command = tokio::process::Command::new("docker");
				command
					.arg("exec")
					.arg("-w")
					.arg(container_cwd.as_str())
					.arg(&container_name)
					.arg("sh")
					.arg("-lc")
					.arg(cmd);
				Ok((command, BASH_TARGET_CONTAINER))
			}
		}
	}
}

/// Wire labels for the `bash` target. Frontend reads these
/// verbatim from the tool result (and from `CoderStatus`) so the
/// strings are part of the protocol — don't rename without
/// updating `src/lib/protocol.ts` in lockstep.
pub(crate) const BASH_TARGET_HOST: &str = "host";
pub(crate) const BASH_TARGET_CONTAINER: &str = "container";

/// Find every byte-offset at which `needle` appears in `haystack`.
/// Used by `edit_file` to (a) detect zero-match / multi-match cases
/// before mutating, and (b) pick the right occurrence when the LLM
/// disambiguates with `occurrence`.
///
/// Linear-scan with `str::find` advancement: O(n·m) but the inputs
/// are LLM-sized (file contents + a few hundred bytes of `find`),
/// not large-corpus. Same algorithm `pi-mono` uses for the same
/// reason.
fn byte_offsets_of(haystack: &str, needle: &str) -> Vec<usize> {
	if needle.is_empty() {
		return Vec::new();
	}
	let mut hits = Vec::new();
	let mut start = 0;
	while let Some(idx) = haystack[start..].find(needle) {
		let absolute = start + idx;
		hits.push(absolute);
		start = absolute + needle.len();
	}
	hits
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

#[cfg(test)]
mod tests {
	use super::byte_offsets_of;

	#[test]
	fn byte_offsets_of_finds_non_overlapping_hits() {
		// Two distinct hits at non-overlapping offsets — the
		// behaviour `edit_file` relies on for the multi-match
		// `occurrence` selector.
		assert_eq!(byte_offsets_of("foo bar foo", "foo"), vec![0, 8]);
	}

	#[test]
	fn byte_offsets_of_returns_empty_for_empty_needle() {
		// `edit_file` already rejects an empty `find` upstream,
		// but the helper stays safe on its own so a future caller
		// can't get an infinite loop out of it.
		assert!(byte_offsets_of("foo", "").is_empty());
	}

	#[test]
	fn byte_offsets_of_advances_past_match_no_overlap_loop() {
		// `aa` in `aaaa` — naive `start += 1` would emit 0, 1, 2.
		// Our advancement is `+= needle.len()`, so we get 0, 2.
		// `edit_file` deliberately treats overlapping matches as a
		// non-issue: real-world `find` strings aren't pathological.
		assert_eq!(byte_offsets_of("aaaa", "aa"), vec![0, 2]);
	}
}
