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

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::{WorkspaceFolderEntry, WorkspaceRegistry};
use moon_protocol::container::ContainerState;
use moon_protocol::fs::{DirEntry, EntryKind, ReadFileResult, WriteFileResult};
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
	/// the `mode` field on the `task` tool result, etc.).
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
///
/// Also carries a turn-scoped [`FormatQueue`] that `write_file` /
/// `edit_file` push into instead of running the format-on-save
/// pipeline inline. The runner drains the queue at turn end (see
/// `run_turn` / `run_subagent`). One queue per turn, shared across
/// every tool dispatch in that turn — parent and sub-agent each
/// own their own.
#[derive(Clone)]
pub struct ToolContext {
	pub folder: Arc<WorkspaceFolderEntry>,
	pub mode: CoderMode,
	pub format_queue: Arc<FormatQueue>,
	/// When true, the `bash` tool routes to the host machine even
	/// if the workspace shell container is running — the per-session
	/// [`BashTargetOverride::ForceHost`](crate::sessions::BashTargetOverride)
	/// escape hatch. Captured from the session at turn spawn; the
	/// rest of the turn reads it from here so a settings flip
	/// mid-turn doesn't relocate in-flight commands.
	pub force_host_bash: bool,
}

impl ToolContext {
	pub fn new(folder: Arc<WorkspaceFolderEntry>, mode: CoderMode) -> Self {
		Self {
			folder,
			mode,
			format_queue: Arc::new(FormatQueue::default()),
			force_host_bash: false,
		}
	}

	/// Like [`new`](Self::new) but reuses an existing queue. Used
	/// when one logical turn fans out into multiple `ToolContext`s
	/// (e.g. the parent's homogeneous-`task` parallel-dispatch
	/// path) and we still want a single flush at the end.
	pub fn with_format_queue(folder: Arc<WorkspaceFolderEntry>, mode: CoderMode, queue: Arc<FormatQueue>) -> Self {
		Self {
			folder,
			mode,
			format_queue: queue,
			force_host_bash: false,
		}
	}

	/// Builder-style setter for the per-session force-host-bash
	/// flag. Kept separate from the constructors so the sub-agent /
	/// parallel-dispatch call sites that always run auto don't have
	/// to thread it.
	pub fn with_force_host_bash(mut self, force_host: bool) -> Self {
		self.force_host_bash = force_host;
		self
	}
}

/// Where a path argument resolved to. Produced by
/// [`ToolRegistry::resolve_target`] and consumed by the four
/// filesystem tools (`read_file` / `list_dir` / `write_file` /
/// `edit_file`) so they can branch between the bound-folder host
/// (which keeps format-on-save and the future in-container
/// `RemoteHost`) and the container-aware out-of-workspace
/// primitives.
enum ResolvedTarget {
	/// Path lands inside a bound folder. `relative` is the
	/// portion inside the folder root (`.` for the root itself).
	InWorkspace {
		folder: Arc<WorkspaceFolderEntry>,
		relative: String,
	},
	/// Absolute path outside every bound folder. Routed through
	/// `docker exec` when the workspace shell container is running,
	/// the host filesystem otherwise.
	OutOfWorkspace { abs_path: Utf8PathBuf },
}

impl ResolvedTarget {
	fn in_workspace(folder: Arc<WorkspaceFolderEntry>, relative: String) -> Self {
		Self::InWorkspace { folder, relative }
	}

	fn out_of_workspace(abs_path: Utf8PathBuf) -> Self {
		Self::OutOfWorkspace { abs_path }
	}
}

/// Per-turn set of files the coder's write/edit tools touched.
///
/// `write_file` / `edit_file` write raw bytes through
/// [`WorkspaceHost::write_file`] and register the touched path
/// here; at turn end the runner drains the queue and runs
/// [`WorkspaceHost::format_file`] against each entry exactly once,
/// regardless of how many times the model edited that file. See
/// the ADR superseding 0013's per-tool-call invocation for why.
#[derive(Default)]
pub struct FormatQueue {
	entries: Mutex<HashSet<FormatQueueEntry>>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct FormatQueueEntry {
	folder_path: String,
	relative_path: Utf8PathBuf,
}

impl FormatQueue {
	/// Record that `(folder, relative_path)` was written this turn.
	/// Idempotent; the same path coming through N times still
	/// flushes once.
	pub fn record(&self, folder: &WorkspaceFolderEntry, relative_path: &Utf8Path) {
		let entry = FormatQueueEntry {
			folder_path: folder.folder.path.clone(),
			relative_path: relative_path.to_path_buf(),
		};
		// `Mutex::lock` only fails on poisoning; treat that as a
		// programmer error (some earlier panic inside a tool
		// holding the guard) and ignore — the queue is a hint,
		// not authoritative state.
		let Ok(mut guard) = self.entries.lock() else {
			return;
		};
		guard.insert(entry);
	}

	/// Drain the queue and return the recorded entries grouped for
	/// the caller to dispatch against. The set is cleared so a
	/// subsequent flush call is a no-op.
	pub fn drain(&self) -> Vec<(String, Utf8PathBuf)> {
		let Ok(mut guard) = self.entries.lock() else {
			return Vec::new();
		};
		guard.drain().map(|e| (e.folder_path, e.relative_path)).collect()
	}

