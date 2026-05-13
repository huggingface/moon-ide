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
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::{WorkspaceFolderEntry, WorkspaceRegistry};
use moon_protocol::container::ContainerState;
use moon_protocol::search::{ContentSearchHit, ContentSearchOptions};
use moon_terminal::{container_name_for_workspace, TerminalTarget};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::CoderError;
use crate::inference::ToolDefinition;
use crate::web::WebClient;

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

/// Two flavours every dispatched tool runs under. The parent's
/// top-level turn always uses [`CoderMode::Agent`]; sub-agents
/// pick per spawn (Phase C of the multi-project plan). Surfaced
/// to tools via [`ToolContext`] so write-side tools can self-gate
/// without each one re-deriving the rule.
///
/// The variant name `Agent` (and its wire string `"agent"`) is
/// deliberate: an earlier iteration called this `Coder`, but the
/// model would consistently treat a `mode: "coder"` sub-agent as
/// less capable than itself and hesitate to delegate non-trivial
/// work. `agent` reads as "another instance of you" in the model's
/// vocabulary and lifted that hesitation in dogfooding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoderMode {
	/// Read-only intent. `read_file` / `list_dir` / `grep` / `bash`
	/// stay available; `write_file` / `edit_file` short-circuit
	/// with [`CoderError::ReadOnlyMode`]. The "no mutation via
	/// `bash`" half of the constraint is behavioural — we can't
	/// usefully sandbox a shell — and lives in the sub-agent's
	/// system prompt instead.
	Research,
	/// Full toolkit. Today's parent-turn behaviour.
	Agent,
}

impl CoderMode {
	pub fn allows_writes(self) -> bool {
		matches!(self, Self::Agent)
	}

	/// Wire string used by event payloads (`SubagentSpawned.mode`,
	/// the `mode` field on the `spawn_subagent` tool result, etc.).
	/// Stable identifiers — `"research"` / `"agent"` — that the
	/// frontend reads verbatim, so don't rename without also
	/// updating `src/lib/protocol.ts`.
	pub fn as_wire(self) -> &'static str {
		match self {
			Self::Research => "research",
			Self::Agent => "agent",
		}
	}
}

/// Per-dispatch context. Replaces the previous "every tool calls
/// `workspaces.active_folder()` itself" pattern: the dispatcher
/// resolves the folder + mode once and hands them to each tool
/// invocation. Lets the sub-agent runner (Phase C) point a
/// concurrent dispatch at a different folder + mode pair without
/// touching the global active-folder state.
#[derive(Clone)]
pub struct ToolContext {
	pub folder: Arc<WorkspaceFolderEntry>,
	pub mode: CoderMode,
}

impl ToolContext {
	pub fn new(folder: Arc<WorkspaceFolderEntry>, mode: CoderMode) -> Self {
		Self { folder, mode }
	}
}

/// Tools are dispatched by name. The registry holds the JSON-schema
/// descriptors handed to the LLM, the workspace registry the runtime
/// needs to resolve container state for `bash`, the workspaces
/// state-dir parent so `bash` can ask `moon-container` whether the
/// workspace shell container is running, and the shared [`WebClient`]
/// the web search / fetch tools dispatch through. The per-call folder
/// + mode arrive via [`ToolContext`] on each [`dispatch`](Self::dispatch).
#[derive(Clone)]
pub struct ToolRegistry {
	workspaces: Arc<WorkspaceRegistry>,
	workspaces_dir: Utf8PathBuf,
	web: WebClient,
}

impl ToolRegistry {
	pub fn new(workspaces: Arc<WorkspaceRegistry>, workspaces_dir: Utf8PathBuf, web: WebClient) -> Self {
		Self {
			workspaces,
			workspaces_dir,
			web,
		}
	}

	/// Shared [`WebClient`]. Exposed so the Tauri command layer can
	/// expose the keyring-backed Tavily key surface (status / set /
	/// clear) without needing its own keyring entry.
	pub fn web(&self) -> &WebClient {
		&self.web
	}

	/// Build a [`ToolContext`] from the workspace's current active
	/// folder. Callers that already know the folder (sub-agent
	/// runner) construct `ToolContext::new` directly; this helper
	/// is the convenience the parent's `run_turn` uses to keep its
	/// "active folder is the parent folder" invariant in one spot.
	pub async fn context_for_active(&self, mode: CoderMode) -> Result<ToolContext, CoderError> {
		let folder = self
			.workspaces
			.active_folder()
			.await
			.ok_or(CoderError::NoActiveFolder)?;
		Ok(ToolContext::new(folder, mode))
	}

	/// Resolve a path argument against the synthetic `/workspace/`
	/// surface the system prompt advertises and route to the right
	/// bound folder. Returns the `(target_folder, relative_path)` pair
	/// the caller should dispatch against.
	///
	/// Three cases:
	///
	/// - **Synthetic `/workspace/<name>/...`**: routes to the folder
	///   whose basename matches `<name>` (active or otherwise). The
	///   path returned is whatever's left after stripping the
	///   `/workspace/<name>/` prefix; an empty tail becomes `"."`.
	///   Errors with a clear "no folder bound as `<name>`" message
	///   when the basename doesn't match anything bound.
	/// - **Bare relative path starting with another bound folder's
	///   basename** (`<other>/foo.rs`): also routes cross-folder.
	///   The model often types this form when the system prompt
	///   has shown it the synthetic `/workspace/<other>` path.
	///   Disambiguation: a leading `./` opts out and forces the
	///   path to resolve inside the [`ToolContext`]'s folder, so a
	///   legitimate same-named subdirectory still works.
	/// - **Anything else** (relative paths, absolute non-synthetic):
	///   resolved against `cx.folder` and left for
	///   [`WorkspaceHost::resolve`] to validate the way it
	///   always has.
	///
	/// Sub-agents call this with `cx.folder` set to their own
	/// assigned folder. They typically only see one bound folder
	/// in their tool context (the one they were spawned against),
	/// but the routing logic is identical — it just collapses to
	/// the no-op case when the basename matches `cx.folder`.
	async fn resolve_workspace_path(
		&self,
		raw: &str,
		cx: &ToolContext,
		tool: &'static str,
	) -> Result<(Arc<WorkspaceFolderEntry>, String), CoderError> {
		let folders = self.workspaces.folders().await;
		let active_name = cx.folder.folder.name.as_str();
		let path = Utf8Path::new(raw);

		// Synthetic `/workspace/<name>/...` path. Same routing rule
		// regardless of whether `<name>` matches the active folder
		// or a sibling — we always look up the folder, and the
		// returned `target` ends up equal to `cx.folder` when the
		// basenames match.
		if path.is_absolute() {
			if let Ok(rest) = path.strip_prefix("/workspace") {
				let mut comps = rest.components();
				if let Some(camino::Utf8Component::Normal(first)) = comps.next() {
					let target = folders
						.iter()
						.find(|f| f.folder.name == first)
						.cloned()
						.ok_or_else(|| unbound_folder_error(tool, raw, first, &folders))?;
					let tail = rest.strip_prefix(first).unwrap_or(Utf8Path::new(""));
					let s = tail.as_str();
					let resolved = if s.is_empty() { ".".to_string() } else { s.to_string() };
					return Ok((target, resolved));
				}
			}
			// Absolute paths that aren't `/workspace/<name>/...` go
			// through the active folder; the host's `resolve` will
			// reject anything outside its root.
			return Ok((cx.folder.clone(), raw.to_string()));
		}

		let mut comps = path.components();
		match comps.next() {
			// Leading `./` is the explicit "I mean a path *inside*
			// `cx.folder`, even if the first segment looks like a
			// sibling's basename" opt-out. Pass through untouched.
			Some(camino::Utf8Component::CurDir) => Ok((cx.folder.clone(), raw.to_string())),
			Some(camino::Utf8Component::Normal(name)) => {
				if name != active_name {
					if let Some(other) = folders.iter().find(|f| f.folder.name == name).cloned() {
						// `<other-name>/<rest>` — strip the basename
						// and route to that folder. Bare `<other-name>`
						// (no tail) becomes `.`.
						let tail_str = path
							.strip_prefix(name)
							.map(|t| t.as_str().to_string())
							.unwrap_or_default();
						let resolved = if tail_str.is_empty() { ".".to_string() } else { tail_str };
						return Ok((other, resolved));
					}
				}
				Ok((cx.folder.clone(), raw.to_string()))
			}
			_ => Ok((cx.folder.clone(), raw.to_string())),
		}
	}

