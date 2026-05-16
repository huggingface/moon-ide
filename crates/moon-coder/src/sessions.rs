//! Session persistence for the coder agent.
//!
//! Each session is a JSONL file at
//! `<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/<id>.jsonl`.
//! The first line is a header carrying metadata (id, title,
//! timestamps, model); every subsequent line is one append-only
//! [`SessionRecord`] capturing user input, assistant output, or
//! tool I/O.
//!
//! ## Why a global data dir, not the project tree
//!
//! Storing sessions inside the project tree puts them under
//! version control by default and on the user's laptop rather
//! than tied to their account. Both wrong: sessions are personal
//! scratch / history, not project artefacts. The layout sits
//! next to compose state under `<XDG_DATA_HOME>/moon-ide/`, with
//! a `<project-slug>/` subdirectory derived deterministically
//! from the absolute folder path so the same project always
//! maps to the same directory across launches and across
//! teammates' machines maps to *different* directories (their
//! absolute paths differ).
//!
//! ## Project slug
//!
//! `<basename>-<8-char FNV-1a hex>`. Basename keeps the directory
//! readable when the user goes spelunking; the hex suffix
//! disambiguates two folders that happen to share a basename
//! (`projects/api` vs `clients/foo/api`). FNV-1a is small,
//! deterministic, and pure-Rust — `sha2` would be overkill for
//! eight bytes of disambiguation.
//!
//! ## Lazy persistence
//!
//! A freshly-created session has no file on disk yet — we write
//! the header on the first record append. That way "spam the +
//! button" doesn't litter the directory with empty sessions.
//! Once the first user message is committed, every subsequent
//! event flushes immediately so a crash mid-turn loses at most
//! the last in-flight chunk.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::CoderError;
use crate::inference::ToolCall;

/// On-disk schema version for the JSONL header. Bumped when the
/// header or record shape changes incompatibly. Per AGENTS.md "no
/// premature migrations" we don't ship migration code; sessions
/// from older schemas are surfaced as parse errors at load time
/// (the panel falls back to the empty state and logs a warning).
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// File extension on every session file.
const SESSION_EXT: &str = "jsonl";

/// JSONL header — first line of every session file. Must be the
/// first record because the [`load_summary`] fast path stops after
/// reading exactly one line.
///
/// Sub-agent sessions reuse this same struct with the optional
/// `parent_*` / `subagent_mode` fields populated. Top-level
/// (parent) sessions leave them `None`; the optional fields are
/// elided from JSON via `skip_serializing_if`, so existing
/// on-disk sessions stay byte-compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
	pub schema: u32,
	pub id: String,
	/// Human-readable title. Auto-derived from the first user
	/// prompt at create time (`session_title_from_prompt`), then
	/// optionally overwritten by the auto-rename pass after the
	/// first turn completes (which appends a [`SessionRecord::TitleUpdate`]
	/// record so a re-open replays the rename).
	pub title: String,
	pub created_at_ms: i64,
	pub updated_at_ms: i64,
	/// Model the session was started with. Stored once at the
	/// header and then echoed onto session metadata; doesn't bind
	/// individual turns (Phase 6.4 will add per-session model
	/// override that lives here).
	pub model: String,
	/// Parent session id, when this header describes a sub-agent
	/// session. `None` for top-level (user-driven) sessions.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parent_session_id: Option<String>,
	/// `tool_call_id` of the parent's `spawn_subagent` call that
	/// produced this sub-agent. Lets the UI's "pop out" affordance
	/// resolve the sub-agent's transcript across IDE restarts.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parent_tool_call_id: Option<String>,
	/// Wire string ("research" / "agent") of the mode the
	/// sub-agent ran under. `None` for top-level sessions; mirrors
	/// `CoderMode::as_wire()` so the frontend reads it verbatim.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub subagent_mode: Option<String>,
	/// Absolute path of the folder the sub-agent's tools operated
	/// against. May differ from the parent's bound folder (which
	/// owns the JSONL on disk) when the parent passed an explicit
	/// `folder` argument to `spawn_subagent`. `None` for top-level
	/// sessions and for sub-agent sessions that targeted the same
	/// folder as their parent.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub subagent_target_folder: Option<String>,
}