	/// `true` when no tool has registered a path yet. Used by the
	/// runner to skip the flush log entirely on read-only turns.
	pub fn is_empty(&self) -> bool {
		self.entries.lock().map(|g| g.is_empty()).unwrap_or(true)
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

	/// `true` when the workspace shell container is currently
	/// `Running` and the `bash` tool would route there. Reused by
	/// the system-prompt builder to decide whether to advertise
	/// folders by their host absolute paths (host mode) or under
	/// the synthetic `/workspace/<name>` mount the container
	/// actually exposes (container mode). Falls back to host on
	/// any docker-side failure, matching the bash tool's posture.
	///
	/// `force_host` is the per-session override: when set, this
	/// always reports `false` (host mode) so the system prompt
	/// advertises host paths consistently with where `bash` will
	/// actually run.
	pub async fn bash_target_is_container(&self, force_host: bool) -> bool {
		resolve_bash_target(&self.workspaces, &self.workspaces_dir, force_host).await == BASH_TARGET_CONTAINER
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

	/// Resolve a path argument and route to the right bound folder.
	/// Returns the `(target_folder, relative_path)` pair the caller
	/// should dispatch against.
	///
	/// Cases, in order:
	///
	/// - **Absolute path under a bound folder's root**: routes to
	///   that folder. The returned `relative_path` is the portion
	///   inside the folder's root (or `"."` when the path is the
	///   folder root itself). This is the everyday host-mode form:
	///   the system prompt's "Bound folders" section advertises
	///   each folder by its absolute host path, and the model
	///   joins file-relative paths onto it.
	/// - **Synthetic `/workspace/<name>/...`**: container-mode
	///   form. Routes to the folder whose basename matches
	///   `<name>`. Errors with a clear "no folder bound as `<name>`"
	///   message when the basename doesn't match anything bound.
	///   Kept available in host mode too — a model that's seen
	///   both forms across sessions doesn't fight us about it —
	///   but the system prompt only advertises this form when the
	///   workspace shell container is actually running.
	/// - **Bare relative path starting with another bound folder's
	///   basename** (`<other>/foo.rs`): also routes cross-folder.
	///   Disambiguation: a leading `./` opts out and forces the
	///   path to resolve inside the [`ToolContext`]'s folder, so a
	///   legitimate same-named subdirectory still works.
	/// - **Anything else** (relative paths, absolute paths outside
	///   every bound folder's root): resolved against `cx.folder`
	///   and left for [`WorkspaceHost::resolve`] to validate the
	///   way it always has — an absolute path outside every bound
	///   folder fails with a clear "escapes workspace root" error,
	///   which is the behaviour we want for paths like `/etc/passwd`.
	///
	/// Sub-agents call this with `cx.folder` set to their own
	/// assigned folder. They typically only see one bound folder
	/// in their tool context (the one they were spawned against),
	/// but the routing logic is identical — it just collapses to
	/// the no-op case when the path is already inside `cx.folder`.
	///
	/// Returns a [`ResolvedTarget`]:
	///
	/// - [`ResolvedTarget::InWorkspace`] when the path lands inside
	///   a bound folder — the everyday case. The tool dispatches
	///   through that folder's [`WorkspaceHost`], which keeps the
	///   editorconfig / lint-staged / format-on-save pipeline and
	///   (in the container world) the in-container `RemoteHost`.
	/// - [`ResolvedTarget::OutOfWorkspace`] when the path is
	///   absolute and outside every bound folder root (and not a
	///   `/workspace/<name>` synthetic path). The tool routes
	///   through the container-aware out-of-workspace primitives
	///   ([`oow_read_file`] / [`oow_write_file`] / [`oow_read_dir`]),
	///   which `docker exec` into the workspace shell container when
	///   it's running and fall back to the host filesystem
	///   otherwise — same target `bash` would pick. The host-root
	///   gate is *not* applied here: arbitrary-path access is the
	///   point. See ADR 0025.
	async fn resolve_target(
		&self,
		raw: &str,
		cx: &ToolContext,
		tool: &'static str,
	) -> Result<ResolvedTarget, CoderError> {
		let folders = self.workspaces.folders().await;
		let active_name = cx.folder.folder.name.as_str();
		let path = Utf8Path::new(raw);

		if path.is_absolute() {
			// Absolute path that lands under a bound folder's
			// root: route to that folder. The "longest matching
			// root" pick handles the (rare) case where one bound
			// folder is a strict ancestor of another — the inner
			// one wins, matching how the file tree groups them.
			if let Some((target, relative)) = match_bound_folder_root(&folders, path) {
				return Ok(ResolvedTarget::in_workspace(target, relative));
			}
			// Synthetic `/workspace/<name>/...`: the container-mode
			// surface from the system prompt. Looked up by basename
			// against the bound folders just like the absolute-root
			// case above.
			//
			// A `/workspace/<name>/...` path whose `<name>` is a
			// bound folder routes to that folder's host. A
			// `/workspace/<name>` whose `<name>` is *not* bound
			// errors with the list of what is bound — the system
			// prompt only ever advertises this form for bound
			// folders, so an unbound `<name>` is a model mistake
			// worth a precise nudge rather than a silent fall to
			// the out-of-workspace path (where it'd surface as a
			// generic "no such file" against a container mount
			// point that doesn't exist).
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
					return Ok(ResolvedTarget::in_workspace(target, resolved));
				}
			}
			// Absolute path outside every bound folder root (and
			// not a `/workspace/<name>` synthetic): route to the
			// container-aware out-of-workspace primitives. No
			// host-root gate — the agent can reach any path the
			// bash target can. See ADR 0025.
			return Ok(ResolvedTarget::out_of_workspace(path.to_path_buf()));
		}

		let mut comps = path.components();
		match comps.next() {
			// Leading `./` is the explicit "I mean a path *inside*
			// `cx.folder`, even if the first segment looks like a
			// sibling's basename" opt-out. Pass through untouched.
			Some(camino::Utf8Component::CurDir) => Ok(ResolvedTarget::in_workspace(cx.folder.clone(), raw.to_string())),
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
						return Ok(ResolvedTarget::in_workspace(other, resolved));
					}
				}
				Ok(ResolvedTarget::in_workspace(cx.folder.clone(), raw.to_string()))
			}
			// Relative paths that aren't a sibling basename (`..`,
			// etc.) resolve against the active folder's host, which
			// keeps the historical `..`-escape rejection for plain
			// relative inputs. Only *absolute* paths reach the
			// out-of-workspace branch above.
			_ => Ok(ResolvedTarget::in_workspace(cx.folder.clone(), raw.to_string())),
		}
	}

	/// Back-compat thin wrapper that flattens [`resolve_target`] to
	/// the `(folder, relative)` pair for the in-workspace case and
	/// errors on out-of-workspace inputs. The four fs tools call
	/// [`resolve_target`] directly so they can branch; this stays
	/// for any caller that only ever wants the in-workspace shape.
	#[cfg(test)]
	async fn resolve_workspace_path(
		&self,
		raw: &str,
		cx: &ToolContext,
		tool: &'static str,
	) -> Result<(Arc<WorkspaceFolderEntry>, String), CoderError> {
		match self.resolve_target(raw, cx, tool).await? {
			ResolvedTarget::InWorkspace { folder, relative } => Ok((folder, relative)),
			ResolvedTarget::OutOfWorkspace { abs_path } => Err(CoderError::tool_failed(
				tool,
				format!("{abs_path} is outside every bound folder"),
			)),
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
				"Read the contents of a file. Returns the file's text, with each line prefixed by `<line_number>|<line>`. Treat the prefix as metadata — it is not part of the file. Optional `start_line` / `end_line` (1-based, inclusive) read just a slice; both omitted means read the whole file (capped at 200 kB).",
				json!({
					"type": "object",
					"properties": {
					"path": {
						"type": "string",
						"description": "A relative path against the active folder (`src/foo.rs`), or any absolute path."
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
				"List the immediate contents of a directory. Returns one entry per line in `kind  name` form.",
				json!({
					"type": "object",
					"properties": {
					"path": {
						"type": "string",
						"description": "`.` for the active folder root, a relative path against it (`src/`), or any absolute path.",
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
							"description": "Shell command. Host: `bash -lc <cmd>`. Workspace shell container: `docker exec … bash -c <cmd>` so moon-base `ENV PATH` (fnm, cargo, bun, …) is preserved."
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
				"Overwrite a file with new content (or create it if missing). Use for new files or whole-file rewrites; prefer `edit_file` for surgical changes inside a large file. Missing parent directories are created automatically (`mkdir -p`).",
				json!({
					"type": "object",
					"properties": {
					"path": {
						"type": "string",
						"description": "A relative path against the active folder (`src/foo.rs`), or any absolute path. Created if it does not exist."
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
				"Replace a substring in a file. `find` is whitespace-tolerant (indent shifts and interior whitespace runs forgiven) and must be unique unless `occurrence` is given. Empty `replace` deletes. Bytes the file holds between your edits in a turn are exactly what `write_file` / `edit_file` wrote — for files in a bound folder the format-on-save chain runs once per touched file at the end of the turn.",
				json!({
					"type": "object",
					"properties": {
					"path": {
						"type": "string",
						"description": "A relative path against the active folder, or any absolute path. The file must already exist."
						},
						"find": {
							"type": "string",
							"description": "Substring to locate. No regex; whitespace-tolerant."
						},
						"replace": {
							"type": "string",
							"description": "Replacement text. Empty string deletes the matched span."
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
			ToolDefinition::function(
				"todo_write",
				"Maintain a small in-context todo list for the current task. Returns the canonical full list after the call. `merge: false` (default) replaces the list wholesale — pass `todos: []` to clear. `merge: true` matches incoming items by `id` and updates in place; unknown ids are appended; items you don't mention are left untouched. Each item must carry `id` and `content`; `status` defaults to `pending` if omitted.",
				json!({
					"type": "object",
					"properties": {
						"todos": {
							"type": "array",
							"description": "`id` and `content` are required on every entry; `status` defaults to `pending`.",
							"items": {
								"type": "object",
								"properties": {
									"id": {
										"type": "string",
										"description": "Stable identifier you assign and reuse across calls. Match an existing item with `merge: true` to update it."
									},
									"content": {
										"type": "string",
										"description": "Short imperative description (\"Add foo\", \"Wire up bar\")."
									},
									"status": {
										"type": "string",
										"enum": ["pending", "in_progress", "completed", "cancelled"],
										"description": "Lifecycle state. Mark exactly one item `in_progress` while working on it; flip to `completed` or `cancelled` when done. Defaults to `pending` if omitted.",
										"default": "pending"
									}
								},
								"required": ["id", "content"]
							}
						},
						"merge": {
							"type": "boolean",
							"description": "When true, items are matched by `id` and merged into the current list (unknown ids appended; unmentioned ids left alone). When false, the incoming list replaces the current one wholesale. Default false.",
							"default": false
						}
					},
					"required": ["todos"]
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
			#[serde(alias = "file_path", alias = "file")]
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
		let result = match self.resolve_target(&parsed.path, cx, "read_file").await? {
			ResolvedTarget::InWorkspace { folder, relative } => folder.host.read_file(Utf8Path::new(&relative)).await?,
			ResolvedTarget::OutOfWorkspace { abs_path } => self.oow_read_file(&abs_path, cx).await?,
		};
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
		let entries = match self.resolve_target(&parsed.path, cx, "list_dir").await? {
			ResolvedTarget::InWorkspace { folder, relative } => folder.host.read_dir(Utf8Path::new(&relative)).await?,
			ResolvedTarget::OutOfWorkspace { abs_path } => self.oow_read_dir(&abs_path, cx).await?,
		};
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
			..Default::default()
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
		let result = match self.resolve_target(&parsed.path, cx, "write_file").await? {
			ResolvedTarget::InWorkspace { folder, relative } => {
				let resolved = Utf8Path::new(&relative);
				// Raw bytes-to-disk: the format-on-save pipeline
				// (editorconfig + lint-staged) runs at turn end via
				// `FormatQueue::drain` against the union of every
				// path touched this turn. Doing the format inline
				// here would mean (a) re-spawning prettier / eslint
				// / rustfmt once per edit even when the model is
				// about to edit the same file again two tool calls
				// later, and (b) `eslint --fix` stripping an
				// "unused" import in between the model adding the
				// import and adding its first use. See the ADR
				// superseding 0013's per-call invocation.
				let result = folder.host.write_file(resolved, &parsed.content).await?;
				cx.format_queue.record(&folder, resolved);
				result
			}
			// Out-of-workspace writes bypass format-on-save
			// entirely: there's no bound-folder host to anchor the
			// editorconfig / lint-staged cascade, and an external
			// file (`/etc/hosts`, `~/.config/...`) has no project
			// config to apply. Raw bytes, no FormatQueue entry.
			ResolvedTarget::OutOfWorkspace { abs_path } => self.oow_write_file(&abs_path, &parsed.content, cx).await?,
		};
		Ok(json!({
			"path": parsed.path,
			"bytes_written": parsed.content.len(),
			"mtime_ms": result.mtime_ms,
		}))
	}

	async fn edit_file(&self, args: &Value, cx: &ToolContext) -> Result<Value, CoderError> {
		#[derive(Deserialize)]
		struct EditFileArgs {
			#[serde(alias = "file_path", alias = "file")]
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
		let target = self.resolve_target(&parsed.path, cx, "edit_file").await?;
		let original = match &target {
			ResolvedTarget::InWorkspace { folder, relative } => folder.host.read_file(Utf8Path::new(relative)).await?,
			ResolvedTarget::OutOfWorkspace { abs_path } => self.oow_read_file(abs_path, cx).await?,
		};
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

		let result = match target {
			ResolvedTarget::InWorkspace { folder, relative } => {
				let path = Utf8Path::new(&relative);
				// Raw bytes-to-disk; format-on-save runs at turn
				// end. See the `write_file` tool above for the
				// rationale.
				let result = folder.host.write_file(path, &new_text).await?;
				cx.format_queue.record(&folder, path);
				result
			}
			// Out-of-workspace: raw bytes, no format-on-save. Same
			// reasoning as `write_file`'s out-of-workspace arm.
			ResolvedTarget::OutOfWorkspace { abs_path } => self.oow_write_file(&abs_path, &new_text, cx).await?,
		};
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

		let (mut command, target_kind) = self.build_bash_command(folder, &parsed.cmd, cx.force_host_bash).await?;
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
	///   `docker exec -w <container_cwd> <name> bash -c <cmd>`.
	///   Reuses `moon_terminal::container_name_for_workspace` +
	///   `TerminalTarget::container_cwd_for_folder` so the framing
	///   matches terminals and LSP exactly.
	///
	///   **`bash -c` here (not `-lc`).** moon-base sets toolchain
	///   PATH segments via Dockerfile `ENV`. A login shell reads
	///   Debian `/etc/profile`, which resets `PATH` before project
	///   hooks run; non-interactive shells skip `~/.bashrc`, so fnm's
	///   eval never restores Node — yet `docker exec … bash` (PTY
	///   terminals) is interactive and does load `~/.bashrc`. Using a
	///   non-login shell inherits the container env verbatim and
	///   matches what `node`, `cargo`, etc. expect from the image.
	/// - **Host** (otherwise): `bash -lc <cmd>` rooted at the folder.
	///
	/// **Why host uses `bash -lc` and not `sh -lc`.** On most modern
	/// Linuxes `/bin/sh` is `dash`, which as a login shell reads only
	/// `~/.profile`. Most host toolchains (rustup, mise, pyenv, …)
	/// extend PATH from `~/.bashrc` — sometimes additionally in
	/// `~/.profile`, often not. Result: `sh -lc 'cargo …'` returns
	/// "cargo: not found" even though the user's interactive terminal
	/// has cargo on PATH. `bash -lc` reads `~/.bash_profile` (which on
	/// almost every dev box sources `~/.bashrc`). Trade-off: requires
	/// `bash` in the container — true for moon-base, since terminals
	/// (`moon-terminal::target`) already assume it.
	async fn build_bash_command(
		&self,
		folder: &WorkspaceFolderEntry,
		cmd: &str,
		force_host: bool,
	) -> Result<(tokio::process::Command, &'static str), CoderError> {
		let target = resolve_bash_target(&self.workspaces, &self.workspaces_dir, force_host).await;
		if target == BASH_TARGET_CONTAINER {
			let workspace_id = self.workspaces.workspace_id().await;
			let container_name = container_name_for_workspace(&workspace_id);
			// Worktree-backed sessions (ADR 0029) live inside the parent
			// repo at `<parent>/.worktrees/…`, so their `bash` cwd is the
			// parent's `/workspace/<name>` mount plus the relative tail.
			// Everything else falls back to `/workspace` if the host
			// path has no basename — same fallback `moon-terminal` uses
			// for pathological inputs (`/`).
			let folder_path = Utf8Path::new(&folder.folder.path);
			let container_cwd =
				if let moon_protocol::workspace::FolderOrigin::Worktree { parent_path, .. } = &folder.folder.origin {
					moon_core::worktree::worktree_container_path(Utf8Path::new(parent_path), folder_path)
						.unwrap_or_else(|| Utf8PathBuf::from("/workspace"))
				} else {
					TerminalTarget::container_cwd_for_folder(folder_path).unwrap_or_else(|| Utf8PathBuf::from("/workspace"))
				};
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
				.arg("-c")
				.arg(cmd);
			return Ok((command, BASH_TARGET_CONTAINER));
		}
		let mut command = tokio::process::Command::new("bash");
		command.arg("-lc").arg(cmd).current_dir(folder.folder.path.as_str());
		Ok((command, BASH_TARGET_HOST))
	}

	/// `true` when out-of-workspace fs ops should route through the
	/// workspace shell container. Same probe as `bash` so the file
	/// tools and `bash` agree on where an external path lives — an
	/// agent that `ls`'d `/etc` via `bash` and then `read_file`'d
	/// `/etc/hosts` reaches the same filesystem both times.
	async fn oow_target_is_container(&self, cx: &ToolContext) -> bool {
		resolve_bash_target(&self.workspaces, &self.workspaces_dir, cx.force_host_bash).await == BASH_TARGET_CONTAINER
	}

	/// `docker exec` (no TTY) against the workspace shell container,
	/// invoking `program` with `args` directly — no shell, so the
	/// path arguments need no quoting and can't be re-interpreted.
	async fn container_exec(&self, program: &str, args: &[&str]) -> tokio::process::Command {
		let workspace_id = self.workspaces.workspace_id().await;
		let container_name = container_name_for_workspace(&workspace_id);
		let mut command = tokio::process::Command::new("docker");
		command.arg("exec").arg(&container_name).arg(program);
		for arg in args {
			command.arg(arg);
		}
		command
	}

	/// Read an arbitrary absolute path, routed to the container when
	/// it's running and the host otherwise. Mirrors the binary /
	/// mtime shape of [`moon_core::read_host_file`] so the
	/// `read_file` tool handles both arms identically.
	///
	/// In-container reads `cat` the file and stat its mtime in one
	/// `docker exec` round-trip via a tiny `sh` wrapper; the host
	/// arm calls [`moon_core::read_host_file`] directly.
	async fn oow_read_file(&self, abs_path: &Utf8Path, cx: &ToolContext) -> Result<ReadFileResult, CoderError> {
		if !self.oow_target_is_container(cx).await {
			return Ok(moon_core::read_host_file(abs_path).await?);
		}
		// `cat -- <path>`: direct exec, no shell, so the path can't
		// be word-split or glob-expanded. mtime comes from a
		// separate `stat` exec — cheaper to reason about than
		// multiplexing both onto one stdout, and the file tools
		// only use mtime as an advisory freshness hint.
		let mut cat = self.container_exec("cat", &["--", abs_path.as_str()]).await;
		let output = run_capturing(&mut cat, "read_file").await?;
		if !output.status.success() {
			return Err(CoderError::tool_failed(
				"read_file",
				container_io_error(abs_path, &output.stderr),
			));
		}
		let mtime_ms = self.container_stat_mtime_ms(abs_path).await;
		if looks_binary_bytes(&output.stdout) {
			return Ok(ReadFileResult {
				text: String::new(),
				mtime_ms,
				is_binary: true,
			});
		}
		let text = String::from_utf8(output.stdout)
			.map_err(|e| CoderError::tool_failed("read_file", format!("{abs_path}: invalid UTF-8: {e}")))?;
		Ok(ReadFileResult {
			text,
			mtime_ms,
			is_binary: false,
		})
	}

	/// Write raw bytes to an arbitrary absolute path, routed to the
	/// container when it's running and the host otherwise. No
	/// format-on-save either way — external files have no project
	/// config (see the `write_file` tool's out-of-workspace arm).
	async fn oow_write_file(
		&self,
		abs_path: &Utf8Path,
		content: &str,
		cx: &ToolContext,
	) -> Result<WriteFileResult, CoderError> {
		if !self.oow_target_is_container(cx).await {
			return Ok(moon_core::write_host_file(abs_path, content).await?);
		}
		// `cp /dev/stdin <path>` with the content piped on stdin:
		// direct exec (no shell redirection to quote), and `cp`
		// from `/dev/stdin` lands the bytes verbatim. `-i` keeps
		// stdin open for the pipe.
		let mut cmd = self
			.container_exec_stdin("cp", &["/dev/stdin", abs_path.as_str()])
			.await;
		cmd
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped());
		let mut child = cmd
			.spawn()
			.map_err(|err| CoderError::tool_failed("write_file", format!("spawn failed: {err}")))?;
		if let Some(mut stdin) = child.stdin.take() {
			use tokio::io::AsyncWriteExt as _;
			stdin
				.write_all(content.as_bytes())
				.await
				.map_err(|err| CoderError::tool_failed("write_file", format!("write to container stdin: {err}")))?;
			stdin
				.shutdown()
				.await
				.map_err(|err| CoderError::tool_failed("write_file", format!("close container stdin: {err}")))?;
		}
		let output = child
			.wait_with_output()
			.await
			.map_err(|err| CoderError::tool_failed("write_file", err.to_string()))?;
		if !output.status.success() {
			return Err(CoderError::tool_failed(
				"write_file",
				container_io_error(abs_path, &output.stderr),
			));
		}
		let mtime_ms = self.container_stat_mtime_ms(abs_path).await;
		Ok(WriteFileResult {
			mtime_ms,
			bytes_written: content.len() as u64,
		})
	}

	/// List an arbitrary absolute directory, routed to the
	/// container when it's running and the host otherwise. Returns
	/// the same [`DirEntry`] shape `read_dir` produces (kind + name;
	/// `path` carries the absolute child path so the model can feed
	/// it straight back into another tool call).
	async fn oow_read_dir(&self, abs_path: &Utf8Path, cx: &ToolContext) -> Result<Vec<DirEntry>, CoderError> {
		if !self.oow_target_is_container(cx).await {
			return host_read_dir_abs(abs_path).await;
		}
		// One `find` exec lists the directory's direct children with
		// a type tag per line (`d`/`f`/`l`/`?`) so we don't need a
		// `stat` per entry. `-maxdepth 1 -mindepth 1` is direct
		// children only; `printf` keeps the framing fixed.
		let arg = format!(
			"find {} -maxdepth 1 -mindepth 1 -printf '%y\\t%f\\n' 2>&1",
			shell_single_quote(abs_path.as_str())
		);
		let mut cmd = self.container_exec("sh", &["-c", &arg]).await;
		let output = run_capturing(&mut cmd, "list_dir").await?;
		if !output.status.success() {
			return Err(CoderError::tool_failed(
				"list_dir",
				container_io_error(abs_path, &output.stdout),
			));
		}
		let text = String::from_utf8_lossy(&output.stdout);
		let mut entries = Vec::new();
		for line in text.lines() {
			let Some((tag, name)) = line.split_once('\t') else {
				continue;
			};
			if name.is_empty() || name == ".git" {
				continue;
			}
			let kind = match tag {
				"d" => EntryKind::Dir,
				"f" => EntryKind::File,
				"l" => EntryKind::Symlink,
				_ => EntryKind::Other,
			};
			entries.push(DirEntry {
				is_hidden: name.starts_with('.'),
				name: name.to_string(),
				path: abs_path.join(name).to_string(),
				kind,
				size: None,
				mtime_ms: None,
			});
		}
		entries.sort_by(|a, b| match (a.kind, b.kind) {
			(EntryKind::Dir, EntryKind::Dir) => a.name.cmp(&b.name),
			(EntryKind::Dir, _) => std::cmp::Ordering::Less,
			(_, EntryKind::Dir) => std::cmp::Ordering::Greater,
			_ => a.name.cmp(&b.name),
		});
		Ok(entries)
	}

	/// Best-effort container-side mtime (epoch ms). Used only as the
	/// advisory `mtime_ms` on read/write results, so any failure
	/// collapses to `None` rather than failing the tool.
	async fn container_stat_mtime_ms(&self, abs_path: &Utf8Path) -> Option<i64> {
		// `stat -c %Y` is seconds since epoch on GNU coreutils
		// (moon-base is Debian). Multiply to ms for parity with the
		// host path's `system_time_to_ms`.
		let mut cmd = self
			.container_exec("stat", &["-c", "%Y", "--", abs_path.as_str()])
			.await;
		let output = run_capturing(&mut cmd, "stat").await.ok()?;
		if !output.status.success() {
			return None;
		}
		let secs: i64 = String::from_utf8_lossy(&output.stdout).trim().parse().ok()?;
		Some(secs * 1000)
	}

	/// Like [`container_exec`](Self::container_exec) but adds `-i` so
	/// stdin stays open for the caller to pipe bytes through.
	async fn container_exec_stdin(&self, program: &str, args: &[&str]) -> tokio::process::Command {
		let workspace_id = self.workspaces.workspace_id().await;
		let container_name = container_name_for_workspace(&workspace_id);
		let mut command = tokio::process::Command::new("docker");
		command.arg("exec").arg("-i").arg(&container_name).arg(program);
		for arg in args {
			command.arg(arg);
		}
		command
	}
}

/// Run `command` to completion capturing stdout/stderr, mapping a
/// spawn / wait failure onto the named tool's `ToolFailed`. Caller
/// inspects `output.status` for command-level failures.
async fn run_capturing(
	command: &mut tokio::process::Command,
	tool: &'static str,
) -> Result<std::process::Output, CoderError> {
	command
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped());
	command
		.output()
		.await
		.map_err(|err| CoderError::tool_failed(tool, format!("docker exec failed: {err}")))
}

/// Read an absolute host directory (out-of-workspace, no root
/// gate). Mirrors `LocalHost::read_dir`'s entry shape but without
/// the relative-path rewrite — `path` carries the absolute child
/// path so the model can feed it straight back into another tool.
async fn host_read_dir_abs(abs_path: &Utf8Path) -> Result<Vec<DirEntry>, CoderError> {
	let mut read = tokio::fs::read_dir(abs_path.as_std_path())
		.await
		.map_err(|err| CoderError::tool_failed("list_dir", format!("{abs_path}: {err}")))?;
	let mut entries = Vec::new();
	while let Some(entry) = read
		.next_entry()
		.await
		.map_err(|err| CoderError::tool_failed("list_dir", err.to_string()))?
	{
		let file_type = entry
			.file_type()
			.await
			.map_err(|err| CoderError::tool_failed("list_dir", err.to_string()))?;
		let kind = if file_type.is_dir() {
			EntryKind::Dir
		} else if file_type.is_symlink() {
			EntryKind::Symlink
		} else if file_type.is_file() {
			EntryKind::File
		} else {
			EntryKind::Other
		};
		let name = entry.file_name().to_string_lossy().to_string();
		if name == ".git" {
			continue;
		}
		entries.push(DirEntry {
			is_hidden: name.starts_with('.'),
			path: abs_path.join(&name).to_string(),
			name,
			kind,
			size: None,
			mtime_ms: None,
		});
	}
	entries.sort_by(|a, b| match (a.kind, b.kind) {
		(EntryKind::Dir, EntryKind::Dir) => a.name.cmp(&b.name),
		(EntryKind::Dir, _) => std::cmp::Ordering::Less,
		(_, EntryKind::Dir) => std::cmp::Ordering::Greater,
		_ => a.name.cmp(&b.name),
	});
	Ok(entries)
}

/// Same null-byte heuristic as `moon_core`'s `looks_binary`: a NUL
/// in the first 8 kB means binary. Duplicated here because the
/// container read path holds raw `Vec<u8>` from `docker exec`
/// stdout rather than going through `read_host_file`.
fn looks_binary_bytes(bytes: &[u8]) -> bool {
	bytes[..bytes.len().min(8000)].contains(&0)
}

/// Build a useful error string from a failed container fs op. We
/// surface the captured stderr (trimmed) when there is one, else a
/// generic "no such file or permission denied" hint keyed on the
/// path.
fn container_io_error(abs_path: &Utf8Path, stderr: &[u8]) -> String {
	let msg = String::from_utf8_lossy(stderr);
	let trimmed = msg.trim();
	if trimmed.is_empty() {
		format!("{abs_path}: no such file or directory, or permission denied (in container)")
	} else {
		format!("{abs_path}: {trimmed}")
	}
}

/// Single-quote a string for safe interpolation into an `sh -c`
/// command. Wraps in `'...'` and escapes embedded single quotes as
/// `'\''`. Used for the one place an out-of-workspace fs op needs a
/// shell (`find` for `list_dir`); the read/write paths pass the
/// path as a direct exec arg and don't need this.
fn shell_single_quote(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('\'');
	for ch in s.chars() {
		if ch == '\'' {
			out.push_str("'\\''");
		} else {
			out.push(ch);
		}
	}
	out.push('\'');
	out
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
///
/// `force_host` is the per-session escape hatch
/// ([`crate::sessions::BashTargetOverride::ForceHost`]): when set,
/// short-circuit to host without probing the container at all, so
/// an agent can inspect host-side Docker / networking even while
/// the workspace runs in a container. The default (`false`) keeps
/// the historical auto behaviour.
pub(crate) async fn resolve_bash_target(
	workspaces: &WorkspaceRegistry,
	workspaces_dir: &Utf8Path,
	force_host: bool,
) -> &'static str {
	if force_host {
		return BASH_TARGET_HOST;
	}
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
/// Pick the bound folder whose root is an ancestor of `path` (or
/// equal to it) and return `(folder, relative_path_inside)`. When
/// multiple roots match — only possible if a bound folder is a
/// strict ancestor of another, which the file tree allows — the
/// **longest** match wins so the inner folder's relative addressing
/// stays correct. Returns `None` for absolute paths that aren't
/// under any bound root.
///
/// `path` must be absolute; callers branch on `is_absolute()` first.
/// We compare on the component-prefix (not raw-string-prefix) so a
/// folder root `/foo/bar` doesn't match an unrelated path
/// `/foo/barbaz/...`.
fn match_bound_folder_root(
	folders: &[Arc<WorkspaceFolderEntry>],
	path: &Utf8Path,
) -> Option<(Arc<WorkspaceFolderEntry>, String)> {
	let mut best: Option<(Arc<WorkspaceFolderEntry>, String, usize)> = None;
	for entry in folders {
		let root = Utf8Path::new(entry.folder.path.as_str());
		let Ok(tail) = path.strip_prefix(root) else {
			continue;
		};
		let depth = root.as_str().len();
		let relative = if tail.as_str().is_empty() {
			".".to_string()
		} else {
			tail.as_str().to_string()
		};
		match &best {
			Some((_, _, prev_depth)) if *prev_depth >= depth => {}
			_ => best = Some((entry.clone(), relative, depth)),
		}
	}
	best.map(|(f, r, _)| (f, r))
}

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
			"`{raw_path}` references a workspace `{requested}`, but no folder is bound under that name \
({bound_clause}). Use the absolute path the system prompt's \"Bound folders\" section advertises for the \
target folder, or a plain relative path to address the active folder."
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
	/// protocol: `"exact"`, `"fuzzy_unescape"`, `"fuzzy_backslash"`,
	/// `"fuzzy_indent"`, `"fuzzy_whitespace"`.
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
///    mode) and the unescaped form matches exactly while the
///    original doesn't, treat that as the intended pattern.
///    `replace` is unescaped in the same way so the splice is
///    consistent.
/// 3. **Backslash-run-collapsing fallback** — the model can't keep
///    backslash counts straight across the JSON ⇄ source-string
///    boundary, especially in regex literals and template-literal
///    `RegExp(...)` patterns. Collapse every run of consecutive
///    backslashes to a single backslash on both `find` and the
///    file, then re-match. On a unique hit, splice the file's
///    *original* byte range (so the file's escaping survives) and
///    drop `replace` in verbatim — the model owns its own
///    replacement bytes here, same as the whitespace stage below.
/// 4. **Indent-tolerant fallback** — strip per-line leading whitespace
///    from both `find` and the file's lines, look for a line-aligned
///    match. On success, splice the *original* file lines' byte range
///    and re-indent `replace` so its first non-blank line lines up
///    with the file's match indent. This catches the "model is off
///    by one tab depth" failure mode without weakening exact-match
///    semantics — only kicks in when the strict match misses.
/// 5. **Whitespace-collapsing fallback** — last resort for the
///    "prettier rewrote it" failure mode: collapse every run of ASCII
///    whitespace (space, tab, CR, LF) to a single space on both
///    `find` and the file, anchor the comparison on `find.trim()`,
///    and locate the match in normalised space. On success, splice
///    the *original* file byte range that produced the matched
///    normalised window and drop `replace` in verbatim. Catches the
///    cases the indent stage can't — multi-space runs collapsed to
///    one, line breaks rewritten as spaces (or vice-versa), trailing
///    whitespace differences — without baking any per-formatter
///    rules in.
///
/// The fuzzy paths assume format-on-save runs after the splice and
/// will catch any residual whitespace / indentation skew. Without a
/// formatter the edit may land with the model's exact bytes dropped
/// into a slightly different file shape; that's the deliberate trade
/// — fewer "find not found" loops at the cost of a one-off
/// re-format the formatter will normalise anyway.
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

	// Stage 3: backslash-run-collapsing. The model regularly sends
	// a `find` whose backslash count is off by one — `\w` vs `\\w`
	// in a template-literal `RegExp(...)`, `\\$&` vs `\\\\$&` in a
	// JS string, etc. Normalise every run of consecutive `\` bytes
	// to a single `\` on both sides, then re-search. Splice the
	// file's original byte range so the file's own escaping
	// survives the edit; `replace` is the model's responsibility,
	// same as the whitespace stage.
	let bs_hits = find_backslash_collapsed(text, find);
	if !bs_hits.is_empty() {
		let (chosen, picked) = select_bs(&bs_hits, occurrence, path_for_error, text)?;
		return Ok(EditPlan {
			start: chosen.start,
			end: chosen.end,
			replace_text: replace.to_owned(),
			total_matches: bs_hits.len(),
			occurrence: picked,
			mode: "fuzzy_backslash",
		});
	}

	// Stage 4: per-line indent-tolerant. Only well-defined when
	// `find` is line-aligned (every line is on its own); a mid-line
	// `find` falls through to the next stage.
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

	// Stage 5: whitespace-collapsing. Normalises every run of ASCII
	// whitespace to a single space on both sides, then anchors the
	// match on `find.trim()` so we don't accidentally swallow file
	// whitespace at the splice boundary. Last resort for cases where
	// the file's shape no longer matches the model's — formatter
	// rewrote a multi-line call into one line, split a long string
	// across lines, collapsed indentation, etc.
	let ws_hits = find_whitespace_collapsed(text, find);
	if !ws_hits.is_empty() {
		let (chosen, picked) = select_ws(&ws_hits, occurrence, path_for_error, text)?;
		return Ok(EditPlan {
			start: chosen.start,
			end: chosen.end,
			replace_text: replace.to_owned(),
			total_matches: ws_hits.len(),
			occurrence: picked,
			mode: "fuzzy_whitespace",
		});
	}

	Err(CoderError::tool_failed(
		"edit_file",
		format!(
			"`find` not found in {path_for_error}. The file's bytes did not match `find` exactly, no \
indent-tolerant match was found, and no whitespace-collapsing match was found either. Re-run \
`read_file` to see the current state of the file and pass `find` with content that actually appears \
in it (whitespace differences are tolerated; missing or different non-whitespace characters are not)."
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
/// string. Backslash-count mismatches have their own dedicated
/// stage — see [`find_backslash_collapsed`].
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

/// A single backslash-collapsing hit. `start..end` is the byte range
/// in the original file that the splice will replace; we don't carry
/// any extra context because the replacement bytes come from the
/// model verbatim (same shape as [`WsMatch`]).
struct BsMatch {
	start: usize,
	end: usize,
}

/// Collapse every run of consecutive `\` bytes to a single `\` on
/// both `find` and the file, then locate the needle in the file's
/// normalised form. On a hit, the index map carries us back to the
/// original-file byte range that produced it.
///
/// Why this stage exists: the model's tool-call pipeline (thought →
/// JSON → us) regularly miscounts backslashes in regex literals,
/// template-literal `RegExp(...)` patterns, and `String.replace`
/// replacement strings. We saw a real session where the on-disk
/// `(^|[^\\w-])@${escaped}` came back as `(^|[^\w-])@${escaped}` in
/// `find` (under-escaped) and on the retry as `(^|[^\\\\w-])` (over-
/// escaped). Both attempts failed against the exact / unescape /
/// indent / whitespace stages, and the model gave up and wrote a
/// less-good edit. Collapsing runs of backslashes catches both
/// directions in one pass.
///
/// We splice the file's *original* byte range so the file's own
/// escaping survives the edit — the model owns its `replace` bytes
/// here, same trade as [`find_whitespace_collapsed`]. If `find`
/// contains no `\` at all this stage is a no-op vs. Stage 1 and
/// would just rediscover the exact match, so we early-return.
fn find_backslash_collapsed(text: &str, find: &str) -> Vec<BsMatch> {
	if !find.as_bytes().contains(&b'\\') {
		return Vec::new();
	}
	let norm_find = collapse_backslash_runs(find);
	let norm_text = normalize_backslash_runs_with_map(text);
	if norm_find.is_empty() || norm_text.text.is_empty() {
		return Vec::new();
	}
	let hits = byte_offsets_of_bytes(&norm_text.text, &norm_find);
	hits
		.into_iter()
		.map(|i| BsMatch {
			start: norm_text.orig_start[i],
			end: norm_text.orig_end[i + norm_find.len() - 1],
		})
		.collect()
}

/// Mirror of [`select_ws`] for backslash-collapsed matches. Kept
/// separate so the ambiguity error message names the right stage —
/// helps when someone is reading a session log and trying to figure
/// out which fallback misfired.
fn select_bs<'a>(
	matches: &'a [BsMatch],
	occurrence: Option<usize>,
	path: &str,
	text: &str,
) -> Result<(&'a BsMatch, usize), CoderError> {
	match (matches.len(), occurrence) {
		(0, _) => unreachable!("select_bs called with empty matches"),
		(1, None | Some(1)) => Ok((&matches[0], 1)),
		(n, None) => {
			let lines: Vec<u32> = matches.iter().map(|m| line_number_at_byte(text, m.start)).collect();
			let lines_csv = lines.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
			Err(CoderError::tool_failed(
				"edit_file",
				format!(
					"`find` backslash-collapsed match was ambiguous in {path} ({n} hits at lines \
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

/// Per-byte mapping from the backslash-collapsed form of a string
/// back to original-file byte ranges. For each byte `i` in
/// [`NormalizedBs::text`], `orig_start[i]..orig_end[i]` is the byte
/// range in the original input that produced it — a run of N
/// backslashes collapses to a single output byte whose range spans
/// all N input bytes, so the splice picks up the file's actual
/// escaping when we map back.
struct NormalizedBs {
	text: Vec<u8>,
	orig_start: Vec<usize>,
	orig_end: Vec<usize>,
}

/// Walk `s` once, emitting every non-`\` byte verbatim and every
/// run of one-or-more `\` bytes as a single `\`, with the index map
/// pointing at the full original run. Linear in input length; we
/// touch each byte at most twice.
fn normalize_backslash_runs_with_map(s: &str) -> NormalizedBs {
	let bytes = s.as_bytes();
	let mut text = Vec::with_capacity(bytes.len());
	let mut orig_start = Vec::with_capacity(bytes.len());
	let mut orig_end = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'\\' {
			let run_start = i;
			while i < bytes.len() && bytes[i] == b'\\' {
				i += 1;
			}
			text.push(b'\\');
			orig_start.push(run_start);
			orig_end.push(i);
			continue;
		}
		text.push(bytes[i]);
		orig_start.push(i);
		orig_end.push(i + 1);
		i += 1;
	}
	NormalizedBs {
		text,
		orig_start,
		orig_end,
	}
}

/// Same normalisation as [`normalize_backslash_runs_with_map`]
/// without the index map — used to build the needle once.
fn collapse_backslash_runs(s: &str) -> Vec<u8> {
	normalize_backslash_runs_with_map(s).text
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

/// A single whitespace-collapsed hit. `start..end` is the byte range
/// in the *original* file that the splice replaces — covers exactly
/// the content the trimmed/normalised `find` anchored against,
/// including any internal whitespace runs that got collapsed to a
/// single space for matching. No re-indent of `replace`; format-on-save
/// is expected to reshape the spliced text.
struct WsMatch {
	start: usize,
	end: usize,
}

/// Whitespace-collapsing match. Normalises every run of ASCII
/// whitespace (` `, `\t`, `\r`, `\n`) to a single space on both
/// `find` and the file, then trims `find` so leading/trailing
/// whitespace in the model's input doesn't force the splice to
/// swallow file whitespace at the boundary. Returns the list of
/// non-overlapping hits as original-file byte ranges.
///
/// Catches the cases the indent-tolerant stage can't: formatters
/// (prettier, oxfmt, black) often rewrite multi-line function calls
/// into one line or split a long string across several. After such a
/// rewrite the model's earlier `find` no longer line-aligns, but the
/// content it cares about is still there in the file — just spaced
/// differently. This stage anchors on content, not shape.
fn find_whitespace_collapsed(text: &str, find: &str) -> Vec<WsMatch> {
	let norm_find = collapse_ws(find);
	let norm_text = normalize_ws_with_map(text);
	if norm_find.is_empty() || norm_text.text.is_empty() {
		return Vec::new();
	}
	let hits = byte_offsets_of_bytes(&norm_text.text, &norm_find);
	hits
		.into_iter()
		.map(|i| WsMatch {
			start: norm_text.orig_start[i],
			end: norm_text.orig_end[i + norm_find.len() - 1],
		})
		.collect()
}

fn select_ws<'a>(
	matches: &'a [WsMatch],
	occurrence: Option<usize>,
	path: &str,
	text: &str,
) -> Result<(&'a WsMatch, usize), CoderError> {
	match (matches.len(), occurrence) {
		(0, _) => unreachable!("select_ws called with empty matches"),
		(1, None | Some(1)) => Ok((&matches[0], 1)),
		(n, None) => {
			let lines: Vec<u32> = matches.iter().map(|m| line_number_at_byte(text, m.start)).collect();
			let lines_csv = lines.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
			Err(CoderError::tool_failed(
				"edit_file",
				format!(
					"`find` whitespace-collapsed match was ambiguous in {path} ({n} hits at lines \
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

fn is_ascii_ws_byte(b: u8) -> bool {
	matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Normalise a UTF-8 string for whitespace-tolerant matching.
///
/// Walks the bytes and rewrites runs of ASCII whitespace using a
/// context-sensitive rule:
///
/// - Whitespace **between two word bytes** (`[A-Za-z0-9_]`) collapses
///   to a single `' '`. This keeps `let foo` from matching `letfoo`
///   — losing the space would turn separate identifiers into one.
/// - Whitespace **adjacent to a non-word byte** on either side
///   (punctuation like `(`, `)`, `,`, `=`, …, or the start/end of the
///   string) is dropped entirely. Prettier-style reformatting moves
///   freely around punctuation, so `foo( a, b )` and `foo(a,b,)` and
///   `foo(\n\ta,\n\tb,\n)` all normalise to the same `foo(a,b,)`.
///
/// Bytes are emitted as `Vec<u8>` rather than `String` so multi-byte
/// UTF-8 continuation bytes round-trip without re-encoding (and so
/// the per-byte index map stays well-defined). All non-ASCII bytes
/// are treated as non-word, which is the safe default — non-ASCII
/// identifiers exist but they're rare and never collide with the
/// ASCII-only word-boundary heuristic.
fn normalize_ws_with_map(s: &str) -> NormalizedWs {
	let bytes = s.as_bytes();
	let mut text = Vec::with_capacity(bytes.len());
	let mut orig_start = Vec::with_capacity(bytes.len());
	let mut orig_end = Vec::with_capacity(bytes.len());
	let mut i = 0;
	let mut last_non_ws: Option<u8> = None;
	while i < bytes.len() {
		if is_ascii_ws_byte(bytes[i]) {
			let run_start = i;
			while i < bytes.len() && is_ascii_ws_byte(bytes[i]) {
				i += 1;
			}
			let next = bytes.get(i).copied();
			let keep = matches!(last_non_ws, Some(b) if is_word_byte(b)) && matches!(next, Some(b) if is_word_byte(b));
			if keep {
				text.push(b' ');
				orig_start.push(run_start);
				orig_end.push(i);
			}
			continue;
		}
		text.push(bytes[i]);
		orig_start.push(i);
		orig_end.push(i + 1);
		last_non_ws = Some(bytes[i]);
		i += 1;
	}
	NormalizedWs {
		text,
		orig_start,
		orig_end,
	}
}

/// The normalised form of a source text plus the per-byte mapping
/// back to original byte ranges. For each byte `i` in
/// [`NormalizedWs::text`], `orig_start[i]..orig_end[i]` is the byte
/// range in the original input that produced it. See
/// [`normalize_ws_with_map`] for the normalisation rule.
struct NormalizedWs {
	text: Vec<u8>,
	orig_start: Vec<usize>,
	orig_end: Vec<usize>,
}

/// Same context-sensitive normalisation as
/// [`normalize_ws_with_map`] but without the index map — used to
/// build the needle once for the search. Keeping a single source of
/// truth for the rule prevents the two from drifting.
fn collapse_ws(s: &str) -> Vec<u8> {
	normalize_ws_with_map(s).text
}

fn is_word_byte(b: u8) -> bool {
	b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte-level substring search. Same `+= needle.len()` advancement
/// `byte_offsets_of` uses; needed here because the normalised text
/// is `Vec<u8>` rather than `&str` (see [`NormalizedWs`] for why).
fn byte_offsets_of_bytes(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
	if needle.is_empty() || haystack.len() < needle.len() {
		return Vec::new();
	}
	let mut hits = Vec::new();
	let mut start = 0;
	while start + needle.len() <= haystack.len() {
		if &haystack[start..start + needle.len()] == needle {
			hits.push(start);
			start += needle.len();
		} else {
			start += 1;
		}
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
	fn locate_edit_backslash_fallback_handles_under_escape() {
		// Real failure mode from a moon-coder session: the file
		// contains a template-literal RegExp with source-level
		// double-backslash escapes (`\\w`, `\\[`, `\\]`); the
		// model's `find` had single backslashes (`\w`, `\[`,
		// `\]`) because it lost a level of escaping somewhere in
		// the thought-to-JSON pipeline. Stage 3 should locate
		// the line, splice the file's original bytes, and drop
		// the model's `replace` in verbatim.
		let file = "  const tokenRe = new RegExp(`(^|[^\\\\w-])@${escaped}(?:\\\\[bot\\\\])?(?![\\\\w-])`, \"gi\");\n";
		let find = "  const tokenRe = new RegExp(`(^|[^\\w-])@${escaped}(?:\\[bot\\])?(?![\\w-])`, \"gi\");";
		let replace = "  const tokenRe = REPLACED;";
		let plan = locate_edit(file, find, replace, None, "src/github.ts").expect("backslash-collapse match");
		assert_eq!(plan.mode, "fuzzy_backslash");
		// Splice covers the file's original line bytes (preserving
		// the `\\w` double-backslashes), not the model's `\w`.
		assert_eq!(
			&file[plan.start..plan.end],
			"  const tokenRe = new RegExp(`(^|[^\\\\w-])@${escaped}(?:\\\\[bot\\\\])?(?![\\\\w-])`, \"gi\");"
		);
		assert_eq!(plan.replace_text, "  const tokenRe = REPLACED;");
	}

	#[test]
	fn locate_edit_backslash_fallback_handles_over_escape() {
		// Mirror direction: model double-escaped where the file
		// has a single backslash (`\\w` in find, `\w` on disk).
		// Same stage catches both because we collapse *runs* of
		// `\` on both sides.
		let file = "const re = /\\w+/g;\n";
		let find = "const re = /\\\\w+/g;";
		let replace = "const re = /[a-z]+/g;";
		let plan = locate_edit(file, find, replace, None, "src/foo.ts").expect("backslash-collapse match");
		assert_eq!(plan.mode, "fuzzy_backslash");
		assert_eq!(&file[plan.start..plan.end], "const re = /\\w+/g;");
		assert_eq!(plan.replace_text, "const re = /[a-z]+/g;");
	}

	#[test]
	fn locate_edit_backslash_fallback_skipped_when_find_has_no_backslash() {
		// Cheap pre-check: `find` without any `\` should never
		// engage Stage 3 — it'd just rediscover whatever Stage 1
		// already saw. Important so a legitimate "not found"
		// stays on Stage 1's exact path and surfaces the strict
		// error rather than the backslash-stage one.
		let file = "hello world\n";
		let err = locate_edit(file, "missing", "x", None, "src/foo.rs").unwrap_err();
		let msg = err.to_string();
		// Falls through every stage to the final actionable error.
		assert!(msg.contains("src/foo.rs"));
		assert!(msg.contains("indent-tolerant"));
	}

	#[test]
	fn locate_edit_backslash_fallback_yields_to_exact() {
		// Exact must always win, even when `find` contains
		// backslashes that *would* also match under collapse.
		// Otherwise Stage 3 would steal hits and silently strip
		// the model's intentional escaping in `replace`.
		let file = "let s = \"a\\nb\";\n";
		let find = "let s = \"a\\nb\";";
		let replace = "let s = REPLACED;";
		let plan = locate_edit(file, find, replace, None, "src/foo.rs").expect("exact match wins");
		assert_eq!(plan.mode, "exact");
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
	fn locate_edit_whitespace_fallback_collapses_multiple_spaces() {
		// Prettier collapsed a `foo(  a,  b)` style call into the
		// usual single-space convention. The model still has the
		// pre-format `find` from an earlier `read_file`; the
		// whitespace-collapse stage matches anyway and the splice
		// covers exactly the content range (no `replace` re-shape —
		// format-on-save handles that downstream).
		let file = "let x = foo(a, b, c);\n";
		let find = "foo(a,  b,  c)";
		let replace = "foo(a, b, c, d)";
		let plan = locate_edit(file, find, replace, None, "test.rs").expect("ws-collapse match");
		assert_eq!(plan.mode, "fuzzy_whitespace");
		assert_eq!(&file[plan.start..plan.end], "foo(a, b, c)");
		assert_eq!(plan.replace_text, "foo(a, b, c, d)");
	}

	#[test]
	fn locate_edit_whitespace_fallback_collapses_multi_line_call() {
		// Mirror case: the file has the multi-line version, the
		// model gives a single-line `find`. We still find it, and
		// the splice spans the whole multi-line region so the
		// replacement drops in cleanly. Format-on-save reshapes.
		let file = "let x = foo(\n\ta,\n\tb,\n\tc,\n);\n";
		let find = "foo(a, b, c,)";
		let replace = "foo(a, b, c, d,)";
		let plan = locate_edit(file, find, replace, None, "test.rs").expect("multi-line ws-collapse match");
		assert_eq!(plan.mode, "fuzzy_whitespace");
		assert_eq!(&file[plan.start..plan.end], "foo(\n\ta,\n\tb,\n\tc,\n)");
		assert_eq!(plan.replace_text, "foo(a, b, c, d,)");
	}

	#[test]
	fn locate_edit_whitespace_fallback_runs_after_indent_stage() {
		// When the indent-tolerant stage can match, *it* takes
		// precedence — `mode` is `fuzzy_indent`, not
		// `fuzzy_whitespace`. Guards the ordering invariant: indent
		// is conservative (preserves shape, re-indents `replace`),
		// whitespace-collapse is the loosest net.
		let file = "\terror: {\n\t\tname: x,\n\t},\n";
		let find = "error: {\n\tname: x,\n},";
		let replace = "error: {\n\tname: x,\n\tstatus: 500,\n},";
		let plan = locate_edit(file, find, replace, None, "test.rs").expect("indent path takes precedence");
		assert_eq!(plan.mode, "fuzzy_indent");
	}

	#[test]
	fn locate_edit_whitespace_fallback_ambiguous_lists_lines() {
		// Two whitespace-collapse hits, no `occurrence` → error
		// names both line numbers so the model can disambiguate.
		// Use a `find` that doesn't match exactly (different
		// internal spacing) so it falls through to the ws stage
		// before the multi-match check fires.
		let file = "let a = foo(x, y);\nlet b = foo(x,  y);\n";
		let find = "foo(x,   y)";
		let err = locate_edit(file, find, "X", None, "dup.rs").unwrap_err();
		let msg = err.to_string();
		assert!(
			msg.contains("whitespace-collapsed"),
			"error should mention the stage, got: {msg}"
		);
		assert!(msg.contains("lines 1, 2"), "error should list line numbers, got: {msg}");
	}

	#[test]
	fn locate_edit_whitespace_fallback_anchors_on_punctuation() {
		// Sloppy whitespace inside punctuation (`(  a , b  )`)
		// can't be reached by the indent stage (line content
		// differs after the trim) but the ws-collapse stage drops
		// whitespace adjacent to non-word characters and lands the
		// match. Leading / trailing ws on `find` doesn't push the
		// splice past the matched content — the normalisation
		// strips both because they're adjacent to the
		// start/end-of-string sentinel.
		let file = "let x = foo(a,b);\n";
		let find = "  foo(  a , b  )  ";
		let replace = "foo(a, b, c)";
		let plan = locate_edit(file, find, replace, None, "test.rs").expect("ws-collapse via punctuation");
		assert_eq!(plan.mode, "fuzzy_whitespace");
		assert_eq!(&file[plan.start..plan.end], "foo(a,b)");
		assert_eq!(plan.replace_text, "foo(a, b, c)");
	}

	#[test]
	fn collapse_ws_keeps_space_between_word_chars_only() {
		// Between word chars: collapses to one space (don't merge
		// adjacent identifiers).
		assert_eq!(super::collapse_ws("a  b\tc\nd"), b"a b c d");
		// Adjacent to punctuation / EOL: dropped entirely.
		assert_eq!(super::collapse_ws("foo( a, b )"), b"foo(a,b)");
		assert_eq!(super::collapse_ws("  hello  "), b"hello");
		// All-whitespace inputs reduce to nothing.
		assert_eq!(super::collapse_ws("   "), b"");
		assert_eq!(super::collapse_ws(""), b"");
	}

	#[test]
	fn normalize_ws_with_map_preserves_word_boundary_space() {
		// `let  x = 1;` → `let x=1;` (space between `t` and `x`
		// kept; whitespace around `=` and after `1` adjacent to
		// non-word, dropped).
		let norm = super::normalize_ws_with_map("let  x = 1;");
		assert_eq!(norm.text, b"let x=1;");
		// `l e t` map to bytes 0,1,2; the run "  " between `t`
		// and `x` (bytes 3..5) emits ' ' → orig [3, 5); `x` → 5;
		// the run " " (byte 6) around `=` is dropped; `=` → 7;
		// the run " " (byte 8) dropped; `1` → 9; `;` → 10.
		assert_eq!(norm.orig_start, vec![0, 1, 2, 3, 5, 7, 9, 10]);
		assert_eq!(norm.orig_end, vec![1, 2, 3, 5, 6, 8, 10, 11]);
	}

	#[test]
	fn normalize_ws_with_map_preserves_multibyte_utf8() {
		// `é` is two bytes (0xC3 0xA9). Both bytes count as
		// non-word and round-trip to themselves; the map is
		// byte-precise so a hit ending on `é`'s last byte still
		// lands on a valid char boundary in the original.
		let norm = super::normalize_ws_with_map("café x");
		assert_eq!(norm.text, b"caf\xC3\xA9x"); // " " before `x` is dropped — `é`'s last byte is non-word ASCII-wise.
		assert_eq!(norm.orig_start.len(), norm.text.len());
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
		async fn unrelated_absolute_path_resolves_out_of_workspace() {
			// Absolute paths outside every bound folder root now
			// route to the out-of-workspace primitives (container
			// when running, host otherwise) instead of being
			// gated. See ADR 0025.
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let target = tools
				.resolve_target("/etc/passwd", &cx, "read_file")
				.await
				.expect("unrelated absolute paths resolve");
			match target {
				super::super::ResolvedTarget::OutOfWorkspace { abs_path } => {
					assert_eq!(abs_path.as_str(), "/etc/passwd");
				}
				super::super::ResolvedTarget::InWorkspace { .. } => {
					panic!("expected out-of-workspace routing for /etc/passwd")
				}
			}
		}

		#[tokio::test]
		async fn out_of_workspace_read_write_round_trips_on_host() {
			// No container in the test environment, so the
			// out-of-workspace primitives fall back to the host
			// filesystem. A file outside every bound folder should
			// read and write the same bytes back — proving the gate
			// is lifted end-to-end, not just at the resolver.
			let bound = TempDir::new().unwrap();
			let bound_path = camino::Utf8PathBuf::from_path_buf(bound.path().to_path_buf()).unwrap();
			// A *separate* temp dir, deliberately not bound.
			let external = TempDir::new().unwrap();
			let external_path = camino::Utf8PathBuf::from_path_buf(external.path().canonicalize().unwrap()).unwrap();
			let file = external_path.join("outside.txt");

			let (registry, tools) = build_registry(&[bound_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;

			let write_args = serde_json::json!({ "path": file.as_str(), "content": "hello outside\n" });
			tools
				.write_file(&write_args, &cx)
				.await
				.expect("out-of-workspace write should succeed on host");

			let read_args = serde_json::json!({ "path": file.as_str() });
			let read = tools
				.read_file(&read_args, &cx)
				.await
				.expect("out-of-workspace read should succeed on host");
			assert_eq!(read["content"].as_str().unwrap(), "1|hello outside\n");
		}

		#[tokio::test]
		async fn out_of_workspace_list_dir_lists_external_children_on_host() {
			let bound = TempDir::new().unwrap();
			let bound_path = camino::Utf8PathBuf::from_path_buf(bound.path().to_path_buf()).unwrap();
			let external = TempDir::new().unwrap();
			let external_path = camino::Utf8PathBuf::from_path_buf(external.path().canonicalize().unwrap()).unwrap();
			std::fs::write(external_path.join("a.txt"), "x").unwrap();
			std::fs::create_dir(external_path.join("sub")).unwrap();

			let (registry, tools) = build_registry(&[bound_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;

			let args = serde_json::json!({ "path": external_path.as_str() });
			let out = tools.list_dir(&args, &cx).await.expect("out-of-workspace list_dir");
			let entries = out["entries"].as_str().unwrap();
			assert!(entries.contains("dir  sub"), "entries: {entries}");
			assert!(entries.contains("file a.txt"), "entries: {entries}");
		}

		#[tokio::test]
		async fn absolute_host_path_inside_active_folder_strips_to_relative() {
			// Host-mode form: the system prompt advertises bound
			// folders by their host abs path; the model joins file
			// paths onto it. The resolver must route to the matching
			// folder and emit the inside-folder relative path.
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let raw = format!("{}/src/foo.rs", one_path.as_str());
			let (folder, out) = tools
				.resolve_workspace_path(&raw, &cx, "read_file")
				.await
				.expect("abs path under active folder should resolve");
			assert_eq!(out, "src/foo.rs");
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}

		#[tokio::test]
		async fn absolute_host_path_at_folder_root_resolves_to_dot() {
			let one = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let (folder, out) = tools
				.resolve_workspace_path(one_path.as_str(), &cx, "list_dir")
				.await
				.expect("abs path equal to folder root should resolve to '.'");
			assert_eq!(out, ".");
			assert!(Arc::ptr_eq(&folder, &cx.folder));
		}

		#[tokio::test]
		async fn absolute_host_path_inside_sibling_routes_cross_folder() {
			let one = TempDir::new().unwrap();
			let two = TempDir::new().unwrap();
			let one_path = camino::Utf8PathBuf::from_path_buf(one.path().to_path_buf()).unwrap();
			let two_path = camino::Utf8PathBuf::from_path_buf(two.path().to_path_buf()).unwrap();
			let (registry, tools) = build_registry(&[one_path.as_path(), two_path.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let folders = registry.folders().await;
			let other = folders[1].clone();
			let raw = format!("{}/src/foo.rs", two_path.as_str());
			let (target, out) = tools
				.resolve_workspace_path(&raw, &cx, "read_file")
				.await
				.expect("abs path under sibling should route cross-folder");
			assert_eq!(out, "src/foo.rs");
			assert!(Arc::ptr_eq(&target, &other));
		}

		#[tokio::test]
		async fn absolute_host_path_under_nested_bound_folder_picks_inner() {
			// `/foo/bar/sub` should route to `/foo/bar/sub` when both
			// `/foo/bar` and `/foo/bar/sub` are bound — the longer
			// match wins, matching how the file tree groups files.
			let outer = TempDir::new().unwrap();
			let outer_path = camino::Utf8PathBuf::from_path_buf(outer.path().to_path_buf()).unwrap();
			let inner_dir = outer_path.join("nested");
			std::fs::create_dir(inner_dir.as_std_path()).unwrap();
			let (registry, tools) = build_registry(&[outer_path.as_path(), inner_dir.as_path()]).await;
			let cx = make_cx(&registry, 0).await;
			let inner_entry = registry.folders().await[1].clone();
			let raw = format!("{}/file.rs", inner_dir.as_str());
			let (target, out) = tools
				.resolve_workspace_path(&raw, &cx, "read_file")
				.await
				.expect("nested bound folder path should resolve to the inner folder");
			assert_eq!(out, "file.rs");
			assert!(Arc::ptr_eq(&target, &inner_entry));
		}
	}
}