	/// Tool definitions to advertise to the model on every chat call.
	///
	/// `web_search` is gated on a configured Tavily API key: with no
	/// key the model never sees the definition, so it can't be tempted
	/// to call a tool that's guaranteed to error. `web_fetch` is
	/// always advertised — Jina Reader's free tier needs no key.
	pub fn definitions(&self) -> Vec<ToolDefinition> {
		let mut defs = vec![
			ToolDefinition::function(
				"read_file",
				"Read the contents of a file in any currently-bound workspace folder. Returns the file's text, with each line prefixed by `<line_number>|<line>`. Treat the prefix as metadata — it is not part of the file. Optional `start_line` / `end_line` (1-based, inclusive) read just a slice; both omitted means read the whole file (capped at 200 kB).",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Either a path inside the active workspace folder (`src/foo.rs`), or a synthetic `/workspace/<name>/src/foo.rs` to address any other currently-bound folder. Both forms work the same way; the latter is how you reach files outside the active folder."
						},
						"start_line": {
							"type": "integer",
							"description": "First line to include, 1-based and inclusive. Omit to start at line 1."
						},
						"end_line": {
							"type": "integer",
							"description": "Last line to include, 1-based and inclusive. Clamped silently if past EOF. Omit to read to EOF."
						}
					},
					"required": ["path"]
				}),
			),
			ToolDefinition::function(
				"list_dir",
				"List the immediate contents of a directory in any currently-bound workspace folder. Returns one entry per line in `kind  name` form.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "`.` for the active folder root; a relative path (`src/`) for an active-folder subtree; or `/workspace/<name>/...` to list inside any other currently-bound folder.",
							"default": "."
						}
					}
				}),
			),
			ToolDefinition::function(
				"grep",
				"Regex search across the workspace folder, gitignore-aware. Returns one match per line in `path:line: match` form. The `line` field is the 1-based line number, so a follow-up `read_file` can target it directly via `start_line` / `end_line`.",
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
							"description": "Shell command, executed via `bash -lc <cmd>`."
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
				"Overwrite a file with new content (or create it if missing) in any currently-bound workspace folder. Use for new files or whole-file rewrites; prefer `edit_file` for surgical changes inside a large file. The file's parent directory must already exist.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Path in the target folder. Either active-folder-relative (`src/foo.rs`) or synthetic `/workspace/<name>/...` to write into any other currently-bound folder. Created if it does not exist."
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
				"Replace an exact substring inside a file in any currently-bound workspace folder. `find` must match the file (whitespace tolerant) and must be unique unless `occurrence` is given. To insert text, set `find` to the line you want to insert before/after and include it in `replace`. To delete, set `replace` to an empty string. Failure throws — when it does, retry with more surrounding context in `find` so the match becomes unique.",
				json!({
					"type": "object",
					"properties": {
						"path": {
							"type": "string",
							"description": "Path in the target folder. Either active-folder-relative or synthetic `/workspace/<name>/...` to edit a file in any other currently-bound folder. The file must already exist."
						},
						"find": {
							"type": "string",
							"description": "Exact substring to locate. No regex; whitespace tolerant."
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
			ToolDefinition::function(
				"web_fetch",
				"Fetch a single web page and return its main content as clean Markdown. Backed by Jina Reader — strips boilerplate, preserves headings / links / code blocks. Use this to read documentation, blog posts, RFCs, release notes, or any URL surfaced by `web_search`. Only `http`/`https` URLs are accepted. Long pages are truncated at ~200 kB; if `truncated` is true, fetch a more specific sub-page rather than re-fetching the same URL.",
				json!({
					"type": "object",
					"properties": {
						"url": {
							"type": "string",
							"description": "Absolute http or https URL to fetch."
						}
					},
					"required": ["url"]
				}),
			),
		];
		if self.web.has_tavily_key() {
			defs.push(ToolDefinition::function(
				"web_search",
				"Search the open web. Returns a small list of `{ title, url, snippet }` entries (plus `published_date` when known) sorted by Tavily's relevance ranking. Use this when you need information that might be missing or outdated in your training data — recent releases, API docs you don't already know, error messages quoted online, news, package changelogs. After picking a promising URL, call `web_fetch` on it for the full page. Don't use `web_search` for facts you're confident about, and don't use it for anything inside the workspace — that's what `grep` / `read_file` / `bash` are for.",
				json!({
					"type": "object",
					"properties": {
						"query": {
							"type": "string",
							"description": "Free-form search query, same way you'd type into a search engine. Be specific — include version numbers, language names, error message fragments."
						},
						"max_results": {
							"type": "integer",
							"description": "Maximum number of results to return. Defaults to 8; capped at 20."
						}
					},
					"required": ["query"]
				}),
			));
		}
		defs
	}

	pub async fn dispatch(
		&self,
		name: &str,
		args: &Value,
		cx: &ToolContext,
		cancel: &CancellationToken,
	) -> Result<Value, CoderError> {
		match name {
			"read_file" => self.read_file(args, cx).await,
			"list_dir" => self.list_dir(args, cx).await,
			"grep" => self.grep(args, cx).await,
			// `bash` deliberately is *not* mode-gated: a Research
			// sub-agent gets to run inspection commands (`git log`,
			// `cargo check`, `pytest --collect-only`, …). The
			// "don't mutate" half is enforced via the sub-agent's
			// system prompt — see Phase C's `run_subagent`.
			"bash" => self.bash(args, cx, cancel).await,
			"write_file" => {
				if !cx.mode.allows_writes() {
					return Err(CoderError::read_only_mode("write_file"));
				}
				self.write_file(args, cx).await
			}
			"edit_file" => {
				if !cx.mode.allows_writes() {
					return Err(CoderError::read_only_mode("edit_file"));
				}
				self.edit_file(args, cx).await
			}
			// Web tools are intentionally *not* mode-gated: a
			// Research sub-agent reading the open web is exactly
			// the kind of read-only inspection the mode exists for.
			"web_search" => self.web_search(args, cancel).await,
			"web_fetch" => self.web_fetch(args, cancel).await,
			other => Err(CoderError::UnknownTool(other.to_string())),
		}
	}

	async fn read_file(&self, args: &Value, cx: &ToolContext) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct ReadFileArgs {
			path: String,
			#[serde(default)]
			start_line: Option<u32>,
			#[serde(default)]
			end_line: Option<u32>,
		}
		let parsed: ReadFileArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("read_file", err.to_string()))?;
		if let (Some(start), Some(end)) = (parsed.start_line, parsed.end_line) {
			if start == 0 || end < start {
				return Err(CoderError::invalid_args(
					"read_file",
					"start_line / end_line must be 1-based and end_line >= start_line",
				));
			}
		}
		if matches!(parsed.start_line, Some(0)) || matches!(parsed.end_line, Some(0)) {
			return Err(CoderError::invalid_args("read_file", "line numbers are 1-based"));
		}
		let (folder, resolved_path) = self.resolve_workspace_path(&parsed.path, cx, "read_file").await?;
		let result = folder.host.read_file(Utf8Path::new(&resolved_path)).await?;
		if result.is_binary {
			return Err(CoderError::tool_failed("read_file", "binary file"));
		}
		let total_lines = if result.text.is_empty() {
			0
		} else {
			result.text.lines().count() as u32
		};
		let start_line = parsed.start_line.unwrap_or(1);
		let end_line = parsed.end_line.unwrap_or(u32::MAX);
		let (rendered, byte_truncated) = format_numbered_lines(&result.text, start_line, end_line);
		Ok(json!({
			"path": parsed.path,
			"content": rendered,
			"start_line": start_line,
			// `end_line` in the response is the *effective* end after
			// clamping to EOF, so the model can tell when its
			// requested range was shorter than asked for.
			"end_line": end_line.min(total_lines.max(1)),
			"total_lines": total_lines,
			"truncated": byte_truncated,
			"mtime_ms": result.mtime_ms,
		}))
	}

	async fn list_dir(&self, args: &Value, cx: &ToolContext) -> Result<Value, CoderError> {
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
		let (folder, resolved_path) = self.resolve_workspace_path(&parsed.path, cx, "list_dir").await?;
		let entries = folder.host.read_dir(Utf8Path::new(&resolved_path)).await?;
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

	async fn grep(&self, args: &Value, cx: &ToolContext) -> Result<Value, CoderError> {
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
		let folder = &cx.folder;
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

	async fn write_file(&self, args: &Value, cx: &ToolContext) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct WriteFileArgs {
			path: String,
			content: String,
		}
		let parsed: WriteFileArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("write_file", err.to_string()))?;
		let (folder, resolved_path) = self.resolve_workspace_path(&parsed.path, cx, "write_file").await?;
		let result = folder
			.host
			.save_file(Utf8Path::new(&resolved_path), &parsed.content)
			.await?;
		Ok(json!({
			"path": parsed.path,
			"bytes_written": parsed.content.len(),
			"mtime_ms": result.mtime_ms,
		}))
	}

	async fn edit_file(&self, args: &Value, cx: &ToolContext) -> Result<Value, CoderError> {
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
		let (folder, resolved_path) = self.resolve_workspace_path(&parsed.path, cx, "edit_file").await?;
		let path = Utf8Path::new(&resolved_path);
		let original = folder.host.read_file(path).await?;
		if original.is_binary {
			return Err(CoderError::tool_failed("edit_file", "binary file"));
		}

		let plan = locate_edit(
			&original.text,
			&parsed.find,
			&parsed.replace,
			parsed.occurrence,
			&parsed.path,
		)?;

		let mut new_text = String::with_capacity(original.text.len() - (plan.end - plan.start) + plan.replace_text.len());
		new_text.push_str(&original.text[..plan.start]);
		new_text.push_str(&plan.replace_text);
		new_text.push_str(&original.text[plan.end..]);

		let result = folder.host.save_file(path, &new_text).await?;
		Ok(json!({
			"path": parsed.path,
			"replaced_at_byte": plan.start,
			"bytes_written": new_text.len(),
			"mtime_ms": result.mtime_ms,
			"occurrence": plan.occurrence,
			"total_matches": plan.total_matches,
			"match_mode": plan.mode,
		}))
	}

	async fn web_search(&self, args: &Value, cancel: &CancellationToken) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct WebSearchArgs {
			query: String,
			#[serde(default)]
			max_results: Option<u32>,
		}
		let parsed: WebSearchArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("web_search", err.to_string()))?;
		let max_results = parsed.max_results.unwrap_or_else(WebClient::default_search_max_results);
		let results = self.web.search(&parsed.query, max_results, cancel).await?;
		let count = results.len();
		Ok(json!({
			"query": parsed.query,
			"results": results,
			"count": count,
		}))
	}

	async fn web_fetch(&self, args: &Value, cancel: &CancellationToken) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct WebFetchArgs {
			url: String,
		}
		let parsed: WebFetchArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("web_fetch", err.to_string()))?;
		let fetched = self.web.fetch(&parsed.url, cancel).await?;
		Ok(json!({
			"url": fetched.url,
			"markdown": fetched.markdown,
			"truncated": fetched.truncated,
			"bytes": fetched.markdown.len(),
		}))
	}

	async fn bash(&self, args: &Value, cx: &ToolContext, cancel: &CancellationToken) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct BashArgs {
			cmd: String,
			#[serde(default)]
			timeout_ms: Option<u64>,
		}
		let parsed: BashArgs =
			serde_json::from_value(args.clone()).map_err(|err| CoderError::invalid_args("bash", err.to_string()))?;
		let folder = &cx.folder;
		let timeout = parsed
			.timeout_ms
			.map(Duration::from_millis)
			.unwrap_or(BASH_DEFAULT_TIMEOUT)
			.min(BASH_MAX_TIMEOUT);

		let (mut command, target_kind) = self.build_bash_command(folder, &parsed.cmd).await?;
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
	/// folder. Routing decision flows through
	/// [`resolve_bash_target`] so it stays in lockstep with what the
	/// panel header / `CoderStatus.bash_target` advertises:
	///
	/// - **Container** (workspace shell container is `Running`):
	///   `docker exec -w <container_cwd> <name> bash -lc <cmd>`.
	///   Reuses `moon_terminal::container_name_for_workspace` +
	///   `TerminalTarget::container_cwd_for_folder` so the framing
	///   matches terminals and LSP exactly.
	/// - **Host** (otherwise): `bash -lc <cmd>` rooted at the folder.
	///
	/// **Why `bash -lc` and not `sh -lc`.** On most modern Linuxes
	/// `/bin/sh` is `dash`, which as a login shell reads only
	/// `~/.profile`. Most dev toolchains (rustup, fnm, mise,
	/// pyenv, …) put their PATH-extending env line in `~/.bashrc`
	/// — sometimes additionally in `~/.profile`, often not.
	/// Result: `sh -lc 'cargo …'` returns "cargo: not found" even
	/// though the user's interactive terminal has cargo on PATH.
	/// `bash -lc` reads `~/.bash_profile` (which on almost every
	/// dev box sources `~/.bashrc`), so the tool's PATH matches
	/// the terminal's. Trade-off: requires `bash` to exist in the
	/// container — true for every dev image we care about, since
	/// terminals (`moon-terminal::target`) already assume it.
	async fn build_bash_command(
		&self,
		folder: &WorkspaceFolderEntry,
		cmd: &str,
	) -> Result<(tokio::process::Command, &'static str), CoderError> {
		let target = resolve_bash_target(&self.workspaces, &self.workspaces_dir).await;
		if target == BASH_TARGET_CONTAINER {
			let workspace_id = self.workspaces.workspace_id().await;
			let container_name = container_name_for_workspace(&workspace_id);
			// Fall back to `/workspace` if the host path has no
			// basename — same fallback `moon-terminal` uses for
			// pathological inputs (`/`).
			let container_cwd = TerminalTarget::container_cwd_for_folder(Utf8Path::new(&folder.folder.path))
				.unwrap_or_else(|| Utf8PathBuf::from("/workspace"));
			// `docker exec` (no `-it`): we want captured
			// stdout/stderr, not a TTY. Terminals get `-it`; the
			// bash tool doesn't.
			let mut command = tokio::process::Command::new("docker");
			command
				.arg("exec")
				.arg("-w")
				.arg(container_cwd.as_str())
				.arg(&container_name)
				.arg("bash")
				.arg("-lc")
				.arg(cmd);
			return Ok((command, BASH_TARGET_CONTAINER));
		}
		let mut command = tokio::process::Command::new("bash");
		command.arg("-lc").arg(cmd).current_dir(folder.folder.path.as_str());
		Ok((command, BASH_TARGET_HOST))
	}
}