/// One append-only record in the JSONL body. Tagged enum so each
/// line is self-describing — the loader doesn't need to track
/// state to decide what kind of record comes next.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRecord {
	/// One user prompt landed. `images` is the data-URL form of
	/// any pictures pasted into the composer with the prompt;
	/// empty for the vast majority of turns and elided from the
	/// JSONL line in that case so a no-image transcript stays
	/// trivially diffable.
	User {
		text: String,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		images: Vec<crate::inference::ImageAttachment>,
	},
	/// One assistant turn completed. Carries the canonical full
	/// content + reasoning trace + any tool calls the model
	/// emitted. Both `content` and `thinking` are `None` when the
	/// turn was tool-only / reasoning-disabled.
	Assistant {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		content: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		thinking: Option<String>,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		tool_calls: Vec<ToolCall>,
	},
	/// One tool result fed back to the model. `tool_call_id`
	/// matches the `id` on the parent [`SessionRecord::Assistant`]
	/// record's `tool_calls`.
	Tool { tool_call_id: String, content: String },
	/// The session title changed mid-stream. Replayed on reopen so
	/// the auto-renamed title sticks across launches without a
	/// rewrite-the-header-in-place dance.
	TitleUpdate { title: String },
	/// Provider-supplied token usage from the round-trip that
	/// just landed. Appended after every parent-loop call whose
	/// response carried a `usage` chunk; absent for round-trips
	/// where the provider didn't emit usage (we don't bother
	/// persisting bytes/4 estimates — they're recomputable from
	/// the message history). On reopen, the last `Usage` record
	/// drives the post-replay `TokenUsage` event so the panel's
	/// context-usage ring shows provider-exact figures from the
	/// moment the session re-mounts, instead of a bytes/4
	/// estimate that often lands 20–30 % off. Cache fields only
	/// emitted when non-zero (Anthropic via OpenRouter).
	///
	/// Sub-agents share the same JSONL machinery and persist their
	/// own Usage records into their per-parent subdir, but the
	/// "open session" path doesn't load sub-agent files (the
	/// reload is top-level only) so those records are inert until
	/// somebody adds a sub-agent restore path. Persisting them
	/// today still costs nothing and pays off the moment that path
	/// lands.
	Usage {
		prompt_tokens: u32,
		completion_tokens: u32,
		total_tokens: u32,
		#[serde(default, skip_serializing_if = "u32_is_zero")]
		cache_read_input_tokens: u32,
		#[serde(default, skip_serializing_if = "u32_is_zero")]
		cache_creation_input_tokens: u32,
	},
	/// One auto-compaction pass landed. The runtime drains the
	/// older message prefix and replaces it with a synthetic
	/// system message holding `summary`; this record is the
	/// on-disk twin so replay reaches the same in-memory shape.
	/// Without it, reopening a long session re-inflates the full
	/// pre-compaction transcript and the next turn instantly
	/// trips the provider's context-length cap.
	///
	/// `messages_compacted` mirrors the value the runtime emits
	/// in [`crate::CoderEvent::CompactionStarted`], kept here
	/// for symmetry / debugging — replay doesn't actually need
	/// it (the truncation logic is purely "drop everything since
	/// the system prompt and inject the summary").
	Compaction { summary: String, messages_compacted: u32 },
	/// Snapshot of the session's todo list after one `todo_write`
	/// call. Append-only: each call writes one record carrying the
	/// **full** post-merge list (the same list the model sees as
	/// the tool result, so on-disk and in-context never drift).
	///
	/// On replay the last `TodosUpdate` wins; intermediate ones
	/// are read but discarded. The list is small (a few items at
	/// most), and replay throws them away anyway, so the on-disk
	/// cost is negligible compared to the simplicity of "tool
	/// result == record body".
	///
	/// The record is **not** wired into the post-replay event
	/// stream as a synthetic event: the panel reconstructs its
	/// `coder.todos` bucket from the same `tool_result` payloads
	/// it sees during replay (the runner re-emits them as
	/// [`crate::CoderEvent::ToolResult`]), so we don't need a
	/// dedicated `TodosLoaded` event for the empty-list case
	/// either — an unset bucket renders the same "no list" pill.
	TodosUpdate { todos: Vec<crate::TodoItem> },
}

fn u32_is_zero(n: &u32) -> bool {
	*n == 0
}

/// Lightweight summary used by the panel's session list. Avoids
/// loading every record off disk when the user just wants to pick
/// a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
	pub id: String,
	pub title: String,
	pub created_at_ms: i64,
	pub updated_at_ms: i64,
}

/// Full load: header + every record in order. Used when the user
/// opens a session and we need to replay events into the panel
/// + reconstruct the chat history for the runner.
pub struct LoadedSession {
	pub header: SessionHeader,
	pub records: Vec<SessionRecord>,
}

/// Resolve the sessions directory for one workspace folder under
/// the global coder-sessions root. Doesn't create the directory —
/// that happens lazily when the first session writes its header.
///
/// `coder_sessions_root` is the per-machine
/// `<XDG_DATA_HOME>/moon-ide/coder-sessions/` directory, owned by
/// the Tauri layer; `folder_root` is the absolute workspace
/// folder path the session is bound to.
pub fn sessions_dir(coder_sessions_root: &Utf8Path, folder_root: &Utf8Path) -> Utf8PathBuf {
	coder_sessions_root.join(project_slug(folder_root))
}

