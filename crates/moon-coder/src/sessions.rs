//! Session persistence for the coder agent.
//!
//! Each session is a JSONL file at
//! `<workspace folder>/.moon/agent-sessions/<id>.jsonl`. The first
//! line is a header carrying metadata (id, title, timestamps,
//! model); every subsequent line is one append-only
//! [`SessionRecord`] capturing user input, assistant output, or
//! tool I/O.
//!
//! Per-workspace placement (rather than a single global directory)
//! pairs a session with the codebase it's about — switching
//! folders surfaces only that folder's sessions, and a future
//! bucket-sync scheme can mirror the folder tree to
//! `<user>/moon-ide-sessions/<workspace-slug>/<id>.jsonl` without
//! a slugging dance.
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

/// Subdirectory under each workspace folder where sessions live.
const SESSIONS_DIRNAME: &str = ".moon/agent-sessions";

/// File extension on every session file.
const SESSION_EXT: &str = "jsonl";

/// JSONL header — first line of every session file. Must be the
/// first record because the [`load_summary`] fast path stops after
/// reading exactly one line.
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
}

/// One append-only record in the JSONL body. Tagged enum so each
/// line is self-describing — the loader doesn't need to track
/// state to decide what kind of record comes next.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRecord {
	/// One user prompt landed.
	User { text: String },
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

/// Resolve the per-folder sessions directory. Doesn't create it —
/// that happens lazily when the first session writes its header.
pub fn sessions_dir(folder_root: &Utf8Path) -> Utf8PathBuf {
	folder_root.join(SESSIONS_DIRNAME)
}

/// Generate a fresh session id. Prefixed with the local-date so
/// sorting by id roughly matches sorting by creation time, which
/// helps when staring at a `ls` of `.moon/agent-sessions/`.
pub fn new_session_id() -> String {
	let ts = current_time_ms();
	let random: u32 = rand_suffix();
	format!("sess-{:013}-{:08x}", ts, random)
}

/// Truncate a freshly-sent prompt into a title. We keep the first
/// non-empty line, drop trailing whitespace, cap at ~60 chars on a
/// word boundary. Doesn't try hard — the auto-rename pass replaces
/// this within a few seconds.
pub fn session_title_from_prompt(prompt: &str) -> String {
	const MAX_TITLE_CHARS: usize = 60;
	let first_line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or(prompt).trim();
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

/// List every session under `folder_root`'s sessions directory.
/// Returns summaries sorted by `updated_at_ms` descending — the
/// most-recently-touched session is the most-likely-wanted one
/// when the panel mounts. Missing directory yields an empty list,
/// not an error: a fresh workspace just has no sessions yet.
pub async fn list_sessions(folder_root: &Utf8Path) -> Result<Vec<SessionSummary>, CoderError> {
	let dir = sessions_dir(folder_root);
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

/// Read just the JSONL header line of a session file and project
/// it onto a [`SessionSummary`]. Cheap — one line only.
pub async fn load_summary(path: &Utf8Path) -> Result<SessionSummary, CoderError> {
	let file = tokio::fs::File::open(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	let mut reader = BufReader::new(file);
	let mut header_line = String::new();
	reader.read_line(&mut header_line).await.map_err(CoderError::from)?;
	let header: SessionHeader = serde_json::from_str(header_line.trim_end()).map_err(|err| {
		CoderError::decode(
			path.as_str(),
			format!("could not parse session header: {err}; raw_len={}", header_line.len()),
		)
	})?;
	Ok(SessionSummary {
		id: header.id,
		title: header.title,
		created_at_ms: header.created_at_ms,
		updated_at_ms: header.updated_at_ms,
	})
}

/// Full read: every JSONL line into [`SessionRecord`]s.
pub async fn load(folder_root: &Utf8Path, id: &str) -> Result<LoadedSession, CoderError> {
	let path = session_path(folder_root, id);
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
pub async fn append_record(
	folder_root: &Utf8Path,
	header: &SessionHeader,
	record: &SessionRecord,
) -> Result<(), CoderError> {
	let path = session_path(folder_root, &header.id);
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

/// Delete a session file. Idempotent — a missing file is not an
/// error so the UI's "delete then refresh" flow is well-defined
/// even when two windows race.
pub async fn delete(folder_root: &Utf8Path, id: &str) -> Result<(), CoderError> {
	let path = session_path(folder_root, id);
	match tokio::fs::remove_file(path.as_std_path()).await {
		Ok(()) => Ok(()),
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
		Err(err) => Err(CoderError::from(err)),
	}
}

fn session_path(folder_root: &Utf8Path, id: &str) -> Utf8PathBuf {
	let mut path = sessions_dir(folder_root);
	path.push(format!("{id}.{SESSION_EXT}"));
	path
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
		let dir = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let header = SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: "sess-test".into(),
			title: "round trip".into(),
			created_at_ms: 1,
			updated_at_ms: 1,
			model: "test/model".into(),
		};
		append_record(&root, &header, &SessionRecord::User { text: "hi".into() })
			.await
			.unwrap();
		append_record(
			&root,
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
			&root,
			&header,
			&SessionRecord::TitleUpdate {
				title: "renamed by auto-pass".into(),
			},
		)
		.await
		.unwrap();
		let loaded = load(&root, "sess-test").await.unwrap();
		assert_eq!(loaded.header.title, "renamed by auto-pass");
		assert_eq!(loaded.records.len(), 3);
	}
}