/// Single source of truth for "should bash route through the
/// workspace shell container?". Mirrors `lsp.rs::resolve_target`
/// almost line-for-line: build a [`ContainerWorkspace`] from the
/// current bound-folder set + workspace id, ask its lifecycle
/// `status()`, and route to the container only if the project is
/// `Running`. Any failure (no compose project, daemon
/// unreachable, parse error) falls back to host — the agent's
/// bash should never become unusable just because docker
/// isn't responding.
///
/// Called from both `tools::bash` and `runner::status` so the
/// indicator pip and the actual command's `target` field can't
/// drift.
pub(crate) async fn resolve_bash_target(workspaces: &WorkspaceRegistry, workspaces_dir: &Utf8Path) -> &'static str {
	let workspace_id = workspaces.workspace_id().await;
	let bound: Vec<Utf8PathBuf> = workspaces
		.folders()
		.await
		.iter()
		.map(|entry| Utf8PathBuf::from(&entry.folder.path))
		.collect();
	let ws = match ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: workspace_id.clone(),
		state_dir: workspaces_dir.join(&workspace_id),
		bound_folders: bound,
	}) {
		Ok(ws) => ws,
		Err(err) => {
			tracing::debug!(%err, "coder: container config unavailable, routing bash to host");
			return BASH_TARGET_HOST;
		}
	};
	match ws.status().await {
		Ok(status) if matches!(status.state, ContainerState::Running) => BASH_TARGET_CONTAINER,
		Ok(_) => BASH_TARGET_HOST,
		Err(err) => {
			tracing::debug!(%err, "coder: container status query failed, routing bash to host");
			BASH_TARGET_HOST
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
/// Build the "no bound folder named `<name>`" error returned by
/// [`ToolRegistry::resolve_workspace_path`] when a `/workspace/<name>/...`
/// path doesn't match any currently-bound folder. The error lists what
/// *is* bound so the model can self-correct without another guess turn.
fn unbound_folder_error(
	tool: &'static str,
	raw_path: &str,
	requested: &str,
	folders: &[Arc<WorkspaceFolderEntry>],
) -> CoderError {
	let bound = folders
		.iter()
		.map(|f| format!("`{}`", f.folder.name))
		.collect::<Vec<_>>()
		.join(", ");
	let bound_clause = if bound.is_empty() {
		"no folders are currently bound".to_string()
	} else {
		format!("currently bound: {bound}")
	};
	CoderError::tool_failed(
		tool,
		format!(
			"`{raw_path}` references the workspace `{requested}`, but no folder is bound under that name \
({bound_clause}). Use one of the bound folder basenames in the `/workspace/<name>/...` form, or use a \
plain relative path to address the active folder."
		),
	)
}

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

/// The match `edit_file` ultimately commits to. Carries the byte
/// range to splice out, the replacement text (already re-indented if
/// the fuzzy path adjusted it), and metadata for the JSON result.
#[cfg_attr(test, derive(Debug))]
struct EditPlan {
	start: usize,
	end: usize,
	replace_text: String,
	total_matches: usize,
	occurrence: usize,
	/// Which matcher succeeded. Surfaced verbatim in the tool result
	/// so the model — and any human reading the session log — can see
	/// when a fallback kicked in. The strings are part of the tool
	/// protocol: `"exact"`, `"fuzzy_indent"`, `"fuzzy_unescape"`.
	mode: &'static str,
}

/// Locate `find` in `text` using a layered match. Returns the byte
/// range to splice plus the replacement bytes to write back.
///
/// 1. **Exact** — `str::find` against the file verbatim. Same
///    behaviour `edit_file` has always had; covers every case where
///    the model gets `find` byte-perfect on the first try.
/// 2. **Unescape fallback** — if `find` contains the literal 2-char
///    sequences `\\n` / `\\t` (the model's escape-leakage failure
///    mode — see `specs/test-plans/0068-edit-file-fuzzy-fallback.md`)
///    and the unescaped form matches exactly while the original
///    doesn't, treat that as the intended pattern. `replace` is
///    unescaped in the same way so the splice is consistent.
/// 3. **Indent-tolerant fallback** — strip per-line leading whitespace
///    from both `find` and the file's lines, look for a line-aligned
///    match. On success, splice the *original* file lines' byte range
///    and re-indent `replace` so its first non-blank line lines up
///    with the file's match indent. This catches the "model is off
///    by one tab depth" failure mode without weakening exact-match
///    semantics — only kicks in when the strict match misses.
///
/// The fuzzy paths assume format-on-save will catch any residual
/// indentation skew in `replace`. Without a formatter the edit may
/// land with the model's exact (possibly mis-indented) replacement
/// bytes shifted by the indent delta; that's the deliberate trade
/// — fewer "find not found" loops at the cost of a one-off re-indent
/// the formatter will normalise anyway.
fn locate_edit(
	text: &str,
	find: &str,
	replace: &str,
	occurrence: Option<usize>,
	path_for_error: &str,
) -> Result<EditPlan, CoderError> {
	// Stage 1: exact match. Hot path; returns immediately on success.
	let exact = byte_offsets_of(text, find);
	if !exact.is_empty() {
		let (idx, picked) = select_match(&exact, occurrence, path_for_error, find.len(), text)?;
		return Ok(EditPlan {
			start: idx,
			end: idx + find.len(),
			replace_text: replace.to_owned(),
			total_matches: exact.len(),
			occurrence: picked,
			mode: "exact",
		});
	}

	// Stage 2: escape-leakage. Only kicks in when `find` actually
	// contains a literal `\n` / `\t` pair — otherwise the unescape is
	// a no-op and we'd just retry the same query.
	if has_literal_escape(find) {
		let unescaped_find = unescape_literals(find);
		let m = byte_offsets_of(text, &unescaped_find);
		if !m.is_empty() {
			let unescaped_replace = unescape_literals(replace);
			let (idx, picked) = select_match(&m, occurrence, path_for_error, unescaped_find.len(), text)?;
			return Ok(EditPlan {
				start: idx,
				end: idx + unescaped_find.len(),
				replace_text: unescaped_replace,
				total_matches: m.len(),
				occurrence: picked,
				mode: "fuzzy_unescape",
			});
		}
	}

	// Stage 3: per-line indent-tolerant. Only well-defined when
	// `find` is line-aligned (every line is on its own); a mid-line
	// `find` falls through to the no-match error below.
	let fuzzy = find_indent_tolerant(text, find);
	if !fuzzy.is_empty() {
		let (chosen, picked) = select_fuzzy(&fuzzy, occurrence, path_for_error, text)?;
		let replace_text = reindent_replacement(replace, &chosen.find_indent, &chosen.file_indent);
		return Ok(EditPlan {
			start: chosen.start,
			end: chosen.end,
			replace_text,
			total_matches: fuzzy.len(),
			occurrence: picked,
			mode: "fuzzy_indent",
		});
	}

	Err(CoderError::tool_failed(
		"edit_file",
		format!(
			"`find` not found in {path_for_error}. The file's bytes did not match `find` exactly, and no \
indent-tolerant match was found either. Re-run `read_file` to see the current state of the file and pass \
`find` with the same indentation (tabs vs. spaces, count) and line endings the file actually uses."
		),
	))
}

/// Resolve the index of the match to commit to, given the full list
/// of byte offsets and the caller's optional `occurrence` selector.
/// Returns `(byte_offset, 1-based-occurrence)`.
///
/// `find_len` and `text` aren't used by the picker today but are
/// kept in the signature for symmetry with `select_fuzzy` (which
/// needs `text` to format line numbers in the multi-match error).
fn select_match(
	matches: &[usize],
	occurrence: Option<usize>,
	path: &str,
	_find_len: usize,
	text: &str,
) -> Result<(usize, usize), CoderError> {
	match (matches.len(), occurrence) {
		(0, _) => unreachable!("select_match called with empty matches"),
		(1, None | Some(1)) => Ok((matches[0], 1)),
		(n, None) => {
			let lines: Vec<u32> = matches.iter().map(|&off| line_number_at_byte(text, off)).collect();
			let lines_csv = lines.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
			Err(CoderError::tool_failed(
				"edit_file",
				format!(
					"`find` matched {n} times in {path} (at lines {lines_csv}); pass `occurrence` \
(1-based) or include more surrounding context"
				),
			))
		}
		(n, Some(idx)) if idx == 0 || idx > n => Err(CoderError::tool_failed(
			"edit_file",
			format!("occurrence {idx} out of range — `find` matched {n} times"),
		)),
		// `idx >= 1` and `idx <= n` here, so the subtraction can't
		// underflow. `matches[idx - 1]` is always in bounds.
		(_, Some(idx)) => Ok((matches[idx - 1], idx)),
	}
}

fn select_fuzzy<'a>(
	matches: &'a [FuzzyMatch],
	occurrence: Option<usize>,
	path: &str,
	text: &str,
) -> Result<(&'a FuzzyMatch, usize), CoderError> {
	match (matches.len(), occurrence) {
		(0, _) => unreachable!("select_fuzzy called with empty matches"),
		(1, None | Some(1)) => Ok((&matches[0], 1)),
		(n, None) => {
			let lines: Vec<u32> = matches.iter().map(|m| line_number_at_byte(text, m.start)).collect();
			let lines_csv = lines.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
			Err(CoderError::tool_failed(
				"edit_file",
				format!(
					"`find` indent-tolerant match was ambiguous in {path} ({n} hits at lines \
{lines_csv}); pass `occurrence` (1-based) or include more surrounding context"
				),
			))
		}
		(n, Some(idx)) if idx == 0 || idx > n => Err(CoderError::tool_failed(
			"edit_file",
			format!("occurrence {idx} out of range — `find` matched {n} times"),
		)),
		(_, Some(idx)) => Ok((&matches[idx - 1], idx)),
	}
}

fn line_number_at_byte(text: &str, offset: usize) -> u32 {
	// 1-based line count: bytes preceding `offset` plus one for the
	// line we're sitting on. `bytecount`-free implementation; the
	// inputs here are LLM-call-sized (file ≤ ~200 KB) so the scan
	// cost is irrelevant.
	let upto = offset.min(text.len());
	(text[..upto].bytes().filter(|&b| b == b'\n').count() + 1) as u32
}

/// True when `find` contains a literal two-character `\n` or `\t`
/// sequence (backslash + letter) that an LLM might have meant as the
/// control character. Cheap pre-check so the unescape stage only
/// runs when there's actually something to unescape.
fn has_literal_escape(find: &str) -> bool {
	let bytes = find.as_bytes();
	let mut i = 0;
	while i + 1 < bytes.len() {
		if bytes[i] == b'\\' && (bytes[i + 1] == b'n' || bytes[i + 1] == b't') {
			return true;
		}
		i += 1;
	}
	false
}

/// Translate literal `\n` / `\t` 2-char sequences into the
/// corresponding control characters. We intentionally do **not**
/// touch `\\` — the model rarely means to embed a literal
/// backslash in `find` (real backslashes don't survive its own
/// thought-to-JSON pipeline as `\\\\`), and translating it would
/// confuse the rare case of someone editing a regex / printf
/// string.
fn unescape_literals(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut bytes = s.bytes().peekable();
	while let Some(b) = bytes.next() {
		if b == b'\\' {
			match bytes.peek() {
				Some(b'n') => {
					bytes.next();
					out.push('\n');
					continue;
				}
				Some(b't') => {
					bytes.next();
					out.push('\t');
					continue;
				}
				_ => {}
			}
		}
		out.push(b as char);
	}
	out
}

/// A single indent-tolerant hit. `start..end` is the byte range in
/// the original file that the splice will replace; `file_indent` is
/// the leading whitespace on the file's first matched line, and
/// `find_indent` is the corresponding leading whitespace on `find`'s
/// first non-blank line. Both indents are used to re-indent the
/// caller's `replace` text.
struct FuzzyMatch {
	start: usize,
	end: usize,
	file_indent: String,
	find_indent: String,
}

/// Per-line indent-tolerant match. Splits `find` into lines, strips
/// the common leading whitespace from each non-blank line, then walks
/// the file looking for a window whose lines match after the same
/// per-line strip. The window is aligned to file line boundaries on
/// both ends so the resulting splice is itself line-aligned (which
/// keeps the re-indent of `replace` well-defined).
///
/// Single-line `find` is the easy case — match the trimmed line, take
/// the file's leading whitespace as `file_indent`, take `find`'s
/// leading whitespace as `find_indent`. Multi-line: same idea,
/// per-line, with the constraint that every non-blank line of `find`
/// matches the corresponding file line after its own leading
/// whitespace is stripped. Blank lines in `find` match any blank
/// (or whitespace-only) line in the file.
fn find_indent_tolerant(text: &str, find: &str) -> Vec<FuzzyMatch> {
	let find_lines: Vec<&str> = find.split('\n').collect();
	if find_lines.is_empty() {
		return Vec::new();
	}
	// Drop a trailing empty element from a `find` that ends with `\n`
	// so the per-line walk doesn't try to match a phantom empty line
	// past the end of the window. If `find` was just `\n` (one blank
	// line) we still leave one element so the count stays meaningful.
	let find_lines = if find_lines.len() > 1 && find_lines.last() == Some(&"") {
		&find_lines[..find_lines.len() - 1]
	} else {
		&find_lines[..]
	};
	if find_lines.is_empty() {
		return Vec::new();
	}

	// `find`'s indent is the leading whitespace of its first
	// non-blank line. Falling back to "" when every line is blank
	// (degenerate `find`, but we shouldn't crash on it).
	let find_indent = find_lines
		.iter()
		.find(|l| !l.trim().is_empty())
		.map(|l| leading_whitespace(l).to_owned())
		.unwrap_or_default();

	// Pre-compute file line spans (start byte, end-without-newline
	// byte) so the inner loop can splice without re-scanning.
	let file_lines = collect_line_spans(text);
	if file_lines.len() < find_lines.len() {
		return Vec::new();
	}

	let mut hits = Vec::new();
	let limit = file_lines.len() - find_lines.len() + 1;
	for start_idx in 0..limit {
		let Some(file_indent) = file_indent_at(text, &file_lines[start_idx], find_lines[0]) else {
			continue;
		};
		let mut ok = true;
		for (offset, find_line) in find_lines.iter().enumerate() {
			let file_line = &text[file_lines[start_idx + offset].0..file_lines[start_idx + offset].1];
			if !lines_match_after_dedent(file_line, find_line) {
				ok = false;
				break;
			}
		}
		if !ok {
			continue;
		}
		let span_start = file_lines[start_idx].0;
		let span_end = file_lines[start_idx + find_lines.len() - 1].1;
		hits.push(FuzzyMatch {
			start: span_start,
			end: span_end,
			file_indent,
			find_indent: find_indent.clone(),
		});
	}
	hits
}

/// Indent of the file's first matched line, *or* `None` when the
/// model's first `find` line is blank (in which case we don't have a
/// meaningful anchor; the next iteration will try the line below).
/// Also bails when the file line is shorter than the model's
/// expected post-dedent content — a cheap pre-filter before the full
/// `lines_match_after_dedent` runs.
fn file_indent_at(text: &str, span: &(usize, usize), first_find_line: &str) -> Option<String> {
	let file_line = &text[span.0..span.1];
	let trimmed_find = first_find_line.trim_start_matches([' ', '\t']);
	if trimmed_find.is_empty() {
		return None;
	}
	let trimmed_file = file_line.trim_start_matches([' ', '\t']);
	if trimmed_file.is_empty() {
		return None;
	}
	if !trimmed_file.starts_with(trimmed_find) && trimmed_file != trimmed_find.trim_end() {
		return None;
	}
	Some(leading_whitespace(file_line).to_owned())
}

fn lines_match_after_dedent(file_line: &str, find_line: &str) -> bool {
	let f = file_line.trim_start_matches([' ', '\t']).trim_end_matches([' ', '\t']);
	let n = find_line.trim_start_matches([' ', '\t']).trim_end_matches([' ', '\t']);
	if n.is_empty() {
		// Blank `find` line matches any blank or whitespace-only
		// file line. (Both halves are empty after the trim_*.)
		return f.is_empty();
	}
	f == n
}

fn leading_whitespace(line: &str) -> &str {
	let end = line.bytes().position(|b| b != b' ' && b != b'\t').unwrap_or(line.len());
	&line[..end]
}

/// `(start, end_without_newline)` byte offsets for every line in
/// `text`, including a trailing empty line when the file ends with
/// `\n` (so the count matches `text.lines().count() + 1` for
/// newline-terminated files). The "end without newline" form is what
/// the per-line splice wants: the trailing `\n` lives outside the
/// matched span so the file's existing line structure is preserved.
fn collect_line_spans(text: &str) -> Vec<(usize, usize)> {
	let mut spans = Vec::new();
	let bytes = text.as_bytes();
	let mut line_start = 0;
	for (i, &b) in bytes.iter().enumerate() {
		if b == b'\n' {
			spans.push((line_start, i));
			line_start = i + 1;
		}
	}
	if line_start < bytes.len() {
		spans.push((line_start, bytes.len()));
	}
	spans
}

/// Apply the file-vs-find indent delta to `replace` so the spliced
/// bytes line up with the file's indentation at the match point.
///
/// - `file_indent` longer than `find_indent` and starts with it →
///   the model under-indented; prepend the extra prefix to every
///   non-blank line of `replace`.
/// - `find_indent` longer than `file_indent` and starts with it →
///   the model over-indented; strip the extra prefix from every
///   non-blank line of `replace` that has it. Lines without that
///   prefix are left as-is (defensive — the model can stay
///   internally consistent if it deepens nesting inside `replace`).
/// - Indents differ in *shape* (tab vs. spaces, mixed): leave
///   `replace` alone. Better to let format-on-save sort it than to
///   silently mutate the model's text on a guess.
fn reindent_replacement(replace: &str, find_indent: &str, file_indent: &str) -> String {
	if find_indent == file_indent {
		return replace.to_owned();
	}
	if let Some(extra) = file_indent.strip_prefix(find_indent) {
		return prepend_per_nonblank_line(replace, extra);
	}
	if let Some(extra) = find_indent.strip_prefix(file_indent) {
		return strip_per_nonblank_line(replace, extra);
	}
	replace.to_owned()
}

fn prepend_per_nonblank_line(text: &str, prefix: &str) -> String {
	if prefix.is_empty() {
		return text.to_owned();
	}
	let mut out = String::with_capacity(text.len() + prefix.len() * 4);
	for (i, line) in text.split_inclusive('\n').enumerate() {
		// First line always gets the prefix when non-blank — the
		// caller's invariant is that `replace`'s first content line
		// corresponds to the file's matched first line.
		let trimmed = line.trim_end_matches('\n');
		if trimmed.is_empty() {
			out.push_str(line);
			continue;
		}
		let _ = i; // suppress unused warning; kept for future per-line policy hooks.
		out.push_str(prefix);
		out.push_str(line);
	}
	out
}

fn strip_per_nonblank_line(text: &str, prefix: &str) -> String {
	if prefix.is_empty() {
		return text.to_owned();
	}
	let mut out = String::with_capacity(text.len());
	for line in text.split_inclusive('\n') {
		let trimmed = line.trim_end_matches('\n');
		if trimmed.is_empty() || !trimmed.starts_with(prefix) {
			out.push_str(line);
			continue;
		}
		out.push_str(&line[prefix.len()..]);
	}
	out
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

/// Render `text` as `<line_no>|<line>` pairs, restricted to the
/// 1-based inclusive range `[start_line, end_line]`. Out-of-range
/// `end_line` values clamp silently to EOF; an empty file yields an
/// empty string. The byte cap [`READ_FILE_MAX_BYTES`] applies to
/// the rendered string — long ranges that go past it are cut at a
/// char boundary and `truncated == true` is returned so the agent
/// can ask for a smaller window.
fn format_numbered_lines(text: &str, start_line: u32, end_line: u32) -> (String, bool) {
	use std::fmt::Write as _;
	if text.is_empty() {
		return (String::new(), false);
	}
	let total: u32 = text.lines().count() as u32;
	if total == 0 || start_line > total {
		return (String::new(), false);
	}
	let effective_end = end_line.min(total);
	// Right-align line numbers to the width of the largest one in
	// the rendered range. Keeps narrow files narrow and widens up
	// to whatever the file actually needs for big ones.
	let width = digit_width(effective_end) as usize;
	let mut out = String::new();
	let mut truncated = false;
	for (idx, line) in text.lines().enumerate() {
		let line_no = (idx + 1) as u32;
		if line_no < start_line {
			continue;
		}
		if line_no > effective_end {
			break;
		}
		let _ = writeln!(out, "{line_no:>width$}|{line}");
		if out.len() > READ_FILE_MAX_BYTES {
			let cut = clamp_to_char_boundary(&out, READ_FILE_MAX_BYTES);
			out.truncate(cut);
			truncated = true;
			break;
		}
	}
	(out, truncated)
}

fn digit_width(mut n: u32) -> u32 {
	if n == 0 {
		return 1;
	}
	let mut w = 0;
	while n > 0 {
		n /= 10;
		w += 1;
	}
	w
}

/// Hard cap on the number of UTF-8 characters of a matched line we'll dump
/// into the model's context. A single base64-embedded image, minified JS
/// bundle, or pretty-printed JSON blob can produce a "line" that's tens of
/// kilobytes long; without this cap a single `grep` call could blow the
/// context window. 500 chars is generous for normal code (well past
/// rustfmt's 100-column default and prettier's 80) while keeping a hit on
/// an inlined base64 payload to a stub the model can still act on (the
/// `path:line` is intact, so a follow-up `read_file` with `start_line` /
/// `end_line` is the natural escape hatch).
const GREP_MAX_LINE_CHARS: usize = 500;

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
		out.push_str(&truncate_grep_line(trimmed));
		out.push('\n');
	}
	out
}