/// Per-parent-session directory for sub-agent transcripts. Every
/// sub-agent spawned under `<sessions_dir>/<parent_session_id>.jsonl`
/// persists its own JSONL into
/// `<sessions_dir>/<parent_session_id>/<sub-id>.jsonl`.
///
/// Created lazily when the first sub-agent for that parent writes
/// its header (`persist_subagent` calls `create_dir_all`). The
/// subdir is empty for sessions that never spawned anything, and
/// absent for sessions where no spawn ever fired — so a flat
/// listing of `<sessions_dir>/` plus the `*.jsonl` extension
/// filter naturally excludes sub-agents from the session picker.
pub fn subagent_session_dir(sessions_dir: &Utf8Path, parent_session_id: &str) -> Utf8PathBuf {
	sessions_dir.join(parent_session_id)
}

/// Deterministic short slug for a workspace folder: the basename
/// suffixed with an 8-char FNV-1a hash of the canonical path.
/// Same folder → same slug across launches; different folders →
/// different slugs even when their basenames collide.
pub fn project_slug(folder_root: &Utf8Path) -> String {
	let raw = folder_root.as_str();
	let basename = folder_root.file_name().unwrap_or("project");
	let safe_basename = sanitise_basename(basename);
	let hash = fnv1a32_hex(raw);
	format!("{safe_basename}-{hash}")
}

/// Strip basename characters that would break a directory name on
/// the host filesystem (`/`, `\`, control chars). Everything else
/// passes through verbatim — sessions live on the user's own
/// machine and we don't need to be aggressive.
fn sanitise_basename(s: &str) -> String {
	let cleaned: String = s
		.chars()
		.map(|c| {
			if c.is_control() || c == '/' || c == '\\' {
				'_'
			} else {
				c
			}
		})
		.collect();
	if cleaned.is_empty() {
		"project".to_string()
	} else {
		cleaned
	}
}

/// FNV-1a 32-bit hex hash. Eight characters of disambiguation;
/// not cryptographic, but the only collision case we care about
/// is "two folders whose basename happens to match", and the
/// chance of a 32-bit collision there is well under one in a
/// hundred million.
fn fnv1a32_hex(s: &str) -> String {
	const FNV_OFFSET: u32 = 0x811c_9dc5;
	const FNV_PRIME: u32 = 0x0100_0193;
	let mut h: u32 = FNV_OFFSET;
	for b in s.bytes() {
		h ^= u32::from(b);
		h = h.wrapping_mul(FNV_PRIME);
	}
	format!("{h:08x}")
}

/// Generate a fresh session id. Prefixed with the local-date so
/// sorting by id roughly matches sorting by creation time, which
/// helps when staring at a `ls` of the sessions directory.
pub fn new_session_id() -> String {
	let ts = current_time_ms();
	let random: u32 = rand_suffix();
	format!("sess-{:013}-{:08x}", ts, random)
}

/// Truncate a freshly-sent prompt into a title. We keep the first
/// non-empty line, drop trailing whitespace, cap at ~60 chars on a
/// word boundary. Doesn't try hard — the auto-rename pass replaces
/// this within a few seconds.
///
/// The prompt may carry a trailing `<context>...</context>` block
/// produced by `Ctrl+L` attachments on the frontend (see
/// `renderPromptWithAttachments` in `coder.svelte.ts`). That block
/// is for the model only — it would surface as a literal
/// `<context>` title here when the user sent an attachment-only
/// message (empty prose + selections). Strip it before picking
/// the first line so the fallback title is either real prose or
/// empty (caller renders `(untitled)` for empty).
pub fn session_title_from_prompt(prompt: &str) -> String {
	const MAX_TITLE_CHARS: usize = 60;
	let cleaned = strip_trailing_context_block(prompt);
	let first_line = cleaned
		.lines()
		.find(|l| !l.trim().is_empty())
		.map(str::trim)
		.unwrap_or("");
	if first_line.is_empty() {
		return String::new();
	}
	if first_line.chars().count() <= MAX_TITLE_CHARS {
		return first_line.to_string();
	}
	let mut out = String::with_capacity(MAX_TITLE_CHARS);
	for ch in first_line.chars().take(MAX_TITLE_CHARS) {
		out.push(ch);
	}
	// Trim back to the previous space if we cut mid-word — looks
	// less ragged than `"… implem"`.
	if let Some(idx) = out.rfind(' ') {
		if idx > MAX_TITLE_CHARS / 2 {
			out.truncate(idx);
		}
	}
	out.push('…');
	out
}

/// Peel a trailing `<context>...</context>` block off `prompt`.
/// Permissive: any `<context>` followed (eventually) by
/// `</context>` at end-of-text counts. Returns the original
/// borrow when no such block exists, so callers don't pay an
/// allocation for the common no-attachment case.
fn strip_trailing_context_block(prompt: &str) -> &str {
	let trimmed = prompt.trim_end();
	if !trimmed.ends_with("</context>") {
		return prompt;
	}
	let Some(open_idx) = trimmed.rfind("<context>") else {
		return prompt;
	};
	&prompt[..open_idx]
}

/// List every **top-level** session in `dir`. Returns summaries
/// sorted by `updated_at_ms` descending — the most-recently-touched
/// session is the most-likely-wanted one when the panel mounts.
/// Missing directory yields an empty list, not an error: a fresh
/// workspace just has no sessions yet.
///
/// Sub-agent transcripts live in per-parent subdirectories
/// (`<dir>/<parent-session-id>/<sub-id>.jsonl`), so a flat read of
/// `dir` filtered to `*.jsonl` files naturally excludes them. The
/// subdirectories themselves don't have the `.jsonl` extension and
/// fall through the filter without a special prefix check. To
/// reach a sub-agent's transcript, use the pop-out card on the
/// parent's session or [`Runner::session_jsonl_path`] (which
/// scans the parent subdirs for a matching `sub-*` id).
pub async fn list_sessions(dir: &Utf8Path) -> Result<Vec<SessionSummary>, CoderError> {
	if !tokio::fs::try_exists(dir.as_std_path()).await.unwrap_or(false) {
		return Ok(Vec::new());
	}
	let mut read_dir = tokio::fs::read_dir(dir.as_std_path()).await.map_err(CoderError::from)?;
	let mut summaries: Vec<SessionSummary> = Vec::new();
	while let Some(entry) = read_dir.next_entry().await.map_err(CoderError::from)? {
		let path = entry.path();
		if path.extension().and_then(|s| s.to_str()) != Some(SESSION_EXT) {
			continue;
		}
		let utf8 = match Utf8PathBuf::from_path_buf(path) {
			Ok(p) => p,
			Err(_) => continue,
		};
		match load_summary(&utf8).await {
			Ok(summary) => summaries.push(summary),
			Err(err) => {
				tracing::warn!(error = %err, path = %utf8, "skipping unreadable session file");
			}
		}
	}
	summaries.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
	Ok(summaries)
}