/// Cap a matched line at [`GREP_MAX_LINE_CHARS`] UTF-8 characters. Lines
/// at or under the cap pass through unchanged; longer lines get the cap
/// prefix plus a `[…line truncated, N chars total]` marker so the model
/// can see the line is huge and reach for `read_file` if it needs the
/// rest. Counting in characters (not bytes) keeps multi-byte UTF-8 from
/// landing the slice mid-codepoint.
fn truncate_grep_line(line: &str) -> std::borrow::Cow<'_, str> {
	let mut count = 0usize;
	let mut cut_byte = None;
	for (idx, _) in line.char_indices() {
		if count == GREP_MAX_LINE_CHARS {
			cut_byte = Some(idx);
			break;
		}
		count += 1;
	}
	match cut_byte {
		None => std::borrow::Cow::Borrowed(line),
		Some(cut) => {
			let total = count + line[cut..].chars().count();
			std::borrow::Cow::Owned(format!("{}… [line truncated, {total} chars total]", &line[..cut]))
		}
	}
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
	use super::{
		byte_offsets_of, format_grep_hits, format_numbered_lines, locate_edit, truncate_grep_line, GREP_MAX_LINE_CHARS,
	};
	use moon_protocol::search::ContentSearchHit;

	#[test]
	fn locate_edit_exact_match_returns_byte_range_and_replacement() {
		// Hot path: model's `find` byte-matches the file. Splice
		// covers exactly `find.len()` bytes; `replace_text` is
		// returned verbatim so existing tests / callers keep their
		// invariants.
		let text = "alpha\nbeta\ngamma\n";
		let plan = locate_edit(text, "beta", "BETA", None, "test.txt").expect("exact match");
		assert_eq!(plan.start, 6);
		assert_eq!(plan.end, 10);
		assert_eq!(plan.replace_text, "BETA");
		assert_eq!(plan.mode, "exact");
		assert_eq!(plan.total_matches, 1);
		assert_eq!(plan.occurrence, 1);
	}

	#[test]
	fn locate_edit_indent_fallback_prepends_missing_tabs() {
		// file has 3-tab indent on the `error: {` line, model wrote `find` with
		// 2 tabs. The fuzzy match should still locate the block
		// and the spliced `replace` should pick up the extra tab
		// so the file's indentation is preserved without relying
		// on the formatter.
		let file = "fn main() {\n\t\t\terror: {\n\t\t\t\tname: x,\n\t\t\t},\n}\n";
		let find = "\t\terror: {\n\t\t\tname: x,\n\t\t},";
		let replace = "\t\terror: {\n\t\t\tname: x,\n\t\t\tstatus: 500,\n\t\t},";
		let plan = locate_edit(file, find, replace, None, "test.rs").expect("fuzzy indent match");
		assert_eq!(plan.mode, "fuzzy_indent");
		// Splice covers the file's actual matched lines (3-tab
		// indent), not the model's would-be `find` bytes.
		assert_eq!(
			&file[plan.start..plan.end],
			"\t\t\terror: {\n\t\t\t\tname: x,\n\t\t\t},"
		);
		// Replacement is shifted by the missing tab on every
		// non-blank line.
		assert_eq!(
			plan.replace_text,
			"\t\t\terror: {\n\t\t\t\tname: x,\n\t\t\t\tstatus: 500,\n\t\t\t},"
		);
	}

	#[test]
	fn locate_edit_indent_fallback_strips_extra_tabs() {
		// Mirror case: model over-indented its `find` relative to
		// the file. We strip the excess from `replace` so the
		// splice still slots in cleanly.
		let file = "error: {\n\tname: x,\n},\n";
		let find = "\t\terror: {\n\t\t\tname: x,\n\t\t},";
		let replace = "\t\terror: {\n\t\t\tname: x,\n\t\t\tstatus: 500,\n\t\t},";
		let plan = locate_edit(file, find, replace, None, "test.rs").expect("fuzzy indent strip match");
		assert_eq!(plan.mode, "fuzzy_indent");
		assert_eq!(&file[plan.start..plan.end], "error: {\n\tname: x,\n},");
		assert_eq!(plan.replace_text, "error: {\n\tname: x,\n\tstatus: 500,\n},");
	}

	#[test]
	fn locate_edit_unescape_fallback_translates_literal_tn() {
		// Escape-leakage failure mode: the model's `find` carries
		// the literal two-char `\n` / `\t` sequences instead of
		// the control bytes. The unescape stage retries with the
		// translated form and quietly succeeds.
		let file = "a\n\tb\n";
		let find = r"a\n\tb";
		let replace = r"x\n\ty";
		let plan = locate_edit(file, find, replace, None, "test.txt").expect("unescape match");
		assert_eq!(plan.mode, "fuzzy_unescape");
		assert_eq!(&file[plan.start..plan.end], "a\n\tb");
		assert_eq!(plan.replace_text, "x\n\ty");
	}

	#[test]
	fn locate_edit_multi_match_error_lists_line_numbers() {
		// Model gave a `find` that legitimately matches twice;
		// without `occurrence` we now name the lines so the model
		// can pick the right one with more context.
		let file = "foo\nbar\nfoo\nbaz\n";
		let err = locate_edit(file, "foo", "X", None, "dup.txt").unwrap_err();
		let msg = err.to_string();
		assert!(msg.contains("lines 1, 3"), "error should list line numbers, got: {msg}");
	}

	#[test]
	fn locate_edit_occurrence_picks_nth_exact_match() {
		// Sanity: the `occurrence` selector still works on the
		// exact path after the refactor.
		let file = "foo\nfoo\nfoo\n";
		let plan = locate_edit(file, "foo", "X", Some(2), "dup.txt").expect("occurrence=2");
		assert_eq!(plan.occurrence, 2);
		assert_eq!(plan.start, 4);
		assert_eq!(plan.end, 7);
	}

	#[test]
	fn locate_edit_no_match_returns_actionable_error() {
		// Final fallback: nothing matched. Error mentions the
		// path and tells the model what to do next (re-read,
		// check whitespace) rather than a bare "not found".
		let err = locate_edit("hello world\n", "missing", "x", None, "src/foo.rs").unwrap_err();
		let msg = err.to_string();
		assert!(msg.contains("src/foo.rs"));
		assert!(msg.contains("indent-tolerant"));
	}

	#[test]
	fn locate_edit_indent_fallback_ambiguous_lists_lines() {
		// Two indent-tolerant hits, no `occurrence` → error
		// names both line numbers so the model can disambiguate
		// without another round of guessing.
		let file = "\tfoo\n\t\tfoo\n";
		let err = locate_edit(file, "foo", "X", None, "dup.txt").unwrap_err();
		let msg = err.to_string();
		// Exact match would find both `foo`s anyway → this is
		// the exact-match ambiguity error, not the indent-fuzzy
		// one. The line numbers should still be listed.
		assert!(msg.contains("lines 1, 2"), "error should list line numbers, got: {msg}");
	}

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

	#[test]
	fn format_numbered_lines_full_file_default() {
		let (out, truncated) = format_numbered_lines("alpha\nbeta\ngamma\n", 1, u32::MAX);
		assert!(!truncated);
		assert_eq!(out, "1|alpha\n2|beta\n3|gamma\n");
	}

	#[test]
	fn format_numbered_lines_slice() {
		let (out, _) = format_numbered_lines("a\nb\nc\nd\ne\n", 2, 4);
		assert_eq!(out, "2|b\n3|c\n4|d\n");
	}

	#[test]
	fn format_numbered_lines_clamps_end_past_eof() {
		let (out, _) = format_numbered_lines("only\n", 1, 99);
		assert_eq!(out, "1|only\n");
	}

	#[test]
	fn format_numbered_lines_pads_width_to_largest_in_range() {
		// Range ends at line 12 → width 2 for every printed line,
		// even the single-digit ones.
		let text: String = (1..=15).map(|i| format!("L{i}\n")).collect();
		let (out, _) = format_numbered_lines(&text, 8, 12);
		let first = out.lines().next().unwrap();
		assert_eq!(first, " 8|L8");
	}

	#[test]
	fn format_numbered_lines_empty_file() {
		let (out, truncated) = format_numbered_lines("", 1, 10);
		assert_eq!(out, "");
		assert!(!truncated);
	}

	#[test]
	fn format_numbered_lines_start_past_eof_is_empty() {
		let (out, _) = format_numbered_lines("a\nb\n", 5, 10);
		assert_eq!(out, "");
	}

	fn hit(path: &str, line: u64, line_text: &str) -> ContentSearchHit {
		ContentSearchHit {
			path: path.to_owned(),
			line,
			column: 1,
			line_text: line_text.to_owned(),
			match_start: 0,
			match_end: 0,
		}
	}

	#[test]
	fn truncate_grep_line_passes_short_lines_through() {
		let line = "fn foo() { todo!() }";
		let out = truncate_grep_line(line);
		assert_eq!(out.as_ref(), line);
	}

	#[test]
	fn truncate_grep_line_caps_at_char_boundary() {
		// Build a line longer than the cap with a multi-byte char near
		// the boundary so a naive byte slice would land mid-codepoint.
		let mut line = String::new();
		for _ in 0..GREP_MAX_LINE_CHARS - 1 {
			line.push('a');
		}
		// Push a 2-byte char at exactly the cap, then more content past it.
		line.push('é');
		for _ in 0..50 {
			line.push('b');
		}
		let out = truncate_grep_line(&line);
		assert!(out.starts_with("aaa"));
		assert!(out.contains("[line truncated"));
		assert!(out.contains(&format!("{} chars total", line.chars().count())));
		assert!(!out.contains('b'));
	}

	#[test]
	fn format_grep_hits_truncates_base64_blob() {
		// Simulate a base64 image embedded as one giant single-line
		// match — the regression case the cap was added for.
		let blob = "A".repeat(50_000);
		let hits = vec![hit("assets/data.json", 1, &blob)];
		let out = format_grep_hits(&hits);
		assert!(out.starts_with("assets/data.json:1: "));
		assert!(out.contains("[line truncated, 50000 chars total]"));
		// The on-the-wire payload should be ~ cap chars + a small marker —
		// nowhere near the original 50k.
		assert!(out.len() < GREP_MAX_LINE_CHARS + 200);
	}

	#[test]
	fn format_grep_hits_does_not_touch_normal_lines() {
		let hits = vec![hit("src/lib.rs", 42, "    let x = compute_thing(input);")];
		let out = format_grep_hits(&hits);
		assert_eq!(out, "src/lib.rs:42:     let x = compute_thing(input);\n");
	}

	mod cross_folder {
		use super::super::{CoderMode, ToolContext, ToolRegistry};
		use crate::error::CoderError;
		use camino::Utf8PathBuf;
		use moon_core::WorkspaceRegistry;
		use std::sync::Arc;
		use tempfile::TempDir;

		async fn build_registry(paths: &[&camino::Utf8Path]) -> (Arc<WorkspaceRegistry>, ToolRegistry) {
			let registry = Arc::new(WorkspaceRegistry::new("test-workspace".into()));
			for p in paths {
				registry.add_folder(p.to_path_buf()).await.unwrap();
			}
			let workspaces_dir = Utf8PathBuf::from(paths[0].parent().unwrap_or(camino::Utf8Path::new("/tmp")));
			let web = crate::web::WebClient::new().expect("web client builds in tests");
			let tool_registry = ToolRegistry::new(registry.clone(), workspaces_dir, web);
			(registry, tool_registry)
		}

		async fn make_cx(registry: &Arc<WorkspaceRegistry>, idx: usize) -> ToolContext {
			let folders = registry.folders().await;
			ToolContext::new(folders[idx].clone(), CoderMode::Agent)
		}

		#[tokio::test]
		async fn relative_path_inside_active_folder_passes_through() {
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let (folder, out) = tools
				.resolve_workspace_path("src/lib.rs", &cx, "read_file")
				.await
				.expect("relative path should pass through");
			assert_eq!(out, "src/lib.rs");
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}

		#[tokio::test]
		async fn synthetic_active_path_strips_to_relative() {
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let active_name = registry.folders().await[0].folder.name.clone();
			let raw = format!("/workspace/{active_name}/src/foo.rs");
			let (folder, out) = tools
				.resolve_workspace_path(&raw, &cx, "read_file")
				.await
				.expect("synthetic active-folder path should resolve");
			assert_eq!(out, "src/foo.rs");
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}

		#[tokio::test]
		async fn synthetic_active_path_with_no_tail_resolves_to_root() {
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let active_name = registry.folders().await[0].folder.name.clone();
			let raw = format!("/workspace/{active_name}");
			let (folder, out) = tools
				.resolve_workspace_path(&raw, &cx, "list_dir")
				.await
				.expect("synthetic root should resolve");
			assert_eq!(out, ".");
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}

		#[tokio::test]
		async fn synthetic_sibling_path_routes_to_other_folder() {
			let one = TempDir::new().unwrap();
			let two = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let two_path = camino::Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path(), two_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let folders = registry.folders().await;
			let other = folders[1].clone();
			let other_name = other.folder.name.clone();
			let raw = format!("/workspace/{other_name}/src/foo.rs");
			let (target, out) = tools
				.resolve_workspace_path(&raw, &cx, "read_file")
				.await
				.expect("synthetic sibling path should now route to the sibling folder");
			assert_eq!(out, "src/foo.rs");
			assert!(Arc::ptr_eq(&target, &other), "expected route to sibling folder");
		}

		#[tokio::test]
		async fn relative_sibling_basename_routes_to_other_folder() {
			let one = TempDir::new().unwrap();
			let two = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let two_path = camino::Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path(), two_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let folders = registry.folders().await;
			let other = folders[1].clone();
			let other_name = other.folder.name.clone();
			// The lower-friction form: agent writes `<sibling-name>/foo.rs`
			// (without the `/workspace/` prefix). Same routing.
			let raw = format!("{other_name}/foo.rs");
			let (target, out) = tools
				.resolve_workspace_path(&raw, &cx, "list_dir")
				.await
				.expect("relative sibling path should route");
			assert_eq!(out, "foo.rs");
			assert!(Arc::ptr_eq(&target, &other));
		}

		#[tokio::test]
		async fn relative_sibling_basename_alone_routes_to_root() {
			let one = TempDir::new().unwrap();
			let two = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let two_path = camino::Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path(), two_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let folders = registry.folders().await;
			let other = folders[1].clone();
			let other_name = other.folder.name.clone();
			let (target, out) = tools
				.resolve_workspace_path(&other_name, &cx, "list_dir")
				.await
				.expect("bare sibling basename should list its root");
			assert_eq!(out, ".");
			assert!(Arc::ptr_eq(&target, &other));
		}

		#[tokio::test]
		async fn explicit_dot_slash_disambiguates_same_named_subdir() {
			// A directory inside the active folder happens to share a
			// sibling's basename. Prefixing with `./` opts out of the
			// cross-folder routing so the path passes through against
			// the active folder.
			let one = TempDir::new().unwrap();
			let two = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let two_path = camino::Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path(), two_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let other_name = registry.folders().await[1].folder.name.clone();
			let raw = format!("./{other_name}/foo.rs");
			let (folder, out) = tools
				.resolve_workspace_path(&raw, &cx, "read_file")
				.await
				.expect("./<sibling-name> should pass through against the active folder");
			assert_eq!(out, raw);
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}

		#[tokio::test]
		async fn synthetic_unbound_name_errors_with_bound_list() {
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let active_name = registry.folders().await[0].folder.name.clone();
			let raw = "/workspace/no-such-folder/src/foo.rs";
			let err = tools.resolve_workspace_path(raw, &cx, "read_file").await.unwrap_err();
			match err {
				CoderError::ToolFailed { tool, message } => {
					assert_eq!(tool, "read_file");
					assert!(message.contains("no-such-folder"), "message: {message}");
					assert!(
						message.contains(&active_name),
						"message should list bound folders: {message}"
					);
				}
				other => panic!("expected ToolFailed, got {other:?}"),
			}
		}

		#[tokio::test]
		async fn unrelated_absolute_path_passes_through_for_host_to_reject() {
			// Absolute paths that don't start with `/workspace` are
			// none of our business — the host's `resolve` will reject
			// them. We don't want to second-guess that.
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let (folder, out) = tools
				.resolve_workspace_path("/etc/passwd", &cx, "read_file")
				.await
				.expect("unrelated absolute paths should pass through");
			assert_eq!(out, "/etc/passwd");
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}
	}
}