/// Read the JSONL header line of a session file and project it onto
/// a [`SessionSummary`]. Also scans the rest of the file for
/// [`SessionRecord::TitleUpdate`] entries and folds the last one
/// into the returned title — without that pass, the auto-rename
/// (which only appends a `TitleUpdate` record rather than rewriting
/// the header in place) would never surface in the sessions list,
/// leaving the user staring at the truncated-prompt fallback even
/// after the rename pass finished and persisted a real title.
///
/// Full-file scan is acceptable here: session files are append-only
/// and small (typically a few hundred lines), and the list view
/// runs once per relaunch / once per `SessionListChanged` event.
/// We tolerate broken trailing lines (incomplete final record from
/// a crash mid-write) rather than refuse the whole summary.
pub async fn load_summary(path: &Utf8Path) -> Result<SessionSummary, CoderError> {
	let file = tokio::fs::File::open(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	let mut reader = BufReader::new(file);
	let mut header_line = String::new();
	reader.read_line(&mut header_line).await.map_err(CoderError::from)?;
	let mut header: SessionHeader = serde_json::from_str(header_line.trim_end()).map_err(|err| {
		CoderError::decode(
			path.as_str(),
			format!("could not parse session header: {err}; raw_len={}", header_line.len()),
		)
	})?;
	let mut line = String::new();
	loop {
		line.clear();
		let read = reader.read_line(&mut line).await.map_err(CoderError::from)?;
		if read == 0 {
			break;
		}
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		let Ok(record) = serde_json::from_str::<SessionRecord>(trimmed) else {
			continue;
		};
		if let SessionRecord::TitleUpdate { title } = record {
			header.title = title;
		}
	}
	Ok(SessionSummary {
		id: header.id,
		title: header.title,
		created_at_ms: header.created_at_ms,
		updated_at_ms: header.updated_at_ms,
	})
}

/// Full read: every JSONL line into [`SessionRecord`]s.
pub async fn load(dir: &Utf8Path, id: &str) -> Result<LoadedSession, CoderError> {
	let path = session_path(dir, id);
	let file = tokio::fs::File::open(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	let mut reader = BufReader::new(file);
	let mut header_line = String::new();
	reader.read_line(&mut header_line).await.map_err(CoderError::from)?;
	let mut header: SessionHeader = serde_json::from_str(header_line.trim_end())
		.map_err(|err| CoderError::decode(path.as_str(), format!("could not parse session header: {err}")))?;
	let mut records: Vec<SessionRecord> = Vec::new();
	let mut line = String::new();
	loop {
		line.clear();
		let n = reader.read_line(&mut line).await.map_err(CoderError::from)?;
		if n == 0 {
			break;
		}
		let trimmed = line.trim_end();
		if trimmed.is_empty() {
			continue;
		}
		match serde_json::from_str::<SessionRecord>(trimmed) {
			Ok(rec) => {
				if let SessionRecord::TitleUpdate { title } = &rec {
					header.title = title.clone();
				}
				records.push(rec);
			}
			Err(err) => {
				tracing::warn!(error = %err, path = %path, "skipping unreadable session record");
			}
		}
	}
	Ok(LoadedSession { header, records })
}

/// Append one record to a session's JSONL file. Creates the file
/// (and the parent directory) on the first call so callers don't
/// need to special-case "first write".
pub async fn append_record(dir: &Utf8Path, header: &SessionHeader, record: &SessionRecord) -> Result<(), CoderError> {
	let path = session_path(dir, &header.id);
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent.as_std_path())
			.await
			.map_err(CoderError::from)?;
	}
	let exists = tokio::fs::try_exists(path.as_std_path()).await.unwrap_or(false);
	let mut file = OpenOptions::new()
		.create(true)
		.append(true)
		.open(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	if !exists {
		let header_line = serde_json::to_string(header).map_err(CoderError::from)?;
		file.write_all(header_line.as_bytes()).await.map_err(CoderError::from)?;
		file.write_all(b"\n").await.map_err(CoderError::from)?;
	}
	let body_line = serde_json::to_string(record).map_err(CoderError::from)?;
	file.write_all(body_line.as_bytes()).await.map_err(CoderError::from)?;
	file.write_all(b"\n").await.map_err(CoderError::from)?;
	file.flush().await.map_err(CoderError::from)?;
	Ok(())
}

/// Delete a session file plus its sub-agent subdirectory (if any).
/// Idempotent — a missing file or subdir is not an error so the
/// UI's "delete then refresh" flow is well-defined even when two
/// windows race. The subdir cleanup is best-effort: a partial
/// failure logs at warn but doesn't block the JSONL deletion.
pub async fn delete(dir: &Utf8Path, id: &str) -> Result<(), CoderError> {
	let path = session_path(dir, id);
	match tokio::fs::remove_file(path.as_std_path()).await {
		Ok(()) => {}
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
		Err(err) => return Err(CoderError::from(err)),
	}
	let subagents = subagent_session_dir(dir, id);
	match tokio::fs::remove_dir_all(subagents.as_std_path()).await {
		Ok(()) => {}
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
		Err(err) => {
			tracing::warn!(
				error = %err,
				path = %subagents,
				"could not remove sub-agent subdirectory; leaving on disk",
			);
		}
	}
	Ok(())
}

pub fn session_path(dir: &Utf8Path, id: &str) -> Utf8PathBuf {
	dir.join(format!("{id}.{SESSION_EXT}"))
}

/// Find a sub-agent's JSONL by id, scanning the per-parent
/// subdirectories under `dir`. Returns `None` if `dir` doesn't
/// exist, no subdirectory contains a matching file, or `id`
/// doesn't have the `sub-` prefix (in which case the caller
/// should use [`session_path`] directly). Used by the
/// "open trace" affordance — the IPC takes a single id and
/// doesn't know the parent ahead of time. Cheap because the
/// scan is bounded by the number of parent sessions in the
/// project (a few dozen at most), and each subdir is checked
/// with a single `try_exists`.
pub async fn find_subagent_session(dir: &Utf8Path, id: &str) -> Option<Utf8PathBuf> {
	if !id.starts_with("sub-") {
		return None;
	}
	let mut read_dir = tokio::fs::read_dir(dir.as_std_path()).await.ok()?;
	while let Some(entry) = read_dir.next_entry().await.ok().flatten() {
		let path = entry.path();
		if !path.is_dir() {
			continue;
		}
		let utf8 = Utf8PathBuf::from_path_buf(path).ok()?;
		let candidate = session_path(&utf8, id);
		if tokio::fs::try_exists(candidate.as_std_path()).await.unwrap_or(false) {
			return Some(candidate);
		}
	}
	None
}

pub fn current_time_ms() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0)
}

fn rand_suffix() -> u32 {
	// Cheap "random enough" suffix — we only want collision-free
	// ids within one millisecond, and the timestamp prefix already
	// covers most of that. Uses the system-clock nanos as a
	// pseudo-random seed; xor-shift makes the trailing digits
	// move when calls land in the same millisecond.
	let nanos = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.subsec_nanos())
		.unwrap_or(0);
	let mut x = nanos ^ 0x9e37_79b1;
	x ^= x.wrapping_shl(13);
	x ^= x.wrapping_shr(17);
	x ^= x.wrapping_shl(5);
	x
}

/// Helper for parsing user-supplied ids. Doesn't validate the
/// random suffix — only rejects trivially malformed strings (`/`
/// in the id would let the caller climb out of the sessions dir).
pub fn validate_session_id(id: &str) -> Result<(), CoderError> {
	if id.is_empty() {
		return Err(CoderError::invalid_args("session id", "empty"));
	}
	if id.contains('/') || id.contains('\\') || id.contains("..") {
		return Err(CoderError::invalid_args(
			"session id",
			"must not contain path separators",
		));
	}
	Ok(())
}

impl FromStr for SessionRecord {
	type Err = serde_json::Error;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		serde_json::from_str(s)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn title_short_prompt_is_kept_verbatim() {
		let title = session_title_from_prompt("rename moon-agent → moon-remote");
		assert_eq!(title, "rename moon-agent → moon-remote");
	}

	#[test]
	fn title_long_prompt_truncates_on_word_boundary() {
		let prompt = "implement bucket sync with progress reporting and a status pip in the bottom right";
		let title = session_title_from_prompt(prompt);
		assert!(title.ends_with('…'));
		assert!(title.len() <= 70); // 60 chars + UTF-8 ellipsis padding
		assert!(!title.contains("…progress")); // shouldn't cut mid-word
	}

	#[test]
	fn title_strips_trailing_context_block() {
		// Prose plus an attachment block — title takes the prose line,
		// not `<context>` from the trailing wrapper.
		let prompt = "Refactor this for clarity.\n\n<context>\n<code_selection path=\"src/foo.ts\" lines=\"10-20\">\nbody\n</code_selection>\n</context>";
		let title = session_title_from_prompt(prompt);
		assert_eq!(title, "Refactor this for clarity.");
	}

	#[test]
	fn title_attachment_only_send_returns_empty() {
		// Empty prose, attachment-only send. With the context block
		// stripped there's nothing else; caller falls back to its
		// own "(untitled)" rendering and the auto-rename pass
		// produces a real title once the first turn finishes.
		let prompt = "<context>\n<code_selection path=\"src/foo.ts\" lines=\"10-20\">\nbody\n</code_selection>\n</context>";
		let title = session_title_from_prompt(prompt);
		assert_eq!(title, "");
	}

	#[test]
	fn title_keeps_inline_context_word() {
		// Only a *trailing* `</context>` triggers the stripper. A
		// user asking literal questions about the word "context"
		// keeps their prose intact.
		let title = session_title_from_prompt("What does <context> mean here?");
		assert_eq!(title, "What does <context> mean here?");
	}

	#[test]
	fn title_uses_first_non_blank_line() {
		let title = session_title_from_prompt("\n\n  do the thing  \n\nmore stuff");
		assert_eq!(title, "do the thing");
	}

	#[test]
	fn validate_session_id_rejects_path_traversal() {
		assert!(validate_session_id("../../etc/passwd").is_err());
		assert!(validate_session_id("a/b").is_err());
		assert!(validate_session_id("").is_err());
		assert!(validate_session_id("sess-12345-abcdef").is_ok());
	}

	#[tokio::test]
	async fn write_then_read_round_trip() {
		// Round-trip a session through the JSONL writer + reader
		// to make sure the schema survives serde defaults and
		// `skip_serializing_if` settings.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-test".into(),
			title: "round trip".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};
		append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "hi".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();
		append_record(
			&dir,
			&header,
			&SessionRecord::Assistant {
				content: Some("hey".into()),
				thinking: None,
				tool_calls: Vec::new(),
			},
		)
		.await
		.unwrap();
		append_record(
			&dir,
			&header,
			&SessionRecord::TitleUpdate {
				title: "renamed by auto-pass".into(),
			},
		)
		.await
		.unwrap();
		let loaded = load(&dir, "sess-test").await.unwrap();
		assert_eq!(loaded.header.title, "renamed by auto-pass");
		assert_eq!(loaded.records.len(), 3);

		// Regression: `load_summary` must surface the latest
		// `TitleUpdate` too. Before this fix it returned the
		// stale truncated-prompt title, and the sessions list
		// in the panel showed `<context>` / `@path:line-line …`
		// even after the auto-rename pass had persisted a real
		// title.
		let summary_path = session_path(&dir, "sess-test");
		let summary = load_summary(&summary_path).await.unwrap();
		assert_eq!(summary.title, "renamed by auto-pass");
		assert_eq!(summary.id, "sess-test");
	}

	#[tokio::test]
	async fn user_record_round_trips_with_attached_images() {
		// Regression guard for image attachments: a User record
		// with `images` must round-trip through JSONL → load
		// without losing the data URLs, otherwise pasted
		// screenshots would silently disappear on session
		// reload.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-img".into(),
			title: "img round trip".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};
		append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "look at this".into(),
				images: vec![crate::inference::ImageAttachment {
					data_url: "data:image/png;base64,AAAA".into(),
					mime: "image/png".into(),
				}],
			},
		)
		.await
		.unwrap();
		let loaded = load(&dir, "sess-img").await.unwrap();
		assert_eq!(loaded.records.len(), 1);
		match &loaded.records[0] {
			SessionRecord::User { text, images } => {
				assert_eq!(text, "look at this");
				assert_eq!(images.len(), 1);
				assert_eq!(images[0].data_url, "data:image/png;base64,AAAA");
				assert_eq!(images[0].mime, "image/png");
			}
			other => panic!("expected user record, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn user_record_without_images_omits_images_field() {
		// `skip_serializing_if = "Vec::is_empty"` on `images`
		// must keep no-image User records out of the JSONL.
		// Otherwise every user line gets a stray `"images":[]`,
		// which adds nothing and makes a `git log -p` of a
		// session transcript unreadable.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-noimg".into(),
			title: "no img".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};
		append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "hi".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-noimg").as_std_path())
			.await
			.unwrap();
		// First line is the header, second is the user record.
		let user_line = body.lines().nth(1).unwrap();
		assert!(
			!user_line.contains("\"images\""),
			"user line still serialised images field: {user_line}"
		);
	}

	#[tokio::test]
	async fn compaction_record_round_trips_via_jsonl() {
		// Auto-compaction writes a `Compaction` record so that
		// reopening the session reaches the same compacted
		// in-memory shape instead of re-inflating the full
		// pre-compaction transcript. Round-trip the record to
		// guard the serde shape — losing it silently would push
		// the next turn over the provider's context-length cap.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-compact".into(),
			title: "compaction round trip".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};
		append_record(
			&dir,
			&header,
			&SessionRecord::Compaction {
				summary: "earlier turns: refactored foo into bar".into(),
				messages_compacted: 42,
			},
		)
		.await
		.unwrap();
		let loaded = load(&dir, "sess-compact").await.unwrap();
		assert_eq!(loaded.records.len(), 1);
		match &loaded.records[0] {
			SessionRecord::Compaction {
				summary,
				messages_compacted,
			} => {
				assert_eq!(summary, "earlier turns: refactored foo into bar");
				assert_eq!(*messages_compacted, 42);
			}
			other => panic!("expected compaction record, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn list_sessions_skips_subagent_jsonl() {
		// Sub-agent transcripts live under per-parent subdirectories
		// (`<dir>/<parent-id>/<sub-id>.jsonl`). Listing the flat
		// directory should return only top-level sessions; the
		// subdirectory itself doesn't have the `.jsonl` extension
		// and falls through the filter.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

		let parent_header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-parent".into(),
			title: "parent".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};
		append_record(
			&dir,
			&parent_header,
			&SessionRecord::User {
				text: "hi".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();

		let sub_dir = subagent_session_dir(&dir, "sess-parent");
		let sub_header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sub-child".into(),
			title: "spawned by parent".into(),
			created_at_ms: 2,
			updated_at_ms: 2,
			model: "test/model".into(),
			parent_session_id: Some("sess-parent".into()),
			parent_tool_call_id: Some("call-1".into()),
			subagent_mode: Some("agent".into()),
			subagent_target_folder: None,
		};
		append_record(
			&sub_dir,
			&sub_header,
			&SessionRecord::User {
				text: "do thing".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();

		let listed = list_sessions(&dir).await.unwrap();
		assert_eq!(listed.len(), 1, "list should hide sub-agent transcripts");
		assert_eq!(listed[0].id, "sess-parent");

		// `find_subagent_session` resolves the sub-agent's path
		// for the IPC's "open trace" affordance — the IPC takes
		// a single id so the runner has to scan parent subdirs.
		let found = find_subagent_session(&dir, "sub-child").await;
		assert_eq!(found, Some(session_path(&sub_dir, "sub-child")));
		// Top-level ids return None — the caller should use the
		// flat `session_path` for those.
		let not_found = find_subagent_session(&dir, "sess-parent").await;
		assert_eq!(not_found, None);
	}

	#[tokio::test]
	async fn delete_session_removes_subagent_subdir() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

		let parent_header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-parent".into(),
			title: "parent".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};
		append_record(
			&dir,
			&parent_header,
			&SessionRecord::User {
				text: "hi".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();
		let sub_dir = subagent_session_dir(&dir, "sess-parent");
		let sub_header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sub-child".into(),
			title: "x".into(),
			created_at_ms: 2,
			updated_at_ms: 2,
			model: "test/model".into(),
			parent_session_id: Some("sess-parent".into()),
			parent_tool_call_id: Some("call-1".into()),
			subagent_mode: Some("agent".into()),
			subagent_target_folder: None,
		};
		append_record(
			&sub_dir,
			&sub_header,
			&SessionRecord::User {
				text: "x".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();

		delete(&dir, "sess-parent").await.unwrap();

		assert!(!tokio::fs::try_exists(session_path(&dir, "sess-parent").as_std_path())
			.await
			.unwrap());
		assert!(!tokio::fs::try_exists(sub_dir.as_std_path()).await.unwrap());
	}

	#[tokio::test]
	async fn usage_record_round_trips_with_optional_cache_fields() {
		// `Usage` records drive the post-replay context-usage
		// ring on `open_session`. Two shapes worth pinning:
		//
		// 1. A "no caching" usage (most providers): cache fields
		//    skip-serialise so the JSONL line stays slim.
		// 2. An Anthropic-via-OpenRouter usage: cache fields
		//    present and round-trip back exactly.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-usage".into(),
			title: "usage round trip".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "anthropic/claude-sonnet-4.5".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			subagent_target_folder: None,
		};

		// Plain non-caching usage line: cache fields are zero,
		// so they should be absent from the on-disk JSON.
		let plain = SessionRecord::Usage {
			prompt_tokens: 1234,
			completion_tokens: 56,
			total_tokens: 1290,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 0,
		};
		append_record(&dir, &header, &plain).await.unwrap();
		let raw = tokio::fs::read_to_string(session_path(&dir, "sess-usage").as_std_path())
			.await
			.unwrap();
		// Header line + one usage line. The usage line should
		// not carry the cache_* keys (skip-if-zero).
		let usage_line = raw.lines().nth(1).expect("usage line present");
		assert!(usage_line.contains(r#""kind":"usage""#));
		assert!(usage_line.contains(r#""prompt_tokens":1234"#));
		assert!(!usage_line.contains("cache_read_input_tokens"));
		assert!(!usage_line.contains("cache_creation_input_tokens"));

		// Now an Anthropic-via-OpenRouter usage where caching
		// kicked in. Both cache fields round-trip on disk.
		let with_cache = SessionRecord::Usage {
			prompt_tokens: 9000,
			completion_tokens: 200,
			total_tokens: 9200,
			cache_read_input_tokens: 7500,
			cache_creation_input_tokens: 600,
		};
		append_record(&dir, &header, &with_cache).await.unwrap();
		let loaded = load(&dir, "sess-usage").await.unwrap();
		assert_eq!(loaded.records.len(), 2);
		match &loaded.records[1] {
			SessionRecord::Usage {
				prompt_tokens,
				completion_tokens,
				total_tokens,
				cache_read_input_tokens,
				cache_creation_input_tokens,
			} => {
				assert_eq!(*prompt_tokens, 9000);
				assert_eq!(*completion_tokens, 200);
				assert_eq!(*total_tokens, 9200);
				assert_eq!(*cache_read_input_tokens, 7500);
				assert_eq!(*cache_creation_input_tokens, 600);
			}
			other => panic!("expected Usage, got {other:?}"),
		}

		// And the no-caching record we wrote first should
		// still parse back with default-zero cache fields,
		// proving the missing-on-disk → 0-in-memory fallback
		// works on reload.
		match &loaded.records[0] {
			SessionRecord::Usage {
				prompt_tokens,
				cache_read_input_tokens,
				cache_creation_input_tokens,
				..
			} => {
				assert_eq!(*prompt_tokens, 1234);
				assert_eq!(*cache_read_input_tokens, 0);
				assert_eq!(*cache_creation_input_tokens, 0);
			}
			other => panic!("expected Usage, got {other:?}"),
		}
	}

	#[test]
	fn project_slug_is_deterministic_and_disambiguates() {
		let a = Utf8PathBuf::from("/home/me/code/moon-ide");
		let b = Utf8PathBuf::from("/srv/projects/moon-ide");
		// Same path → same slug, every run.
		assert_eq!(project_slug(&a), project_slug(&a));
		// Different paths with the same basename → different
		// slugs (the FNV suffix disambiguates).
		assert_eq!(a.file_name(), b.file_name());
		assert_ne!(project_slug(&a), project_slug(&b));
	}
}
