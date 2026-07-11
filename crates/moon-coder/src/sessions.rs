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
///
/// `2` introduced the pi-mono compatible wire shape (single
/// `{type:"session", id, cwd, ...}` header + `{type:"message",
/// message:{role:...}}` envelopes per record). `3` picks up the
/// per-line / per-message field additions from pi's current
/// session format: a `timestamp` on every body line (ISO-8601 on
/// the entry envelope, Unix-ms inside each message), `toolName`
/// on tool-result messages, and `stopReason` on assistant
/// messages. We deliberately do **not** adopt pi's tree structure
/// (`id` / `parentId` linking) — Moon sessions are linear, with
/// no in-place branching, so the tree fields would be dead
/// weight. Moon-specific records (title updates, todo snapshots,
/// sub-agent metadata, standalone usage) still ride in pi
/// `custom` rows with no `content`, which the trace viewer
/// silently skips. See [`record_to_pi_wire`] / [`pi_wire_to_records`]
/// for the boundary.
///
/// `4` adds the optional `worktree_root` / `worktree_branch` header
/// fields for worktree-backed sessions (ADR 0028). They elide when
/// absent, so an ordinary session's header is byte-identical to a
/// schema-3 header apart from the version number. `5` adds the
/// optional `committed_branch` — the branch a session's work was
/// committed onto, so the panel can offer a one-click jump back to it
/// (ADR 0028). Also elides when absent.
/// Bumped to 6 for the top-level `mode` field (ADR 0030). The field
/// elides when `None` (the `Agent` default), so pre-6 sessions load
/// byte-compatible — `from_top_level_wire(None) == Agent`.
pub const SESSION_SCHEMA_VERSION: u32 = 6;

/// File extension on every session file.
const SESSION_EXT: &str = "jsonl";

/// JSONL header — first line of every session file. Must be the
/// first record because the [`load_summary`] fast path stops after
/// reading exactly one line.
///
/// On disk the header is rendered into the pi-mono wire shape:
/// `{"type":"session","version":3,"id":...,"timestamp":...,
/// "cwd":...,...}`. We keep the in-memory struct using the
/// existing field names (so call sites and tests don't churn);
/// the manual `Serialize` / `Deserialize` impls translate at the
/// boundary. `type` and `version` are pi-required keys; `cwd` is
/// what the pi harness in moon-landing keys off when sniffing the
/// header line in [detect.ts] (the trace viewer rejects sessions
/// without a string `cwd`). The rest are Moon-IDE-specific and
/// happily co-exist on the same row since pi's zod schemas don't
/// `.strict()` unknown keys.
///
/// Sub-agent sessions reuse this same struct with the optional
/// `parent_*` / `subagent_mode` fields populated. Top-level
/// (parent) sessions leave them `None`; the optional fields are
/// elided from JSON when absent so a sub-agent-free transcript
/// stays clean.
#[derive(Debug, Clone)]
pub struct SessionHeader {
	pub schema: u32,
	pub id: String,
	/// Absolute path of the workspace folder this session is
	/// bound to. Populated at first-persistence time (the runner
	/// sets it when `Coder::send` writes the first record). Pi's
	/// detector requires this to be a non-empty string; we
	/// satisfy that by binding before any append.
	pub cwd: String,
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
	pub parent_session_id: Option<String>,
	/// `tool_call_id` of the parent's `task` call that produced
	/// this sub-agent. Lets the UI's "pop out" affordance resolve
	/// the sub-agent's transcript across IDE restarts.
	pub parent_tool_call_id: Option<String>,
	/// Wire string ("research" / "agent") of the mode the
	/// sub-agent ran under. `None` for top-level sessions; mirrors
	/// `CoderMode::as_wire()` so the frontend reads it verbatim.
	pub subagent_mode: Option<String>,
	/// Wire string of the top-level session's mode (ADR 0030).
	/// `None` means the `Agent` default (byte-compatible with every
	/// pre-6 session); `Some("coordinator")` marks an orchestrator
	/// session. Sub-agents leave this `None` — their mode lives in
	/// `subagent_mode`. Read on load via `CoderMode::from_top_level_wire`
	/// and on each turn to pick the tool list + system prompt.
	pub mode: Option<String>,
	/// Absolute path of the folder the sub-agent's tools operated
	/// against. May differ from the parent's bound folder (which
	/// owns the JSONL on disk) when the parent passed an explicit
	/// `folder` argument to `task`. `None` for top-level sessions
	/// and for sub-agent sessions that targeted the same folder as
	/// their parent.
	pub subagent_target_folder: Option<String>,
	/// Per-session escape hatch for where the coder's `bash` /
	/// shell tools run. `None` is the default ("auto"): `bash`
	/// routes to the workspace shell container when it's running,
	/// else to the host — the historical behaviour. `Some(ForceHost)`
	/// pins this session's `bash` / shell tool to the host machine
	/// even while the workspace runs in a container, so an agent
	/// can inspect the host Docker daemon, host networking, etc.
	/// File tools (`read_file` / `edit_file`) are unaffected —
	/// they're already host-direct through the container bind mount.
	/// Format-on-save is also unaffected: it follows the global
	/// shell resolver and operates on the same bind-mounted bytes
	/// regardless, so the override deliberately doesn't relocate it
	/// (avoids a wider `WorkspaceHost::format_file` signature change
	/// for a diagnostic escape hatch). Per session, not per
	/// workspace:
	/// diagnosing host-side state is a property of one
	/// conversation, and concurrent sessions in the same folder can
	/// each pick independently. Persisted so re-opening a session
	/// restores the choice; a fresh session always starts `None`.
	pub bash_target_override: Option<BashTargetOverride>,
	/// Absolute path of the git worktree this session's tools run
	/// against (ADR 0028). `None` for an ordinary session, which
	/// drives its parent folder's main working tree. When set, the
	/// runner routes `cx.folder` to the worktree while the session
	/// stays filed under `cwd` (the parent folder) for persistence
	/// and the sessions list. Falls back to the parent if the
	/// worktree isn't bound (e.g. before startup re-binding lands).
	pub worktree_root: Option<String>,
	/// Branch the [`worktree_root`](Self::worktree_root) checkout is
	/// on. Informational — surfaced as a badge on the session row.
	/// `None` whenever `worktree_root` is.
	pub worktree_branch: Option<String>,
	/// Branch this (regular, main-tree) session's work was committed
	/// onto — set whenever the user commits with this session visible,
	/// to whatever branch `HEAD` lands on (a fresh "commit on new
	/// branch", or a plain commit on the current branch). Lets the
	/// panel offer a one-click `git switch` back to a past session's
	/// branch (ADR 0028). Most-recent-commit wins. `None` until the
	/// session's work is first committed; unused for worktree sessions
	/// (their branch is [`worktree_branch`](Self::worktree_branch)).
	pub committed_branch: Option<String>,
}

/// Per-session override for the `bash` tool's execution target.
///
/// Only one non-default variant today — `ForceHost`. We don't have
/// a `ForceContainer`: "auto" already prefers the container when
/// it's running, and forcing the container while it's down only
/// produces errors. If a concrete need for it shows up, add it
/// then. The wire string (`"host"`) is part of the session-header
/// JSON and the IPC payload — keep it in sync with
/// `src/lib/protocol.ts` and `tools::BASH_TARGET_HOST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashTargetOverride {
	/// Force `bash` / shell tools onto the host machine regardless
	/// of container state.
	ForceHost,
}

impl BashTargetOverride {
	/// Wire string used in the session header JSON and IPC.
	pub fn as_wire(self) -> &'static str {
		match self {
			BashTargetOverride::ForceHost => "host",
		}
	}

	/// Parse a wire string back into an override. Anything that
	/// isn't a recognised force-target (including `"auto"` and the
	/// empty string) maps to `None` = the auto default, so a stray
	/// or future value degrades to historical behaviour rather than
	/// erroring a session load.
	pub fn from_wire(s: &str) -> Option<Self> {
		match s {
			"host" => Some(BashTargetOverride::ForceHost),
			_ => None,
		}
	}
}

const PI_SESSION_TYPE: &str = "session";
const PI_MESSAGE_TYPE: &str = "message";
const PI_COMPACTION_TYPE: &str = "compaction";

const CUSTOM_TYPE_TITLE_UPDATE: &str = "moon_title_update";
const CUSTOM_TYPE_TODOS_UPDATE: &str = "moon_todos_update";
const CUSTOM_TYPE_SUBAGENT_SPAWNED: &str = "moon_subagent_spawned";
const CUSTOM_TYPE_SUBAGENT_FINISHED: &str = "moon_subagent_finished";
const CUSTOM_TYPE_USAGE: &str = "moon_usage";
const CUSTOM_TYPE_ERROR: &str = "moon_error";

impl Serialize for SessionHeader {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeMap;
		let mut map = serializer.serialize_map(None)?;
		map.serialize_entry("type", PI_SESSION_TYPE)?;
		map.serialize_entry("version", &self.schema)?;
		map.serialize_entry("id", &self.id)?;
		map.serialize_entry("timestamp", &iso8601_utc_ms(self.created_at_ms))?;
		map.serialize_entry("cwd", &self.cwd)?;
		map.serialize_entry("title", &self.title)?;
		map.serialize_entry("created_at_ms", &self.created_at_ms)?;
		map.serialize_entry("updated_at_ms", &self.updated_at_ms)?;
		map.serialize_entry("model", &self.model)?;
		if let Some(v) = &self.parent_session_id {
			map.serialize_entry("parent_session_id", v)?;
		}
		if let Some(v) = &self.parent_tool_call_id {
			map.serialize_entry("parent_tool_call_id", v)?;
		}
		if let Some(v) = &self.subagent_mode {
			map.serialize_entry("subagent_mode", v)?;
		}
		if let Some(v) = &self.mode {
			map.serialize_entry("mode", v)?;
		}
		if let Some(v) = &self.subagent_target_folder {
			map.serialize_entry("subagent_target_folder", v)?;
		}
		if let Some(v) = &self.bash_target_override {
			map.serialize_entry("bash_target_override", v.as_wire())?;
		}
		if let Some(v) = &self.worktree_root {
			map.serialize_entry("worktree_root", v)?;
		}
		if let Some(v) = &self.worktree_branch {
			map.serialize_entry("worktree_branch", v)?;
		}
		if let Some(v) = &self.committed_branch {
			map.serialize_entry("committed_branch", v)?;
		}
		map.end()
	}
}

impl<'de> Deserialize<'de> for SessionHeader {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		#[derive(Deserialize)]
		struct Raw {
			#[serde(rename = "type", default)]
			_kind: Option<String>,
			#[serde(default)]
			version: Option<u32>,
			#[serde(default)]
			schema: Option<u32>,
			id: String,
			#[serde(default)]
			cwd: Option<String>,
			#[serde(default)]
			title: String,
			#[serde(default)]
			created_at_ms: i64,
			#[serde(default)]
			updated_at_ms: i64,
			#[serde(default)]
			model: String,
			#[serde(default)]
			parent_session_id: Option<String>,
			#[serde(default)]
			parent_tool_call_id: Option<String>,
			#[serde(default)]
			subagent_mode: Option<String>,
			#[serde(default)]
			mode: Option<String>,
			#[serde(default)]
			subagent_target_folder: Option<String>,
			#[serde(default)]
			bash_target_override: Option<String>,
			#[serde(default)]
			worktree_root: Option<String>,
			#[serde(default)]
			worktree_branch: Option<String>,
			#[serde(default)]
			committed_branch: Option<String>,
		}
		let raw = Raw::deserialize(deserializer)?;
		Ok(SessionHeader {
			schema: raw.version.or(raw.schema).unwrap_or(SESSION_SCHEMA_VERSION),
			id: raw.id,
			cwd: raw.cwd.unwrap_or_default(),
			title: raw.title,
			created_at_ms: raw.created_at_ms,
			updated_at_ms: raw.updated_at_ms,
			model: raw.model,
			parent_session_id: raw.parent_session_id,
			parent_tool_call_id: raw.parent_tool_call_id,
			subagent_mode: raw.subagent_mode,
			mode: raw.mode,
			subagent_target_folder: raw.subagent_target_folder,
			bash_target_override: raw
				.bash_target_override
				.as_deref()
				.and_then(BashTargetOverride::from_wire),
			worktree_root: raw.worktree_root,
			worktree_branch: raw.worktree_branch,
			committed_branch: raw.committed_branch,
		})
	}
}

/// Format milliseconds-since-Unix-epoch as `YYYY-MM-DDTHH:MM:SS.sssZ`
/// (RFC 3339 / pi-friendly). Pure-stdlib (no `chrono` / `time`)
/// because moon-coder already pulls in enough crates; the algorithm
/// is Howard Hinnant's civil-from-days routine adapted to i64.
pub(crate) fn iso8601_utc_ms(ms: i64) -> String {
	let secs = ms.div_euclid(1000);
	let sub_ms = ms.rem_euclid(1000) as u32;
	let days = secs.div_euclid(86_400);
	let secs_of_day = secs.rem_euclid(86_400);
	let hour = (secs_of_day / 3600) as u32;
	let minute = ((secs_of_day % 3600) / 60) as u32;
	let second = (secs_of_day % 60) as u32;

	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = (z - era * 146_097) as u32;
	let yoe = (doe.saturating_sub(doe / 1460) + doe / 36_524 - doe / 146_096) / 365;
	let y = i64::from(yoe) + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let year = if m <= 2 { y + 1 } else { y };

	format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{sub_ms:03}Z")
}

/// Convert one [`SessionRecord`] to its pi-mono wire shape — the
/// JSON object we actually write to disk. The returned value is a
/// **single** JSONL row: either a pi `message` envelope, a
/// top-level `compaction` row, or a `custom` envelope wrapping a
/// Moon-specific record.
///
/// `Usage` records are folded onto the most recently appended
/// `assistant` line by [`try_fold_usage_into_last_assistant`] in
/// `append_record`; this function is only reached for a `Usage`
/// when there's no prior assistant on disk (a rare edge case —
/// e.g. a sub-agent that streams a `Usage` before its first
/// `Assistant` lands), in which case we emit a `moon_usage` custom
/// row so the data still round-trips on reload.
pub(crate) fn record_to_pi_wire(
	record: &SessionRecord,
	header: &SessionHeader,
	timestamp_ms: i64,
) -> serde_json::Value {
	match record {
		SessionRecord::User { text, images } => pi_message_envelope(pi_user_message(text, images), timestamp_ms),
		SessionRecord::Assistant {
			content,
			thinking,
			thinking_blocks,
			tool_calls,
			model,
			stop_reason,
		} => pi_message_envelope(
			pi_assistant_message(
				content.as_deref(),
				thinking.as_deref(),
				thinking_blocks,
				tool_calls,
				header,
				model.as_deref(),
				stop_reason.as_deref(),
			),
			timestamp_ms,
		),
		SessionRecord::Tool {
			tool_call_id,
			tool_name,
			content,
		} => pi_message_envelope(pi_tool_result_message(tool_call_id, tool_name, content), timestamp_ms),
		SessionRecord::Compaction {
			summary,
			messages_compacted,
			messages_kept,
		} => pi_compaction_row(summary, *messages_compacted, *messages_kept, timestamp_ms),
		SessionRecord::TitleUpdate { title } => pi_message_envelope(
			pi_custom_message(CUSTOM_TYPE_TITLE_UPDATE, serde_json::json!({ "title": title })),
			timestamp_ms,
		),
		SessionRecord::TodosUpdate { todos } => pi_message_envelope(
			pi_custom_message(CUSTOM_TYPE_TODOS_UPDATE, serde_json::json!({ "todos": todos })),
			timestamp_ms,
		),
		SessionRecord::SubagentSpawned {
			tool_call_id,
			subagent_id,
			target_folder,
			mode,
		} => pi_message_envelope(
			pi_custom_message(
				CUSTOM_TYPE_SUBAGENT_SPAWNED,
				serde_json::json!({
					"tool_call_id": tool_call_id,
					"subagent_id": subagent_id,
					"target_folder": target_folder,
					"mode": mode,
				}),
			),
			timestamp_ms,
		),
		SessionRecord::SubagentFinished {
			subagent_id,
			tokens_used_estimate,
			was_error,
			result_preview,
		} => pi_message_envelope(
			pi_custom_message(
				CUSTOM_TYPE_SUBAGENT_FINISHED,
				serde_json::json!({
					"subagent_id": subagent_id,
					"tokens_used_estimate": tokens_used_estimate,
					"was_error": was_error,
					"result_preview": result_preview,
				}),
			),
			timestamp_ms,
		),
		SessionRecord::Usage {
			prompt_tokens,
			completion_tokens,
			total_tokens,
			cache_read_input_tokens,
			cache_creation_input_tokens,
		} => pi_message_envelope(
			pi_custom_message(
				CUSTOM_TYPE_USAGE,
				pi_usage_details(
					*prompt_tokens,
					*completion_tokens,
					*total_tokens,
					*cache_read_input_tokens,
					*cache_creation_input_tokens,
				),
			),
			timestamp_ms,
		),
		SessionRecord::Error { message } => pi_message_envelope(
			pi_custom_message(CUSTOM_TYPE_ERROR, serde_json::json!({ "message": message })),
			timestamp_ms,
		),
	}
}

/// Wrap a pi `message` payload in its JSONL row envelope. Stamps
/// the row twice, mirroring pi's current format: an ISO-8601
/// `timestamp` on the envelope (pi's `SessionEntryBase.timestamp`)
/// and a Unix-millisecond `timestamp` inside the message object
/// (pi's per-message `timestamp`). Both come from the same instant
/// — the moment [`append_record`] flushes the row.
fn pi_message_envelope(mut inner: serde_json::Value, timestamp_ms: i64) -> serde_json::Value {
	if let Some(obj) = inner.as_object_mut() {
		obj.insert("timestamp".into(), serde_json::json!(timestamp_ms));
	}
	serde_json::json!({
		"type": PI_MESSAGE_TYPE,
		"timestamp": iso8601_utc_ms(timestamp_ms),
		"message": inner,
	})
}

fn pi_user_message(text: &str, images: &[crate::inference::ImageAttachment]) -> serde_json::Value {
	if images.is_empty() {
		return serde_json::json!({
			"role": "user",
			"content": text,
		});
	}
	let mut content: Vec<serde_json::Value> = Vec::with_capacity(images.len() + 1);
	if !text.is_empty() {
		content.push(serde_json::json!({ "type": "text", "text": text }));
	}
	for image in images {
		let (data, mime) = strip_data_url_prefix(&image.data_url, &image.mime);
		content.push(serde_json::json!({
			"type": "image",
			"data": data,
			"mimeType": mime,
		}));
	}
	serde_json::json!({
		"role": "user",
		"content": content,
	})
}

/// Strip a leading `data:<mime>;base64,` prefix off `data_url`.
/// Pi's `ImageContent` keeps the raw base64 in `data` and the
/// mime type in `mimeType`; the trace viewer reconstructs the
/// full data URL via [`getBase64ImageDataUrl`]. We preserve the
/// in-memory `data_url` exactly on round-trip by re-prefixing
/// during reload (see [`pi_wire_to_record`]).
fn strip_data_url_prefix<'a>(data_url: &'a str, mime: &'a str) -> (&'a str, &'a str) {
	if let Some(rest) = data_url.strip_prefix("data:") {
		if let Some(comma_idx) = rest.find(',') {
			let header_part = &rest[..comma_idx];
			let body = &rest[comma_idx + 1..];
			let header_mime = header_part.split(';').next().unwrap_or(mime);
			let mime_to_keep = if header_mime.is_empty() { mime } else { header_mime };
			return (body, mime_to_keep);
		}
	}
	(data_url, mime)
}

fn pi_assistant_message(
	content: Option<&str>,
	thinking: Option<&str>,
	thinking_blocks: &[crate::inference::ThinkingBlock],
	tool_calls: &[ToolCall],
	header: &SessionHeader,
	record_model: Option<&str>,
	stop_reason: Option<&str>,
) -> serde_json::Value {
	let mut blocks: Vec<serde_json::Value> = Vec::new();
	// Signed Anthropic reasoning blocks (when present) own the
	// thinking content: each one becomes a pi `thinking` /
	// `redacted_thinking` content block carrying its opaque
	// `signature` / `data` alongside the human-readable summary, so
	// the round-trip survives reload and we can replay it verbatim.
	// Otherwise fall back to the plain `thinking` summary string the
	// non-Anthropic providers produce.
	if thinking_blocks.is_empty() {
		if let Some(thinking) = thinking {
			if !thinking.is_empty() {
				blocks.push(serde_json::json!({
					"type": "thinking",
					"thinking": thinking,
				}));
			}
		}
	} else {
		for block in thinking_blocks {
			blocks.push(pi_thinking_block(block));
		}
	}
	if let Some(text) = content {
		if !text.is_empty() {
			blocks.push(serde_json::json!({
				"type": "text",
				"text": text,
			}));
		}
	}
	for call in tool_calls {
		blocks.push(pi_tool_call_block(call));
	}
	// Prefer the record's stamp (what actually served the round-
	// trip) over the session header's seed (set once at session
	// creation, never updated when the user flips providers).
	// Falling back to the header keeps historical sessions
	// rendering with their best-available guess.
	let model_source = record_model.unwrap_or(header.model.as_str());
	let (provider, model) = split_provider_model(model_source);
	let mut message = serde_json::Map::new();
	message.insert("role".into(), serde_json::Value::String("assistant".into()));
	message.insert("content".into(), serde_json::Value::Array(blocks));
	if let Some(p) = provider {
		message.insert("provider".into(), serde_json::Value::String(p.to_string()));
	}
	if !model.is_empty() {
		message.insert("model".into(), serde_json::Value::String(model.to_string()));
	}
	if let Some(reason) = stop_reason {
		if !reason.is_empty() {
			message.insert("stopReason".into(), serde_json::Value::String(reason.to_string()));
		}
	}
	serde_json::Value::Object(message)
}

/// Render one tool call as a pi `toolCall` content block. The
/// model's `arguments` field is a JSON-string on the wire (because
/// the OpenAI chat-completions schema serialises it that way);
/// pi-mono expects a parsed `arguments` object, so we parse here
/// and fall back to a single-key `{ "_raw": <string> }` on parse
/// failure (rare — a malformed `arguments` would already have
/// broken the dispatch loop anyway, but we'd rather round-trip
/// faithfully than panic).
/// Render one signed/redacted reasoning block as a pi content
/// block. The summary text rides in the standard `thinking` field
/// (so the pi viewer renders it); the opaque `signature` / `data`
/// ride in moon-specific sibling fields the viewer ignores but
/// [`parse_pi_thinking_block`] reads back on reload.
fn pi_thinking_block(block: &crate::inference::ThinkingBlock) -> serde_json::Value {
	use crate::inference::ThinkingBlock;
	match block {
		ThinkingBlock::Thinking { thinking, signature } => serde_json::json!({
			"type": "thinking",
			"thinking": thinking,
			"signature": signature,
		}),
		ThinkingBlock::RedactedThinking { data } => serde_json::json!({
			"type": "redacted_thinking",
			"data": data,
		}),
	}
}

/// Inverse of [`pi_thinking_block`]: reconstruct a signed/redacted
/// reasoning block from a pi `thinking` / `redacted_thinking`
/// content block. Returns `None` for a plain summary-only thinking
/// block (no `signature`) — those carry no replayable signature and
/// stay in the human-readable `thinking` string instead.
fn parse_pi_thinking_block(block: &serde_json::Value) -> Option<crate::inference::ThinkingBlock> {
	use crate::inference::ThinkingBlock;
	match block.get("type").and_then(|v| v.as_str()) {
		Some("redacted_thinking") => Some(ThinkingBlock::RedactedThinking {
			data: block
				.get("data")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
		}),
		Some("thinking") => {
			let signature = block.get("signature").and_then(|v| v.as_str())?;
			Some(ThinkingBlock::Thinking {
				thinking: block
					.get("thinking")
					.and_then(|v| v.as_str())
					.unwrap_or_default()
					.to_string(),
				signature: signature.to_string(),
			})
		}
		_ => None,
	}
}

fn pi_tool_call_block(call: &ToolCall) -> serde_json::Value {
	let args = match serde_json::from_str::<serde_json::Value>(&call.function.arguments) {
		Ok(v) => v,
		Err(_) => serde_json::json!({ "_raw": call.function.arguments }),
	};
	serde_json::json!({
		"type": "toolCall",
		"id": call.id,
		"name": call.function.name,
		"arguments": args,
	})
}

/// Split a `"provider/model"`-shaped header model string into its
/// `(Some("provider"), "model")` parts. Models without a slash
/// (custom / local) return `(None, full_string)`. Mirrors pi's
/// own provider rendering: `${provider}/${model}` in the trace
/// viewer's model label.
fn split_provider_model(model: &str) -> (Option<&str>, &str) {
	if let Some((provider, rest)) = model.split_once('/') {
		(Some(provider), rest)
	} else {
		(None, model)
	}
}

fn pi_tool_result_message(tool_call_id: &str, tool_name: &str, content: &str) -> serde_json::Value {
	let is_error = looks_like_tool_error(content);
	let mut message = serde_json::Map::new();
	message.insert("role".into(), serde_json::Value::String("toolResult".into()));
	message.insert("toolCallId".into(), serde_json::Value::String(tool_call_id.to_string()));
	if !tool_name.is_empty() {
		message.insert("toolName".into(), serde_json::Value::String(tool_name.to_string()));
	}
	message.insert(
		"content".into(),
		serde_json::json!([{ "type": "text", "text": content }]),
	);
	message.insert("isError".into(), serde_json::Value::Bool(is_error));
	serde_json::Value::Object(message)
}

/// Heuristic: did this tool result represent an error condition?
/// Currently true for our own interrupted-tool sentinel and for
/// any JSON object whose only key is `"error"` (the shape the
/// `bash` / `edit_file` tools use when they hard-fail). Mirrors
/// what the panel does to decide whether to paint the tool row
/// red, so the pi viewer's red badge matches our own.
fn looks_like_tool_error(content: &str) -> bool {
	if content == INTERRUPTED_TOOL_RESULT_JSON {
		return true;
	}
	let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(content) else {
		return false;
	};
	map.len() == 1 && map.contains_key("error")
}

fn pi_compaction_row(
	summary: &str,
	messages_compacted: u32,
	messages_kept: u32,
	timestamp_ms: i64,
) -> serde_json::Value {
	serde_json::json!({
		"type": PI_COMPACTION_TYPE,
		"timestamp": iso8601_utc_ms(timestamp_ms),
		"summary": summary,
		"details": { "messages_compacted": messages_compacted, "messages_kept": messages_kept },
	})
}

fn pi_custom_message(custom_type: &str, details: serde_json::Value) -> serde_json::Value {
	serde_json::json!({
		"role": "custom",
		"customType": custom_type,
		"display": false,
		"details": details,
	})
}

/// Usage as it rides folded onto an assistant message: keys
/// match the pi schema (`input` / `output` / `cacheRead` /
/// `cacheWrite` / `totalTokens`). Cache fields are omitted when
/// zero so non-caching providers still produce slim assistant
/// lines.
fn pi_usage_block(
	prompt_tokens: u32,
	completion_tokens: u32,
	total_tokens: u32,
	cache_read_input_tokens: u32,
	cache_creation_input_tokens: u32,
) -> serde_json::Value {
	let mut map = serde_json::Map::new();
	map.insert("input".into(), serde_json::json!(prompt_tokens));
	map.insert("output".into(), serde_json::json!(completion_tokens));
	map.insert("totalTokens".into(), serde_json::json!(total_tokens));
	if cache_read_input_tokens > 0 {
		map.insert("cacheRead".into(), serde_json::json!(cache_read_input_tokens));
	}
	if cache_creation_input_tokens > 0 {
		map.insert("cacheWrite".into(), serde_json::json!(cache_creation_input_tokens));
	}
	serde_json::Value::Object(map)
}

/// Usage as it rides on a stand-alone `custom` row (no prior
/// assistant to fold onto). Same numbers, identical key names so
/// `pi_wire_to_record` reads both shapes through one code path.
fn pi_usage_details(
	prompt_tokens: u32,
	completion_tokens: u32,
	total_tokens: u32,
	cache_read_input_tokens: u32,
	cache_creation_input_tokens: u32,
) -> serde_json::Value {
	pi_usage_block(
		prompt_tokens,
		completion_tokens,
		total_tokens,
		cache_read_input_tokens,
		cache_creation_input_tokens,
	)
}

/// Parse one pi-wire JSONL row back into `SessionRecord`s. A
/// single row maps to:
///
/// - `0` records: the pi header line, unknown row types, or pi
///   message roles we don't understand (e.g. `branchSummary`).
///   Returned as an empty `Vec` so the caller can `extend` past
///   the row without special-casing `None`.
/// - `1` record: the common case — `user`, `toolResult`,
///   `compaction`, and most `custom` rows.
/// - `2` records: an `assistant` row carrying a folded `usage`
///   block, which we re-split into the [`SessionRecord::Assistant`]
///   that wrote it plus the [`SessionRecord::Usage`] that rode
///   along. Restoring the split keeps every downstream consumer
///   (replay, context-usage ring) wired the same way it was in
///   schema 1, where Usage was always a stand-alone record.
pub(crate) fn pi_wire_to_records(value: &serde_json::Value) -> Vec<SessionRecord> {
	let Some(row_type) = value.get("type").and_then(|v| v.as_str()) else {
		return Vec::new();
	};
	if row_type == PI_COMPACTION_TYPE {
		let summary = value
			.get("summary")
			.and_then(|v| v.as_str())
			.unwrap_or_default()
			.to_string();
		let messages_compacted = value
			.get("details")
			.and_then(|d| d.get("messages_compacted"))
			.and_then(|v| v.as_u64())
			.unwrap_or(0) as u32;
		let messages_kept = value
			.get("details")
			.and_then(|d| d.get("messages_kept"))
			.and_then(|v| v.as_u64())
			.unwrap_or(0) as u32;
		return vec![SessionRecord::Compaction {
			summary,
			messages_compacted,
			messages_kept,
		}];
	}
	if row_type != PI_MESSAGE_TYPE {
		return Vec::new();
	}
	let Some(msg) = value.get("message") else {
		return Vec::new();
	};
	let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
	match role {
		"user" => parse_pi_user(msg).map(|r| vec![r]).unwrap_or_default(),
		"assistant" => parse_pi_assistant(msg),
		"toolResult" => parse_pi_tool_result(msg).map(|r| vec![r]).unwrap_or_default(),
		"custom" => parse_pi_custom(msg).map(|r| vec![r]).unwrap_or_default(),
		_ => Vec::new(),
	}
}

fn parse_pi_user(msg: &serde_json::Value) -> Option<SessionRecord> {
	let content = msg.get("content")?;
	if let Some(text) = content.as_str() {
		return Some(SessionRecord::User {
			text: text.to_string(),
			images: Vec::new(),
		});
	}
	let blocks = content.as_array()?;
	let mut texts: Vec<String> = Vec::new();
	let mut images: Vec<crate::inference::ImageAttachment> = Vec::new();
	for block in blocks {
		let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
		match block_type {
			"text" => {
				if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
					texts.push(t.to_string());
				}
			}
			"image" => {
				let data = block.get("data").and_then(|v| v.as_str()).unwrap_or("");
				let mime = block
					.get("mimeType")
					.and_then(|v| v.as_str())
					.unwrap_or("image/png")
					.to_string();
				images.push(crate::inference::ImageAttachment {
					data_url: format!("data:{mime};base64,{data}"),
					mime,
				});
			}
			_ => {}
		}
	}
	Some(SessionRecord::User {
		text: texts.join("\n"),
		images,
	})
}

fn parse_pi_assistant(msg: &serde_json::Value) -> Vec<SessionRecord> {
	let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) else {
		return Vec::new();
	};
	let mut texts: Vec<String> = Vec::new();
	let mut thinkings: Vec<String> = Vec::new();
	let mut thinking_blocks: Vec<crate::inference::ThinkingBlock> = Vec::new();
	let mut tool_calls: Vec<ToolCall> = Vec::new();
	for block in blocks {
		let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
		match block_type {
			"text" => {
				if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
					texts.push(t.to_string());
				}
			}
			"thinking" | "redacted_thinking" => {
				// A signed (or redacted) Anthropic block round-trips as
				// a replayable `ThinkingBlock`; a plain summary-only
				// block (no signature) just feeds the human-readable
				// `thinking` string.
				if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
					thinkings.push(t.to_string());
				}
				if let Some(parsed) = parse_pi_thinking_block(block) {
					thinking_blocks.push(parsed);
				}
			}
			"toolCall" => {
				let id = block.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
				let name = block
					.get("name")
					.and_then(|v| v.as_str())
					.unwrap_or_default()
					.to_string();
				let args_value = block
					.get("arguments")
					.cloned()
					.unwrap_or(serde_json::Value::Object(Default::default()));
				let arguments = if let Some(raw) = args_value.get("_raw").and_then(|v| v.as_str()) {
					raw.to_string()
				} else {
					serde_json::to_string(&args_value).unwrap_or_else(|_| "{}".into())
				};
				tool_calls.push(ToolCall {
					id,
					kind: "function".into(),
					function: crate::inference::FunctionCall { name, arguments },
				});
			}
			_ => {}
		}
	}
	let content = if texts.is_empty() { None } else { Some(texts.join("\n")) };
	let thinking = if thinkings.is_empty() {
		None
	} else {
		Some(thinkings.join("\n"))
	};
	// Empty-shell assistant rows — `{"role":"assistant","content":[]}`
	// with no thinking, no text, no tool calls — were written by
	// earlier versions whenever a provider bailed mid-stream or
	// streamed only a usage chunk. Re-inflating one into a
	// `ChatMessage::Assistant { content: None, tool_calls: [] }`
	// poisons the next prompt on Anthropic (`text content blocks
	// must contain non-whitespace text`) the moment the user
	// reopens the session and sends. Drop the record on load —
	// the surrounding `Usage` block (if any) still lands. The
	// runner now refuses to persist these in the first place, so
	// this only matters for sessions on disk from before the fix.
	let is_empty_shell = content.as_deref().map(|t| t.trim().is_empty()).unwrap_or(true)
		&& thinking.as_deref().map(|t| t.trim().is_empty()).unwrap_or(true)
		&& tool_calls.is_empty();
	let mut out: Vec<SessionRecord> = Vec::with_capacity(2);
	if !is_empty_shell {
		// Reconstruct the `provider/model` stamp from the pi-mono
		// `provider` + `model` fields, when both are present —
		// preserves the real route across reload so a later
		// re-persist (e.g. orphan recovery) doesn't downgrade
		// the record to the session header's seed.
		let provider = msg.get("provider").and_then(|v| v.as_str());
		let model_field = msg.get("model").and_then(|v| v.as_str());
		let model = match (provider, model_field) {
			(Some(p), Some(m)) => Some(format!("{p}/{m}")),
			(None, Some(m)) => Some(m.to_string()),
			_ => None,
		};
		let stop_reason = msg.get("stopReason").and_then(|v| v.as_str()).map(str::to_string);
		out.push(SessionRecord::Assistant {
			content,
			thinking,
			thinking_blocks,
			tool_calls,
			model,
			stop_reason,
		});
	}
	if let Some(usage) = msg.get("usage").and_then(parse_pi_usage_block) {
		out.push(usage);
	}
	out
}

fn parse_pi_tool_result(msg: &serde_json::Value) -> Option<SessionRecord> {
	let tool_call_id = msg.get("toolCallId").and_then(|v| v.as_str())?.to_string();
	let tool_name = msg
		.get("toolName")
		.and_then(|v| v.as_str())
		.unwrap_or_default()
		.to_string();
	let content = msg.get("content").and_then(|v| v.as_array());
	let body = match content {
		Some(blocks) => {
			let mut text_parts: Vec<String> = Vec::new();
			for block in blocks {
				if block.get("type").and_then(|v| v.as_str()) == Some("text") {
					if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
						text_parts.push(t.to_string());
					}
				}
			}
			text_parts.join("\n")
		}
		None => String::new(),
	};
	Some(SessionRecord::Tool {
		tool_call_id,
		tool_name,
		content: body,
	})
}

fn parse_pi_custom(msg: &serde_json::Value) -> Option<SessionRecord> {
	let custom_type = msg.get("customType").and_then(|v| v.as_str())?;
	let details = msg.get("details").cloned().unwrap_or(serde_json::Value::Null);
	match custom_type {
		CUSTOM_TYPE_TITLE_UPDATE => {
			let title = details
				.get("title")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string();
			Some(SessionRecord::TitleUpdate { title })
		}
		CUSTOM_TYPE_TODOS_UPDATE => {
			let todos: Vec<crate::TodoItem> = serde_json::from_value(
				details
					.get("todos")
					.cloned()
					.unwrap_or(serde_json::Value::Array(Vec::new())),
			)
			.unwrap_or_default();
			Some(SessionRecord::TodosUpdate { todos })
		}
		CUSTOM_TYPE_SUBAGENT_SPAWNED => Some(SessionRecord::SubagentSpawned {
			tool_call_id: details
				.get("tool_call_id")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
			subagent_id: details
				.get("subagent_id")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
			target_folder: details
				.get("target_folder")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
			mode: details
				.get("mode")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
		}),
		CUSTOM_TYPE_SUBAGENT_FINISHED => Some(SessionRecord::SubagentFinished {
			subagent_id: details
				.get("subagent_id")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
			tokens_used_estimate: details
				.get("tokens_used_estimate")
				.and_then(|v| v.as_u64())
				.unwrap_or(0) as u32,
			was_error: details.get("was_error").and_then(|v| v.as_bool()).unwrap_or(false),
			result_preview: details
				.get("result_preview")
				.and_then(|v| v.as_str())
				.map(str::to_string),
		}),
		CUSTOM_TYPE_USAGE => parse_pi_usage_block(&details),
		CUSTOM_TYPE_ERROR => Some(SessionRecord::Error {
			message: details
				.get("message")
				.and_then(|v| v.as_str())
				.unwrap_or_default()
				.to_string(),
		}),
		_ => None,
	}
}

fn parse_pi_usage_block(value: &serde_json::Value) -> Option<SessionRecord> {
	let prompt_tokens = value.get("input").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
	let completion_tokens = value.get("output").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
	let total_tokens = value
		.get("totalTokens")
		.and_then(|v| v.as_u64())
		.unwrap_or_else(|| u64::from(prompt_tokens + completion_tokens)) as u32;
	let cache_read_input_tokens = value.get("cacheRead").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
	let cache_creation_input_tokens = value.get("cacheWrite").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
	Some(SessionRecord::Usage {
		prompt_tokens,
		completion_tokens,
		total_tokens,
		cache_read_input_tokens,
		cache_creation_input_tokens,
	})
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
		/// Signed/redacted Anthropic reasoning blocks, preserved so a
		/// reopened session can replay them on the next tool round-trip
		/// (the Messages API rejects a tool turn whose preceding
		/// assistant message dropped its thinking block). Empty for
		/// every non-Anthropic provider. See
		/// [`crate::inference::ThinkingBlock`].
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		thinking_blocks: Vec<crate::inference::ThinkingBlock>,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		tool_calls: Vec<ToolCall>,
		/// Provider/model slug that actually served this round-
		/// trip, in `provider/model` form (e.g. `anthropic/claude-
		/// sonnet-4.5`). Stamped on the pi-mono assistant row in
		/// place of the session header's seed model so the trace
		/// viewer shows the real route per turn — the header's
		/// `model` is only the **session creation** seed and stays
		/// frozen even when the user flips providers mid-session.
		///
		/// `None` only for sessions on disk from before this
		/// field shipped, in which case `pi_assistant_message`
		/// falls back to `header.model` (the old behaviour). The
		/// in-memory runner always populates it.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		model: Option<String>,
		/// Provider-reported stop reason, normalised to pi's
		/// `stopReason` vocabulary (`stop` | `length` | `toolUse` |
		/// `error` | `aborted`) by
		/// [`crate::inference::normalize_stop_reason`]. Stamped on
		/// the pi assistant row so the trace viewer can label why
		/// the turn ended. `None` only for records on disk from
		/// before this field shipped; the runner always populates
		/// it.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		stop_reason: Option<String>,
	},
	/// One tool result fed back to the model. `tool_call_id`
	/// matches the `id` on the parent [`SessionRecord::Assistant`]
	/// record's `tool_calls`. `tool_name` is the function name of
	/// that call, persisted so the pi `toolResult` row carries a
	/// `toolName` (the trace viewer labels results by it) without
	/// a second lookup against the assistant record. Empty only
	/// for synthetic interrupted-tool sentinels and records on
	/// disk from before this field shipped.
	Tool {
		tool_call_id: String,
		#[serde(default)]
		tool_name: String,
		content: String,
	},
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
	/// One auto-compaction pass landed. The runtime folds the
	/// older message prefix into a synthetic system message
	/// holding `summary`; this record is the on-disk twin so
	/// replay reaches the same in-memory shape. Without it,
	/// reopening a long session re-inflates the full
	/// pre-compaction transcript and the next turn instantly
	/// trips the provider's context-length cap.
	///
	/// `messages_compacted` is how many messages the live pass
	/// folded (`messages[1..cutoff]`). `messages_kept` is how
	/// many trailing messages rode through unchanged (the recent
	/// user turns and their replies, `messages[cutoff..]`).
	/// Replay needs `messages_kept`: it rebuilds the full
	/// transcript linearly, then folds everything *except* the
	/// last `messages_kept` messages, which reproduces the live
	/// cutoff exactly. Without it replay would drain the whole
	/// prefix and silently drop the recent turns the live pass
	/// deliberately retained.
	Compaction {
		summary: String,
		messages_compacted: u32,
		messages_kept: u32,
	},
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
	/// One sub-agent was spawned by this session. Mirrors
	/// [`crate::CoderEvent::SubagentSpawned`] one-to-one so replay
	/// can re-emit the same event without re-deriving anything.
	///
	/// Without this record, reopening a session that had spawned
	/// sub-agents leaves the parent transcript with bare `task`
	/// tool rows and no inline summary card — the user can't get
	/// to the sub-agent's transcript at all because the panel
	/// keys the pop-out off `subagentSummaries`, which only ever
	/// got populated from the live event stream.
	SubagentSpawned {
		tool_call_id: String,
		subagent_id: String,
		target_folder: String,
		mode: String,
	},
	/// One sub-agent finished (success or error). Mirrors
	/// [`crate::CoderEvent::SubagentFinished`] plus a
	/// `result_preview` field so the collapsed card on the
	/// reloaded parent transcript can show the sub-agent's final
	/// answer without us also having to lazy-load the sub-agent's
	/// own JSONL just to read its last assistant message.
	SubagentFinished {
		subagent_id: String,
		tokens_used_estimate: u32,
		was_error: bool,
		/// First non-empty trimmed assistant message from the
		/// sub-agent's transcript. `Some(...)` for clean exits;
		/// `None` for errors / abort with no produced text. Two-
		/// line cap is enforced by the panel CSS — we persist the
		/// full preview string here so a future "show full
		/// preview" UI doesn't need a re-derivation pass.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		result_preview: Option<String>,
	},
	/// A turn failed with a non-recoverable backend error — auth
	/// gone bad mid-stream, a decode error from the router, a 400
	/// from an unpaired / malformed tool call, etc. Persisted so
	/// the error survives a session reopen: without it, the on-
	/// disk transcript ends at the last successful record and the
	/// failure is invisible to anyone debugging from the JSONL
	/// later (the UI toast vanished the moment the panel closed or
	/// reloaded). The record carries the provider's error string
	/// verbatim; replay re-emits a [`crate::CoderEvent::Error`]
	/// so the reopened transcript shows the failure inline.
	///
	/// The record rides in a pi `custom` row (`display:false`,
	/// `moon_error`) alongside the other moon-specific records.
	/// It does **not** shape the in-memory `messages` slice on
	/// reload: an error is terminal for the turn it ended, not
	/// part of the chat history the next turn sends to the model.
	Error { message: String },
}

fn u32_is_zero(n: &u32) -> bool {
	*n == 0
}

/// JSON content used as the synthetic `Tool` result when replay
/// detects an interrupted tool call. Kept small + parseable so
/// the panel renders a clean error row and the model — if the
/// user resumes the session — sees an unambiguous "this tool
/// didn't run" signal it can react to instead of a hung promise.
///
/// Both `open_session` (top-level transcript) and
/// `replay_subagent_spawned` (sub-agent transcripts) rely on
/// this string; the panel detects "looks like an error" via the
/// `{"error": "…"}`-only-key shape (see `emit_replay_events`).
pub const INTERRUPTED_TOOL_RESULT_JSON: &str = r#"{"error":"Interrupted before tool completed."}"#;

/// Walk `records` and return the tool-call ids that an Assistant
/// record emitted but no later `Tool` record acknowledged.
///
/// In the wild, this happens when the user stops the coder mid-
/// tool (Ctrl+C, panel close, IDE quit) — the Assistant record
/// hits disk before the tool dispatcher returns, but the
/// matching `Tool` record never gets appended. Without recovery,
/// the panel re-renders the row as "running" forever and the
/// model rejects the next turn (most providers strict-validate
/// "every assistant tool_call has a matching tool message").
///
/// Returned in iteration order so callers can preserve "tool
/// results follow their assistant message" when threading the
/// orphans back into a rebuilt `messages` slice. Cheap: O(N) one
/// pass with a small `HashSet`.
pub fn orphan_tool_call_ids(records: &[SessionRecord]) -> Vec<String> {
	let mut completed: std::collections::HashSet<&str> = std::collections::HashSet::new();
	for record in records {
		if let SessionRecord::Tool { tool_call_id, .. } = record {
			completed.insert(tool_call_id.as_str());
		}
	}
	let mut orphans: Vec<String> = Vec::new();
	for record in records {
		if let SessionRecord::Assistant { tool_calls, .. } = record {
			for call in tool_calls {
				if !completed.contains(call.id.as_str()) {
					orphans.push(call.id.clone());
				}
			}
		}
	}
	orphans
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
	/// Branch of the git worktree this session runs in, when it's a
	/// worktree-backed (isolated) session (ADR 0028). `None` for an
	/// ordinary session. Lets the sessions list badge the row.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub worktree_branch: Option<String>,
	/// Branch this session's work was committed onto (ADR 0028), for
	/// the session list's one-click "switch back to this branch"
	/// chip. `None` until the session's work is committed.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub committed_branch: Option<String>,
	/// Top-level session mode (ADR 0030) — `None` for the `Agent`
	/// default, `Some("coordinator")` for an orchestrator session.
	/// Lets the sessions list badge a coordinator row. Mirrors the
	/// header's `mode` field (which elides the same way).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub mode: Option<String>,
}

/// Full load: header + every record in order. Used when the user
/// opens a session and we need to replay events into the panel
/// + reconstruct the chat history for the runner.
pub struct LoadedSession {
	pub header: SessionHeader,
	pub records: Vec<SessionRecord>,
	/// Per-record creation time in Unix-ms, aligned 1:1 with
	/// `records` (a single JSONL line that fans out to two records
	/// — an assistant row carrying a folded `usage` block — stamps
	/// both with the line's timestamp). Read off the persisted
	/// `timestamp` field; falls back to the header's creation time
	/// for pre-timestamp (schema < 3) lines. Used by the replay
	/// path to stamp the historical `created_at_ms` onto the
	/// reconstructed `UserMessage` / `AssistantMessageEnd` events so
	/// a reopened session shows real per-message times.
	pub record_timestamps: Vec<i64>,
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
	summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_at_ms));
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
/// `updated_at_ms` comes from the file's **mtime**, not the header.
/// The transcript is append-only and the header is written once at
/// creation, so the header's own `updated_at_ms` is frozen at
/// first-persistence time — a session that received follow-up
/// messages would sort back to its creation slot on reopen if we
/// trusted it. mtime is "last write to the file", which is exactly
/// the activity signal we want, and it's free: every `append_record`
/// touches it. We keep the header field for the in-process live
/// bump + re-sort (`Coder::send` / the frontend), but on a
/// cold read from disk mtime is authoritative. Fall back to the
/// header value if the mtime can't be converted (pre-1970 / clock
/// skew shouldn't happen, but we don't want to crash a list-load).
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
	let mtime_ms = file.metadata().await.ok().as_ref().and_then(file_mtime_ms);
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
		let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
			continue;
		};
		for record in pi_wire_to_records(&value) {
			if let SessionRecord::TitleUpdate { title } = record {
				header.title = title;
			}
		}
	}
	Ok(SessionSummary {
		id: header.id,
		title: header.title,
		created_at_ms: header.created_at_ms,
		updated_at_ms: mtime_ms.unwrap_or(header.updated_at_ms),
		worktree_branch: header.worktree_branch,
		committed_branch: header.committed_branch,
		mode: header.mode,
	})
}

/// File mtime as unix milliseconds. The on-disk authority for a
/// session's "last activity" — the header's own `updated_at_ms` is
/// frozen at creation (transcript is append-only, header written
/// once), so reopen ordering and the restored header both key off
/// this instead. `None` only on a missing file or a pre-epoch /
/// clock-skew mtime we can't represent, in which case callers fall
/// back to the frozen header value.
fn file_mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
	meta
		.modified()
		.ok()
		.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
		.and_then(|d| i64::try_from(d.as_millis()).ok())
}

/// Full read: every JSONL line into [`SessionRecord`]s.
pub async fn load(dir: &Utf8Path, id: &str) -> Result<LoadedSession, CoderError> {
	let path = session_path(dir, id);
	let file = tokio::fs::File::open(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	let mtime_ms = file.metadata().await.ok().as_ref().and_then(file_mtime_ms);
	let mut reader = BufReader::new(file);
	let mut header_line = String::new();
	reader.read_line(&mut header_line).await.map_err(CoderError::from)?;
	let mut header: SessionHeader = serde_json::from_str(header_line.trim_end())
		.map_err(|err| CoderError::decode(path.as_str(), format!("could not parse session header: {err}")))?;
	// Override the frozen header timestamp with the file's mtime so
	// the restored in-memory session + the `session_loaded` event
	// agree with the sessions-list ordering (also mtime-derived).
	if let Some(mtime_ms) = mtime_ms {
		header.updated_at_ms = mtime_ms;
	}
	let mut records: Vec<SessionRecord> = Vec::new();
	let mut record_timestamps: Vec<i64> = Vec::new();
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
		match serde_json::from_str::<serde_json::Value>(trimmed) {
			Ok(value) => {
				let line_ts = wire_line_timestamp_ms(&value).unwrap_or(header.created_at_ms);
				for rec in pi_wire_to_records(&value) {
					if let SessionRecord::TitleUpdate { title } = &rec {
						header.title = title.clone();
					}
					records.push(rec);
					record_timestamps.push(line_ts);
				}
			}
			Err(err) => {
				tracing::warn!(error = %err, path = %path, "skipping unreadable session record");
			}
		}
	}
	Ok(LoadedSession {
		header,
		records,
		record_timestamps,
	})
}

/// Pull the creation timestamp (Unix-ms) off one pi-wire JSONL
/// row, reading the per-message `timestamp` (the number we write on
/// every message). Returns `None` for rows without one — compaction
/// rows (no `message`, and they don't surface a per-row time in the
/// UI) and pre-timestamp (schema < 3) lines — so the caller falls
/// back to the header's creation time.
fn wire_line_timestamp_ms(value: &serde_json::Value) -> Option<i64> {
	value
		.get("message")
		.and_then(|m| m.get("timestamp"))
		.and_then(|v| v.as_i64())
}

/// Append one record to a session's JSONL file. Creates the file
/// (and the parent directory) on the first call so callers don't
/// need to special-case "first write".
///
/// `Usage` records get special-cased: if the previous appended
/// line on disk is an `assistant` pi-message, we rewrite that
/// line in place to fold the usage block onto it (matching
/// pi-mono's on-disk shape). If no prior assistant exists (rare
/// — only seen when a sub-agent emits Usage before its first
/// Assistant), we fall back to a stand-alone `moon_usage` custom
/// row so the data still round-trips on reload.
/// Milliseconds since the Unix epoch, clamped at 0 if the clock
/// is somehow before 1970. Used to stamp every appended record.
fn now_ms() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0)
}

pub async fn append_record(dir: &Utf8Path, header: &SessionHeader, record: &SessionRecord) -> Result<(), CoderError> {
	let path = session_path(dir, &header.id);
	if let Some(parent) = path.parent() {
		tokio::fs::create_dir_all(parent.as_std_path())
			.await
			.map_err(CoderError::from)?;
	}
	let exists = tokio::fs::try_exists(path.as_std_path()).await.unwrap_or(false);

	if exists
		&& matches!(record, SessionRecord::Usage { .. })
		&& try_fold_usage_into_last_assistant(&path, record).await?
	{
		return Ok(());
	}

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
	let wire = record_to_pi_wire(record, header, now_ms());
	let body_line = serde_json::to_string(&wire).map_err(CoderError::from)?;
	file.write_all(body_line.as_bytes()).await.map_err(CoderError::from)?;
	file.write_all(b"\n").await.map_err(CoderError::from)?;
	file.flush().await.map_err(CoderError::from)?;
	Ok(())
}

/// Rewrite the header (first line) of an already-persisted
/// session JSONL in place. No-op (returns `Ok(())`) when the file
/// doesn't exist yet — a not-yet-persisted session carries the
/// header in memory and writes it on its first
/// [`append_record`], so there's nothing on disk to fix.
///
/// Used by the per-session bash-target override toggle: flipping
/// it mutates the in-memory header, and this keeps the on-disk
/// copy truthful so a reload restores the choice. Reads the file
/// fully (session JSONLs are small), swaps line 1 for the
/// re-serialised header, and writes the whole thing back. The
/// body lines are byte-preserved.
pub async fn rewrite_header(dir: &Utf8Path, header: &SessionHeader) -> Result<(), CoderError> {
	let path = session_path(dir, &header.id);
	if !tokio::fs::try_exists(path.as_std_path()).await.unwrap_or(false) {
		return Ok(());
	}
	let content = tokio::fs::read_to_string(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	let header_line = serde_json::to_string(header).map_err(CoderError::from)?;
	let mut out = String::with_capacity(content.len() + header_line.len());
	out.push_str(&header_line);
	// Re-attach every line after the first verbatim (including the
	// trailing newline shape of the original).
	if let Some(rest_start) = content.find('\n') {
		out.push_str(&content[rest_start..]);
	} else {
		// Degenerate: file held only a header with no newline.
		out.push('\n');
	}
	tokio::fs::write(path.as_std_path(), out.as_bytes())
		.await
		.map_err(CoderError::from)?;
	Ok(())
}

/// Try to fold a `Usage` record into the last appended assistant
/// line on disk. Returns `Ok(true)` when the fold landed and the
/// caller should skip writing a stand-alone row; `Ok(false)`
/// when the last line wasn't an assistant message (no fold
/// possible — caller falls back to the regular append path).
///
/// Reads the file fully (session JSONLs are small, a few KB to
/// a few hundred KB), finds the last line, parses it, mutates
/// the embedded assistant message to add `usage`, then truncates
/// the file back to the line start and rewrites just that line.
/// The truncate-and-rewrite avoids the in-place line-length
/// problem that would otherwise require shifting bytes around.
async fn try_fold_usage_into_last_assistant(path: &Utf8Path, usage: &SessionRecord) -> Result<bool, CoderError> {
	let SessionRecord::Usage {
		prompt_tokens,
		completion_tokens,
		total_tokens,
		cache_read_input_tokens,
		cache_creation_input_tokens,
	} = usage
	else {
		return Ok(false);
	};

	let content = tokio::fs::read_to_string(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	let bytes = content.as_bytes();
	if bytes.is_empty() {
		return Ok(false);
	}
	let trailing_newline = bytes.ends_with(b"\n");
	let end = if trailing_newline { bytes.len() - 1 } else { bytes.len() };
	if end == 0 {
		return Ok(false);
	}
	let start = bytes[..end]
		.iter()
		.rposition(|&b| b == b'\n')
		.map(|p| p + 1)
		.unwrap_or(0);
	let last_line = &content[start..end];

	let mut parsed: serde_json::Value = match serde_json::from_str(last_line) {
		Ok(v) => v,
		Err(_) => return Ok(false),
	};
	if parsed.get("type").and_then(|v| v.as_str()) != Some(PI_MESSAGE_TYPE) {
		return Ok(false);
	}
	let Some(message) = parsed.get_mut("message").and_then(|v| v.as_object_mut()) else {
		return Ok(false);
	};
	if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
		return Ok(false);
	}
	if message.contains_key("usage") {
		return Ok(false);
	}

	message.insert(
		"usage".into(),
		pi_usage_block(
			*prompt_tokens,
			*completion_tokens,
			*total_tokens,
			*cache_read_input_tokens,
			*cache_creation_input_tokens,
		),
	);

	let new_line = serde_json::to_string(&parsed).map_err(CoderError::from)?;
	let mut file = OpenOptions::new()
		.write(true)
		.open(path.as_std_path())
		.await
		.map_err(CoderError::from)?;
	use tokio::io::AsyncSeekExt;
	file.set_len(start as u64).await.map_err(CoderError::from)?;
	file
		.seek(std::io::SeekFrom::Start(start as u64))
		.await
		.map_err(CoderError::from)?;
	file.write_all(new_line.as_bytes()).await.map_err(CoderError::from)?;
	file.write_all(b"\n").await.map_err(CoderError::from)?;
	file.flush().await.map_err(CoderError::from)?;
	Ok(true)
}

/// Rewrite a session's JSONL to drop the `user_ordinal`-th user
/// prompt and everything after it, powering the panel's "revert
/// to this message" / "edit and resend" affordances.
///
/// `user_ordinal` is 0-based over [`SessionRecord::User`] records
/// in transcript order — the same order the panel renders its
/// `user` rows, so the Nth user bubble maps to the Nth user
/// record without needing a stable per-message id on disk (the
/// runner mints those fresh on every replay). Steers count as
/// user records, matching how they render.
///
/// Returns the dropped user prompt (`text` + `images`) so the
/// caller can prefill the composer for an edit-and-resend, plus
/// the surviving records so the runner can rebuild its in-memory
/// `messages` without a second disk read. Errors with
/// [`CoderError::Internal`] when `user_ordinal` is out of range
/// (no such user record) or the file doesn't exist.
///
/// The file is rewritten from the header plus the surviving
/// records re-serialised one line each. Folded `usage` blocks
/// come back off [`load`] as stand-alone [`SessionRecord::Usage`]
/// records, so re-serialising them stand-alone round-trips
/// cleanly through the next [`load`].
#[derive(Debug)]
pub struct RevertResult {
	pub dropped_text: String,
	pub dropped_images: Vec<crate::inference::ImageAttachment>,
	pub surviving: Vec<SessionRecord>,
}

/// Result of [`truncate_before_assistant_record`]: the kept
/// `Assistant`'s `tool_calls` ready for re-dispatch. The runner
/// re-dispatches them against the current workspace and continues
/// the turn loop.
#[derive(Debug)]
pub struct ResumeResult {
	pub resume_tool_calls: Vec<ToolCall>,
}

pub async fn truncate_before_user_record(
	dir: &Utf8Path,
	header: &SessionHeader,
	user_ordinal: usize,
) -> Result<RevertResult, CoderError> {
	let path = session_path(dir, &header.id);
	if !tokio::fs::try_exists(path.as_std_path()).await.unwrap_or(false) {
		return Err(CoderError::Internal(
			"session has no on-disk transcript to revert".into(),
		));
	}
	let LoadedSession { records, .. } = load(dir, &header.id).await?;

	let mut seen = 0usize;
	let mut cut: Option<usize> = None;
	for (idx, record) in records.iter().enumerate() {
		if matches!(record, SessionRecord::User { .. }) {
			if seen == user_ordinal {
				cut = Some(idx);
				break;
			}
			seen += 1;
		}
	}
	let Some(cut) = cut else {
		return Err(CoderError::Internal(format!(
			"revert target out of range: session has {seen} user message(s), asked for #{user_ordinal}"
		)));
	};

	let (dropped_text, dropped_images) = match &records[cut] {
		SessionRecord::User { text, images } => (text.clone(), images.clone()),
		// Unreachable: `cut` was chosen on a `User` match above.
		_ => (String::new(), Vec::new()),
	};

	let surviving: Vec<SessionRecord> = records.into_iter().take(cut).collect();

	// Re-stamp surviving records with the rewrite instant: the
	// in-memory `SessionRecord`s don't carry their original
	// per-line timestamps (we don't parse them back), and revert
	// already re-bakes the prefix — replay mints fresh row ids and
	// the file's mtime moves — so a refreshed timestamp is
	// consistent with that. The fidelity loss only touches the
	// rare edit-and-resend path.
	let rewritten_at = now_ms();
	let mut out = String::new();
	out.push_str(&serde_json::to_string(header).map_err(CoderError::from)?);
	out.push('\n');
	for record in &surviving {
		let wire = record_to_pi_wire(record, header, rewritten_at);
		out.push_str(&serde_json::to_string(&wire).map_err(CoderError::from)?);
		out.push('\n');
	}
	tokio::fs::write(path.as_std_path(), out.as_bytes())
		.await
		.map_err(CoderError::from)?;

	Ok(RevertResult {
		dropped_text,
		dropped_images,
		surviving,
	})
}

/// Truncate the session JSONL to keep everything up to **and
/// including** the `assistant_ordinal`-th `Assistant` record, but
/// **drop** the `Tool` records that immediately follow it (its tool
/// results) and everything after. Returns the kept `Assistant`'s
/// `tool_calls` so the runner can re-dispatch them against the
/// current workspace — the "resume the tool-loop from this
/// checkpoint" gesture.
///
/// Unlike [`truncate_before_user_record`], the cut target is an
/// `Assistant` record and the surviving prefix **includes** it. The
/// matching `Tool` records are deliberately dropped: re-dispatching
/// the tool calls produces fresh `Tool` records with current results,
/// which is the whole point. Keeping the stale ones would feed the
/// model out-of-date tool outputs on the next round-trip.
///
/// Only `Assistant` records with non-empty `tool_calls` are valid
/// anchors — resuming from a tool-call-less assistant is meaningless
/// (the turn already ended there). Returns `CoderError::Internal` if
/// the ordinal is out of range or the target has no tool calls.
pub async fn truncate_before_assistant_record(
	dir: &Utf8Path,
	header: &SessionHeader,
	assistant_ordinal: usize,
) -> Result<ResumeResult, CoderError> {
	let path = session_path(dir, &header.id);
	if !tokio::fs::try_exists(path.as_std_path()).await.unwrap_or(false) {
		return Err(CoderError::Internal(
			"session has no on-disk transcript to resume".into(),
		));
	}
	let LoadedSession { records, .. } = load(dir, &header.id).await?;

	// Find the Nth Assistant record with non-empty tool_calls.
	let mut seen = 0usize;
	let mut cut: Option<usize> = None;
	for (idx, record) in records.iter().enumerate() {
		if let SessionRecord::Assistant { tool_calls, .. } = record {
			if !tool_calls.is_empty() {
				if seen == assistant_ordinal {
					cut = Some(idx);
					break;
				}
				seen += 1;
			}
		}
	}
	let Some(cut) = cut else {
		return Err(CoderError::Internal(format!(
			"resume target out of range: session has {seen} assistant message(s) with tool calls, asked for #{assistant_ordinal}"
		)));
	};

	// Extract the kept Assistant's tool_calls for re-dispatch.
	let resume_tool_calls = match &records[cut] {
		SessionRecord::Assistant { tool_calls, .. } => tool_calls.clone(),
		// Unreachable: `cut` was chosen on an Assistant match above.
		_ => Vec::new(),
	};

	// Surviving = records[0..=cut] — everything up to and including
	// the target Assistant. The Tool records that followed it
	// (records[cut+1..]) are dropped along with everything else.
	let surviving: Vec<SessionRecord> = records.into_iter().take(cut + 1).collect();

	let rewritten_at = now_ms();
	let mut out = String::new();
	out.push_str(&serde_json::to_string(header).map_err(CoderError::from)?);
	out.push('\n');
	for record in &surviving {
		let wire = record_to_pi_wire(record, header, rewritten_at);
		out.push_str(&serde_json::to_string(&wire).map_err(CoderError::from)?);
		out.push('\n');
	}
	tokio::fs::write(path.as_std_path(), out.as_bytes())
		.await
		.map_err(CoderError::from)?;

	Ok(ResumeResult { resume_tool_calls })
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

	fn make_test_header(id: &str) -> SessionHeader {
		SessionHeader {
			schema: SESSION_SCHEMA_VERSION,
			id: id.into(),
			cwd: "/tmp/test".into(),
			title: format!("{id} title"),
			created_at_ms: 1_700_000_000_000,
			updated_at_ms: 1_700_000_000_000,
			model: "test-provider/test-model".into(),
			parent_session_id: None,
			parent_tool_call_id: None,
			subagent_mode: None,
			mode: None,
			subagent_target_folder: None,
			bash_target_override: None,
			worktree_root: None,
			worktree_branch: None,
			committed_branch: None,
		}
	}

	#[tokio::test]
	async fn write_then_read_round_trip() {
		// Round-trip a session through the JSONL writer + reader
		// to make sure the pi wire shape survives serialise +
		// deserialise.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-test");
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
				thinking_blocks: vec![],
				tool_calls: Vec::new(),
				model: None,
				stop_reason: None,
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

	#[test]
	fn bash_target_override_omitted_when_none_and_round_trips_force_host() {
		// Auto (None) must not emit the key — keeps headers byte-clean
		// for the common case and avoids churn on existing sessions.
		let mut header = make_test_header("sess-bto");
		assert_eq!(header.bash_target_override, None);
		let json = serde_json::to_string(&header).unwrap();
		assert!(
			!json.contains("bash_target_override"),
			"auto sessions must omit the override key, got {json}"
		);

		// ForceHost serialises as the `"host"` wire string and reloads.
		header.bash_target_override = Some(BashTargetOverride::ForceHost);
		let json = serde_json::to_string(&header).unwrap();
		assert!(json.contains("\"bash_target_override\":\"host\""), "got {json}");
		let back: SessionHeader = serde_json::from_str(&json).unwrap();
		assert_eq!(back.bash_target_override, Some(BashTargetOverride::ForceHost));

		// An unrecognised / future value degrades to auto rather than
		// erroring the load.
		let weird = json.replace("\"host\"", "\"someday-container\"");
		let back: SessionHeader = serde_json::from_str(&weird).unwrap();
		assert_eq!(back.bash_target_override, None);
	}

	#[tokio::test]
	async fn rewrite_header_updates_first_line_and_preserves_body() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let mut header = make_test_header("sess-rewrite");
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

		// Flip the override and rewrite the header in place.
		header.bash_target_override = Some(BashTargetOverride::ForceHost);
		rewrite_header(&dir, &header).await.unwrap();

		let loaded = load(&dir, "sess-rewrite").await.unwrap();
		assert_eq!(loaded.header.bash_target_override, Some(BashTargetOverride::ForceHost));
		// Body record survived the rewrite untouched.
		assert_eq!(loaded.records.len(), 1);
	}

	#[tokio::test]
	async fn summary_updated_at_tracks_file_mtime_not_frozen_header() {
		// The header's `updated_at_ms` is frozen at creation
		// (header written once, transcript append-only). A
		// follow-up record bumps the file's mtime but never the
		// header, so `load_summary` must derive recency from mtime
		// to sort the session by real activity on reopen.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-mtime");
		let frozen = header.updated_at_ms;
		append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "first".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();

		// A second append a few ms later advances the mtime past
		// the frozen header value.
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "second".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();

		let summary = load_summary(&session_path(&dir, "sess-mtime")).await.unwrap();
		// mtime is "now-ish", which is far past the make_test_header
		// frozen 2023 timestamp — proves we didn't read the header.
		assert!(
			summary.updated_at_ms > frozen,
			"summary updated_at_ms {} should exceed frozen header {}",
			summary.updated_at_ms,
			frozen
		);
	}

	#[tokio::test]
	async fn rewrite_header_is_a_noop_for_unpersisted_session() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-absent");
		// No file on disk yet — must not error or create one.
		rewrite_header(&dir, &header).await.unwrap();
		assert!(!tokio::fs::try_exists(session_path(&dir, "sess-absent").as_std_path())
			.await
			.unwrap());
	}

	#[tokio::test]
	async fn header_emits_type_session_with_cwd() {
		// pi-mono's `detect.ts` keys off `type === "session" &&
		// typeof id === "string" && typeof cwd === "string"`.
		// The header line we write must satisfy all three or the
		// Hub trace viewer won't recognise the file at all.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let mut header = make_test_header("sess-pi-header");
		header.cwd = "/workspace/moon-ide".into();
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
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-pi-header").as_std_path())
			.await
			.unwrap();
		let header_line = body.lines().next().expect("header line");
		let parsed: serde_json::Value = serde_json::from_str(header_line).unwrap();
		assert_eq!(parsed["type"], "session");
		assert_eq!(parsed["id"], "sess-pi-header");
		assert_eq!(parsed["cwd"], "/workspace/moon-ide");
		assert_eq!(parsed["version"], SESSION_SCHEMA_VERSION);
		assert!(parsed["timestamp"].is_string());

		let loaded = load(&dir, "sess-pi-header").await.unwrap();
		assert_eq!(loaded.header.cwd, "/workspace/moon-ide");
		assert_eq!(loaded.header.schema, SESSION_SCHEMA_VERSION);
	}

	#[tokio::test]
	async fn user_record_round_trips_through_pi_wire() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-user");
		append_record(
			&dir,
			&header,
			&SessionRecord::User {
				text: "hello".into(),
				images: Vec::new(),
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-user").as_std_path())
			.await
			.unwrap();
		let user_line = body.lines().nth(1).expect("user line present");
		let parsed: serde_json::Value = serde_json::from_str(user_line).unwrap();
		assert_eq!(parsed["type"], "message");
		assert_eq!(parsed["message"]["role"], "user");
		assert_eq!(parsed["message"]["content"], "hello");

		let loaded = load(&dir, "sess-user").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::User { text, images } => {
				assert_eq!(text, "hello");
				assert!(images.is_empty());
			}
			other => panic!("expected user record, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn image_attachments_strip_data_url_prefix() {
		// Pi's `ImageContent` stores raw base64 in `data` and
		// the mime type in `mimeType`. We must strip our
		// `data:<mime>;base64,` prefix on write so the viewer
		// can call `getBase64ImageDataUrl(data, mimeType)`; on
		// read we re-prefix so the in-memory `data_url` stays
		// exactly what the composer produced.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-img");
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
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-img").as_std_path())
			.await
			.unwrap();
		let user_line = body.lines().nth(1).expect("user line present");
		let parsed: serde_json::Value = serde_json::from_str(user_line).unwrap();
		let content = parsed["message"]["content"].as_array().expect("array");
		assert_eq!(content.len(), 2);
		assert_eq!(content[0]["type"], "text");
		assert_eq!(content[0]["text"], "look at this");
		assert_eq!(content[1]["type"], "image");
		assert_eq!(content[1]["data"], "AAAA");
		assert_eq!(content[1]["mimeType"], "image/png");

		let loaded = load(&dir, "sess-img").await.unwrap();
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
	async fn assistant_with_thinking_text_and_tool_calls_emits_pi_blocks_in_order() {
		// The pi viewer renders blocks in the order they appear
		// in the message's `content` array. We emit thinking →
		// text → toolCall* so the reasoning panel surfaces
		// before the answer, and tool calls follow the answer
		// (matching the pi-mono coding-agent's own ordering).
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-assistant");
		append_record(
			&dir,
			&header,
			&SessionRecord::Assistant {
				content: Some("here's a plan".into()),
				thinking: Some("let me think".into()),
				thinking_blocks: vec![],
				model: None,
				stop_reason: None,
				tool_calls: vec![ToolCall {
					id: "call-1".into(),
					kind: "function".into(),
					function: crate::inference::FunctionCall {
						name: "bash".into(),
						arguments: r#"{"command":"ls"}"#.into(),
					},
				}],
			},
		)
		.await
		.unwrap();

		let body = tokio::fs::read_to_string(session_path(&dir, "sess-assistant").as_std_path())
			.await
			.unwrap();
		let assistant_line = body.lines().nth(1).expect("assistant line present");
		let parsed: serde_json::Value = serde_json::from_str(assistant_line).unwrap();
		assert_eq!(parsed["type"], "message");
		assert_eq!(parsed["message"]["role"], "assistant");
		let blocks = parsed["message"]["content"].as_array().expect("blocks");
		assert_eq!(blocks.len(), 3);
		assert_eq!(blocks[0]["type"], "thinking");
		assert_eq!(blocks[0]["thinking"], "let me think");
		assert_eq!(blocks[1]["type"], "text");
		assert_eq!(blocks[1]["text"], "here's a plan");
		assert_eq!(blocks[2]["type"], "toolCall");
		assert_eq!(blocks[2]["id"], "call-1");
		assert_eq!(blocks[2]["name"], "bash");
		assert_eq!(blocks[2]["arguments"]["command"], "ls");
		// provider / model derived from `header.model`.
		assert_eq!(parsed["message"]["provider"], "test-provider");
		assert_eq!(parsed["message"]["model"], "test-model");

		let loaded = load(&dir, "sess-assistant").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::Assistant {
				content,
				thinking,
				thinking_blocks: _,
				tool_calls,
				model: _,
				stop_reason: _,
			} => {
				assert_eq!(content.as_deref(), Some("here's a plan"));
				assert_eq!(thinking.as_deref(), Some("let me think"));
				assert_eq!(tool_calls.len(), 1);
				assert_eq!(tool_calls[0].id, "call-1");
				assert_eq!(tool_calls[0].function.name, "bash");
				// Args round-trip as a compact JSON object string.
				let reparsed: serde_json::Value = serde_json::from_str(&tool_calls[0].function.arguments).unwrap();
				assert_eq!(reparsed["command"], "ls");
			}
			other => panic!("expected assistant record, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn usage_folds_onto_prior_assistant_line() {
		// pi-mono carries the round-trip's `Usage` on the
		// `usage` field of its assistant message, not as a
		// stand-alone row. Our append path must fold a Usage
		// record onto the prior assistant line on disk; the
		// in-memory shape stays "Assistant followed by Usage"
		// so every existing consumer (replay, context-usage
		// ring) keeps working.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-usage-fold");
		append_record(
			&dir,
			&header,
			&SessionRecord::Assistant {
				content: Some("done".into()),
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: Vec::new(),
				model: None,
				stop_reason: None,
			},
		)
		.await
		.unwrap();
		append_record(
			&dir,
			&header,
			&SessionRecord::Usage {
				prompt_tokens: 100,
				completion_tokens: 20,
				total_tokens: 120,
				cache_read_input_tokens: 80,
				cache_creation_input_tokens: 10,
			},
		)
		.await
		.unwrap();

		let body = tokio::fs::read_to_string(session_path(&dir, "sess-usage-fold").as_std_path())
			.await
			.unwrap();
		let lines: Vec<&str> = body.lines().collect();
		// Header + assistant — no stand-alone usage row.
		assert_eq!(lines.len(), 2, "usage should fold, not append a new line: {body}");
		let assistant_line = lines[1];
		let parsed: serde_json::Value = serde_json::from_str(assistant_line).unwrap();
		let usage = &parsed["message"]["usage"];
		assert_eq!(usage["input"], 100);
		assert_eq!(usage["output"], 20);
		assert_eq!(usage["totalTokens"], 120);
		assert_eq!(usage["cacheRead"], 80);
		assert_eq!(usage["cacheWrite"], 10);

		let loaded = load(&dir, "sess-usage-fold").await.unwrap();
		// In-memory: Assistant + Usage, same shape as schema 1.
		assert_eq!(loaded.records.len(), 2);
		assert!(matches!(loaded.records[0], SessionRecord::Assistant { .. }));
		match &loaded.records[1] {
			SessionRecord::Usage {
				prompt_tokens,
				completion_tokens,
				total_tokens,
				cache_read_input_tokens,
				cache_creation_input_tokens,
			} => {
				assert_eq!(*prompt_tokens, 100);
				assert_eq!(*completion_tokens, 20);
				assert_eq!(*total_tokens, 120);
				assert_eq!(*cache_read_input_tokens, 80);
				assert_eq!(*cache_creation_input_tokens, 10);
			}
			other => panic!("expected Usage, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn usage_without_caching_omits_cache_keys_on_disk() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-usage-plain");
		append_record(
			&dir,
			&header,
			&SessionRecord::Assistant {
				content: Some("ok".into()),
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: Vec::new(),
				model: None,
				stop_reason: None,
			},
		)
		.await
		.unwrap();
		append_record(
			&dir,
			&header,
			&SessionRecord::Usage {
				prompt_tokens: 1234,
				completion_tokens: 56,
				total_tokens: 1290,
				cache_read_input_tokens: 0,
				cache_creation_input_tokens: 0,
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-usage-plain").as_std_path())
			.await
			.unwrap();
		let assistant_line = body.lines().nth(1).expect("assistant line present");
		assert!(assistant_line.contains(r#""input":1234"#));
		assert!(!assistant_line.contains("cacheRead"));
		assert!(!assistant_line.contains("cacheWrite"));

		let loaded = load(&dir, "sess-usage-plain").await.unwrap();
		match &loaded.records[1] {
			SessionRecord::Usage {
				cache_read_input_tokens,
				cache_creation_input_tokens,
				..
			} => {
				assert_eq!(*cache_read_input_tokens, 0);
				assert_eq!(*cache_creation_input_tokens, 0);
			}
			other => panic!("expected Usage, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn title_update_round_trips_via_custom() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-title");
		append_record(
			&dir,
			&header,
			&SessionRecord::TitleUpdate {
				title: "renamed".into(),
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-title").as_std_path())
			.await
			.unwrap();
		let title_line = body.lines().nth(1).expect("title line");
		let parsed: serde_json::Value = serde_json::from_str(title_line).unwrap();
		assert_eq!(parsed["type"], "message");
		assert_eq!(parsed["message"]["role"], "custom");
		assert_eq!(parsed["message"]["customType"], CUSTOM_TYPE_TITLE_UPDATE);
		assert_eq!(parsed["message"]["display"], false);
		assert_eq!(parsed["message"]["details"]["title"], "renamed");

		let loaded = load(&dir, "sess-title").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::TitleUpdate { title } => assert_eq!(title, "renamed"),
			other => panic!("expected TitleUpdate, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn todos_update_round_trips_via_custom() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-todos");
		let todos = vec![
			crate::TodoItem {
				id: "t1".into(),
				content: "first".into(),
				status: crate::TodoStatus::Completed,
			},
			crate::TodoItem {
				id: "t2".into(),
				content: "second".into(),
				status: crate::TodoStatus::Pending,
			},
		];
		append_record(&dir, &header, &SessionRecord::TodosUpdate { todos: todos.clone() })
			.await
			.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-todos").as_std_path())
			.await
			.unwrap();
		let line = body.lines().nth(1).expect("todos line");
		let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
		assert_eq!(parsed["message"]["customType"], CUSTOM_TYPE_TODOS_UPDATE);
		assert_eq!(parsed["message"]["display"], false);
		let written_todos = parsed["message"]["details"]["todos"].as_array().expect("todos");
		assert_eq!(written_todos.len(), 2);

		let loaded = load(&dir, "sess-todos").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::TodosUpdate { todos: out } => {
				assert_eq!(out.len(), 2);
				assert_eq!(out[0].id, "t1");
				assert_eq!(out[0].status, crate::TodoStatus::Completed);
				assert_eq!(out[1].status, crate::TodoStatus::Pending);
			}
			other => panic!("expected TodosUpdate, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn error_record_round_trips_via_custom() {
		// A terminal turn error persists as a `moon_error` custom
		// row and reloads as `SessionRecord::Error` so the reopened
		// transcript shows the failure inline instead of trailing
		// off at the last successful record.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-error");
		append_record(
			&dir,
			&header,
			&SessionRecord::Error {
				message: "router 400: invalid tool arguments".into(),
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-error").as_std_path())
			.await
			.unwrap();
		let line = body.lines().nth(1).expect("error line");
		let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
		assert_eq!(parsed["message"]["customType"], CUSTOM_TYPE_ERROR);
		assert_eq!(parsed["message"]["display"], false);
		assert_eq!(
			parsed["message"]["details"]["message"],
			"router 400: invalid tool arguments"
		);

		let loaded = load(&dir, "sess-error").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::Error { message } => {
				assert_eq!(message, "router 400: invalid tool arguments");
			}
			other => panic!("expected Error, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn compaction_emits_top_level_pi_compaction_row() {
		// Compaction rides as its own top-level pi row (not a
		// message envelope), matching pi-mono's session log
		// shape so the trace viewer renders the compaction
		// banner without us having to wrap it in a custom row.
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-compaction");
		append_record(
			&dir,
			&header,
			&SessionRecord::Compaction {
				summary: "earlier turns: refactored foo into bar".into(),
				messages_compacted: 42,
				messages_kept: 12,
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-compaction").as_std_path())
			.await
			.unwrap();
		let comp_line = body.lines().nth(1).expect("compaction line");
		let parsed: serde_json::Value = serde_json::from_str(comp_line).unwrap();
		assert_eq!(parsed["type"], "compaction");
		assert_eq!(parsed["summary"], "earlier turns: refactored foo into bar");
		assert_eq!(parsed["details"]["messages_compacted"], 42);
		assert_eq!(parsed["details"]["messages_kept"], 12);

		let loaded = load(&dir, "sess-compaction").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::Compaction {
				summary,
				messages_compacted,
				messages_kept,
			} => {
				assert_eq!(summary, "earlier turns: refactored foo into bar");
				assert_eq!(*messages_compacted, 42);
				assert_eq!(*messages_kept, 12);
			}
			other => panic!("expected Compaction, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn tool_result_emits_pi_tool_result_envelope() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-tool");
		append_record(
			&dir,
			&header,
			&SessionRecord::Tool {
				tool_call_id: "call-1".into(),
				tool_name: String::new(),
				content: r#"{"stdout":"ok"}"#.into(),
			},
		)
		.await
		.unwrap();
		append_record(
			&dir,
			&header,
			&SessionRecord::Tool {
				tool_call_id: "call-2".into(),
				tool_name: String::new(),
				content: INTERRUPTED_TOOL_RESULT_JSON.into(),
			},
		)
		.await
		.unwrap();
		let body = tokio::fs::read_to_string(session_path(&dir, "sess-tool").as_std_path())
			.await
			.unwrap();
		let lines: Vec<&str> = body.lines().collect();
		let ok_line: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
		assert_eq!(ok_line["message"]["role"], "toolResult");
		assert_eq!(ok_line["message"]["toolCallId"], "call-1");
		assert_eq!(ok_line["message"]["isError"], false);
		assert_eq!(ok_line["message"]["content"][0]["text"], r#"{"stdout":"ok"}"#);

		let err_line: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
		assert_eq!(err_line["message"]["isError"], true);

		let loaded = load(&dir, "sess-tool").await.unwrap();
		match &loaded.records[0] {
			SessionRecord::Tool {
				tool_call_id,
				content,
				tool_name: _,
			} => {
				assert_eq!(tool_call_id, "call-1");
				assert_eq!(content, r#"{"stdout":"ok"}"#);
			}
			other => panic!("expected Tool, got {other:?}"),
		}
		match &loaded.records[1] {
			SessionRecord::Tool { content, .. } => {
				assert_eq!(content, INTERRUPTED_TOOL_RESULT_JSON);
			}
			other => panic!("expected Tool, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn subagent_records_round_trip_via_custom() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header("sess-sub");
		append_record(
			&dir,
			&header,
			&SessionRecord::SubagentSpawned {
				tool_call_id: "call-1".into(),
				subagent_id: "sub-x".into(),
				target_folder: "/workspace/api".into(),
				mode: "agent".into(),
			},
		)
		.await
		.unwrap();
		append_record(
			&dir,
			&header,
			&SessionRecord::SubagentFinished {
				subagent_id: "sub-x".into(),
				tokens_used_estimate: 1234,
				was_error: false,
				result_preview: Some("did it".into()),
			},
		)
		.await
		.unwrap();
		let loaded = load(&dir, "sess-sub").await.unwrap();
		assert_eq!(loaded.records.len(), 2);
		match &loaded.records[0] {
			SessionRecord::SubagentSpawned {
				tool_call_id,
				subagent_id,
				target_folder,
				mode,
			} => {
				assert_eq!(tool_call_id, "call-1");
				assert_eq!(subagent_id, "sub-x");
				assert_eq!(target_folder, "/workspace/api");
				assert_eq!(mode, "agent");
			}
			other => panic!("expected SubagentSpawned, got {other:?}"),
		}
		match &loaded.records[1] {
			SessionRecord::SubagentFinished {
				subagent_id,
				tokens_used_estimate,
				was_error,
				result_preview,
			} => {
				assert_eq!(subagent_id, "sub-x");
				assert_eq!(*tokens_used_estimate, 1234);
				assert!(!*was_error);
				assert_eq!(result_preview.as_deref(), Some("did it"));
			}
			other => panic!("expected SubagentFinished, got {other:?}"),
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

		let parent_header = make_test_header("sess-parent");
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
		let mut sub_header = make_test_header("sub-child");
		sub_header.parent_session_id = Some("sess-parent".into());
		sub_header.parent_tool_call_id = Some("call-1".into());
		sub_header.subagent_mode = Some("agent".into());
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

		let found = find_subagent_session(&dir, "sub-child").await;
		assert_eq!(found, Some(session_path(&sub_dir, "sub-child")));
		let not_found = find_subagent_session(&dir, "sess-parent").await;
		assert_eq!(not_found, None);
	}

	#[tokio::test]
	async fn delete_session_removes_subagent_subdir() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

		let parent_header = make_test_header("sess-parent");
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
		let mut sub_header = make_test_header("sub-child");
		sub_header.parent_session_id = Some("sess-parent".into());
		sub_header.parent_tool_call_id = Some("call-1".into());
		sub_header.subagent_mode = Some("agent".into());
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

	#[test]
	fn iso8601_utc_ms_formats_epoch_zero() {
		assert_eq!(iso8601_utc_ms(0), "1970-01-01T00:00:00.000Z");
	}

	#[test]
	fn iso8601_utc_ms_formats_known_timestamp() {
		// 2024-01-15T12:34:56.789Z (validated against an
		// independent computation).
		let ms = 1_705_322_096_789;
		assert_eq!(iso8601_utc_ms(ms), "2024-01-15T12:34:56.789Z");
	}

	#[test]
	fn split_provider_model_handles_slashed_and_plain() {
		assert_eq!(
			split_provider_model("anthropic/claude-sonnet-4.5"),
			(Some("anthropic"), "claude-sonnet-4.5")
		);
		assert_eq!(split_provider_model("local-model"), (None, "local-model"));
	}

	#[test]
	fn strip_data_url_prefix_recovers_raw_base64() {
		let (data, mime) = strip_data_url_prefix("data:image/png;base64,AAAA", "image/png");
		assert_eq!(data, "AAAA");
		assert_eq!(mime, "image/png");
		// No prefix → passthrough.
		let (data, mime) = strip_data_url_prefix("AAAA", "image/png");
		assert_eq!(data, "AAAA");
		assert_eq!(mime, "image/png");
	}

	#[test]
	fn looks_like_tool_error_detects_error_shapes() {
		assert!(looks_like_tool_error(INTERRUPTED_TOOL_RESULT_JSON));
		assert!(looks_like_tool_error(r#"{"error":"boom"}"#));
		assert!(!looks_like_tool_error(r#"{"stdout":"ok"}"#));
		assert!(!looks_like_tool_error("plain text"));
		assert!(!looks_like_tool_error(r#"{"error":"boom","stderr":""}"#));
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

	fn make_tool_call(id: &str, name: &str) -> ToolCall {
		ToolCall {
			id: id.into(),
			kind: "function".into(),
			function: crate::inference::FunctionCall {
				name: name.into(),
				arguments: "{}".into(),
			},
		}
	}

	#[test]
	fn orphan_tool_call_ids_returns_empty_when_every_call_has_a_result() {
		let records = vec![
			SessionRecord::User {
				text: "hi".into(),
				images: Vec::new(),
			},
			SessionRecord::Assistant {
				content: None,
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: vec![make_tool_call("call-a", "bash")],
				model: None,
				stop_reason: None,
			},
			SessionRecord::Tool {
				tool_call_id: "call-a".into(),
				tool_name: String::new(),
				content: r#"{"stdout":"ok"}"#.into(),
			},
		];
		assert!(orphan_tool_call_ids(&records).is_empty());
	}

	#[test]
	fn orphan_tool_call_ids_finds_interrupted_tail() {
		// Common shape: user prompted, model emitted two
		// tool_calls in one Assistant message, the dispatcher
		// got through the first one and the user pressed Stop
		// before the second returned.
		let records = vec![
			SessionRecord::User {
				text: "go".into(),
				images: Vec::new(),
			},
			SessionRecord::Assistant {
				content: None,
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: vec![make_tool_call("call-a", "bash"), make_tool_call("call-b", "bash")],
				model: None,
				stop_reason: None,
			},
			SessionRecord::Tool {
				tool_call_id: "call-a".into(),
				tool_name: String::new(),
				content: r#"{"stdout":"a"}"#.into(),
			},
		];
		assert_eq!(orphan_tool_call_ids(&records), vec!["call-b".to_string()]);
	}

	#[test]
	fn orphan_tool_call_ids_handles_multiple_assistant_turns() {
		// Two complete turns followed by a third orphan turn.
		// The completed turns shouldn't show up as orphans even
		// though their tool_calls came earlier in the stream.
		let records = vec![
			SessionRecord::Assistant {
				content: None,
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: vec![make_tool_call("call-a", "bash")],
				model: None,
				stop_reason: None,
			},
			SessionRecord::Tool {
				tool_call_id: "call-a".into(),
				tool_name: String::new(),
				content: "{}".into(),
			},
			SessionRecord::Assistant {
				content: None,
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: vec![make_tool_call("call-b", "read_file")],
				model: None,
				stop_reason: None,
			},
			SessionRecord::Tool {
				tool_call_id: "call-b".into(),
				tool_name: String::new(),
				content: "{}".into(),
			},
			SessionRecord::Assistant {
				content: None,
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: vec![make_tool_call("call-c", "bash")],
				model: None,
				stop_reason: None,
			},
		];
		assert_eq!(orphan_tool_call_ids(&records), vec!["call-c".to_string()]);
	}

	#[test]
	fn orphan_tool_call_ids_ignores_text_only_assistant_turns() {
		// Plain "ask the model a question, get an answer back"
		// — no tool_calls anywhere. Must return empty.
		let records = vec![
			SessionRecord::User {
				text: "what time is it".into(),
				images: Vec::new(),
			},
			SessionRecord::Assistant {
				content: Some("I don't know".into()),
				thinking: None,
				thinking_blocks: vec![],
				tool_calls: Vec::new(),
				model: None,
				stop_reason: None,
			},
		];
		assert!(orphan_tool_call_ids(&records).is_empty());
	}

	#[test]
	fn empty_shell_assistant_row_drops_on_load() {
		// An assistant envelope written by an older runner that
		// landed `{"role":"assistant","content":[]}` on disk
		// after the provider bailed mid-stream. Reinflating one
		// into `ChatMessage::Assistant { content: None, tool_calls:
		// [] }` poisons the next prompt on Anthropic (`text
		// content blocks must contain non-whitespace text`).
		// The load path must drop it so reopened sessions stay
		// sendable.
		let envelope = serde_json::json!({
			"type": "message",
			"message": {
				"role": "assistant",
				"content": [],
				"provider": "anthropic",
				"model": "claude-sonnet-4.5",
				"usage": { "input": 100, "output": 0, "totalTokens": 100 },
			},
		});
		let records = pi_wire_to_records(&envelope);
		// Drop the assistant row, keep the usage block.
		assert_eq!(records.len(), 1);
		assert!(matches!(records[0], SessionRecord::Usage { .. }));
	}

	#[test]
	fn whitespace_only_assistant_row_drops_on_load() {
		// Same shape, but the empty shell came back as a single
		// whitespace text block instead of `content: []`. Anthropic
		// rejects both the same way; the load path treats them
		// identically.
		let envelope = serde_json::json!({
			"type": "message",
			"message": {
				"role": "assistant",
				"content": [{ "type": "text", "text": "   \n\t" }],
			},
		});
		let records = pi_wire_to_records(&envelope);
		assert!(records.is_empty(), "expected drop, got {records:?}");
	}

	#[test]
	fn assistant_with_only_tool_calls_survives_load() {
		// Tool-only assistant turns (no text, no thinking) are
		// legitimate and must keep round-tripping. Only the
		// fully-empty shell gets dropped.
		let envelope = serde_json::json!({
			"type": "message",
			"message": {
				"role": "assistant",
				"content": [{
					"type": "toolCall",
					"id": "call-1",
					"name": "bash",
					"arguments": { "command": "ls" },
				}],
			},
		});
		let records = pi_wire_to_records(&envelope);
		assert_eq!(records.len(), 1);
		match &records[0] {
			SessionRecord::Assistant {
				content,
				thinking,
				thinking_blocks: _,
				tool_calls,
				model,
				stop_reason: _,
			} => {
				assert!(content.is_none());
				assert!(thinking.is_none());
				assert_eq!(tool_calls.len(), 1);
				assert_eq!(tool_calls[0].id, "call-1");
				assert!(model.is_none());
			}
			other => panic!("expected Assistant, got {other:?}"),
		}
	}

	#[test]
	fn assistant_model_stamp_round_trips_through_pi_wire() {
		// The runner stamps `provider/model` (e.g.
		// `anthropic/claude-sonnet-4.5`) into the record. Pi-mono
		// renders it as separate `provider` + `model` fields on
		// the assistant envelope; on reload we glue them back
		// together. End-to-end: the value the runner persisted
		// is the value the next load reconstructs.
		let header = make_test_header("sess-model-stamp");
		let record = SessionRecord::Assistant {
			content: Some("hello".into()),
			thinking: None,
			thinking_blocks: vec![],
			tool_calls: Vec::new(),
			model: Some("anthropic/claude-sonnet-4.5".into()),
			stop_reason: None,
		};
		let wire = record_to_pi_wire(&record, &header, 0);
		// Sanity: pi envelope split the stamp into the conventional fields.
		let msg = wire.get("message").unwrap();
		assert_eq!(msg.get("provider").and_then(|v| v.as_str()), Some("anthropic"));
		assert_eq!(msg.get("model").and_then(|v| v.as_str()), Some("claude-sonnet-4.5"));
		// Round-trip back through the loader.
		let records = pi_wire_to_records(&wire);
		match &records[0] {
			SessionRecord::Assistant { model, .. } => {
				assert_eq!(model.as_deref(), Some("anthropic/claude-sonnet-4.5"));
			}
			other => panic!("expected Assistant, got {other:?}"),
		}
	}

	#[test]
	fn pi_wire_stamps_timestamps_tool_name_and_stop_reason() {
		// Schema 3 picked up pi's per-line / per-message field
		// additions. Pin them on the wire shape: an ISO-8601
		// `timestamp` on the envelope, a Unix-ms `timestamp` inside
		// the message, `stopReason` on the assistant message, and
		// `toolName` on the tool-result message.
		let header = make_test_header("sess-v3-fields");
		let ts: i64 = 1_700_000_000_123;

		let assistant = SessionRecord::Assistant {
			content: Some("done".into()),
			thinking: None,
			thinking_blocks: vec![],
			tool_calls: Vec::new(),
			model: Some("anthropic/claude-sonnet-4.5".into()),
			stop_reason: Some("toolUse".into()),
		};
		let wire = record_to_pi_wire(&assistant, &header, ts);
		assert_eq!(
			wire.get("timestamp").and_then(|v| v.as_str()),
			Some("2023-11-14T22:13:20.123Z"),
			"envelope carries the ISO-8601 timestamp"
		);
		let msg = wire.get("message").unwrap();
		assert_eq!(
			msg.get("timestamp").and_then(|v| v.as_i64()),
			Some(ts),
			"message carries the Unix-ms timestamp"
		);
		assert_eq!(msg.get("stopReason").and_then(|v| v.as_str()), Some("toolUse"));
		// stopReason survives the reload round-trip.
		match &pi_wire_to_records(&wire)[0] {
			SessionRecord::Assistant { stop_reason, .. } => {
				assert_eq!(stop_reason.as_deref(), Some("toolUse"));
			}
			other => panic!("expected Assistant, got {other:?}"),
		}

		let tool = SessionRecord::Tool {
			tool_call_id: "call-1".into(),
			tool_name: "read_file".into(),
			content: r#"{"ok":true}"#.into(),
		};
		let wire = record_to_pi_wire(&tool, &header, ts);
		let msg = wire.get("message").unwrap();
		assert_eq!(msg.get("toolName").and_then(|v| v.as_str()), Some("read_file"));
		match &pi_wire_to_records(&wire)[0] {
			SessionRecord::Tool { tool_name, .. } => assert_eq!(tool_name, "read_file"),
			other => panic!("expected Tool, got {other:?}"),
		}
	}

	#[test]
	fn signed_thinking_blocks_round_trip_through_pi_wire() {
		// A reopened session that was mid-tool-loop on a thinking
		// model must replay the signed/redacted reasoning blocks
		// verbatim, or the next round-trip 400s. Pin the persist →
		// reload round-trip end-to-end.
		use crate::inference::ThinkingBlock;
		let header = make_test_header("sess-thinking");
		let record = SessionRecord::Assistant {
			content: None,
			thinking: Some("let me think".into()),
			thinking_blocks: vec![
				ThinkingBlock::Thinking {
					thinking: "let me think".into(),
					signature: "sig-xyz".into(),
				},
				ThinkingBlock::RedactedThinking { data: "opaque".into() },
			],
			tool_calls: vec![ToolCall {
				id: "toolu_1".into(),
				kind: "function".into(),
				function: crate::inference::FunctionCall {
					name: "bash".into(),
					arguments: r#"{"cmd":"ls"}"#.into(),
				},
			}],
			model: Some("anthropic/claude-fable-5".into()),
			stop_reason: None,
		};
		let wire = record_to_pi_wire(&record, &header, 0);
		let records = pi_wire_to_records(&wire);
		match &records[0] {
			SessionRecord::Assistant { thinking_blocks, .. } => {
				assert_eq!(thinking_blocks.len(), 2, "both reasoning blocks survive reload");
				assert!(
					matches!(&thinking_blocks[0], ThinkingBlock::Thinking { signature, .. } if signature == "sig-xyz"),
					"signed thinking block preserves its signature"
				);
				assert!(
					matches!(&thinking_blocks[1], ThinkingBlock::RedactedThinking { data } if data == "opaque"),
					"redacted block preserves its opaque data"
				);
			}
			other => panic!("expected Assistant, got {other:?}"),
		}
	}

	#[test]
	fn plain_summary_thinking_does_not_round_trip_as_replayable_block() {
		// Non-Anthropic providers (or legacy rows) carry only a
		// human-readable `thinking` string with no signature. That
		// must NOT reconstruct into a replayable `ThinkingBlock` —
		// there's no signature to replay, and emitting one would 400.
		let header = make_test_header("sess-plain-thinking");
		let record = SessionRecord::Assistant {
			content: Some("the answer".into()),
			thinking: Some("some reasoning".into()),
			thinking_blocks: vec![],
			tool_calls: Vec::new(),
			model: Some("hf/deepseek".into()),
			stop_reason: None,
		};
		let wire = record_to_pi_wire(&record, &header, 0);
		let records = pi_wire_to_records(&wire);
		match &records[0] {
			SessionRecord::Assistant {
				thinking,
				thinking_blocks,
				..
			} => {
				assert_eq!(thinking.as_deref(), Some("some reasoning"));
				assert!(
					thinking_blocks.is_empty(),
					"summary-only thinking carries no replayable block"
				);
			}
			other => panic!("expected Assistant, got {other:?}"),
		}
	}

	/// Build a two-turn transcript on disk and return its dir +
	/// header. Turn 1: user "first" → assistant. Turn 2: user
	/// "second" → assistant. Used by the revert tests below.
	async fn make_two_turn_session(id: &str) -> (tempfile::TempDir, Utf8PathBuf, SessionHeader) {
		let tmp = tempfile::tempdir().unwrap();
		let dir = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let header = make_test_header(id);
		for (user, assistant) in [("first", "reply-1"), ("second", "reply-2")] {
			append_record(
				&dir,
				&header,
				&SessionRecord::User {
					text: user.into(),
					images: Vec::new(),
				},
			)
			.await
			.unwrap();
			append_record(
				&dir,
				&header,
				&SessionRecord::Assistant {
					content: Some(assistant.into()),
					thinking: None,
					thinking_blocks: vec![],
					tool_calls: Vec::new(),
					model: None,
					stop_reason: None,
				},
			)
			.await
			.unwrap();
		}
		(tmp, dir, header)
	}

	#[tokio::test]
	async fn truncate_before_user_record_drops_second_turn() {
		let (_tmp, dir, header) = make_two_turn_session("sess-revert").await;
		// Revert to the second user message (ordinal 1): keep
		// turn 1 (user + assistant), drop turn 2.
		let result = truncate_before_user_record(&dir, &header, 1).await.unwrap();
		assert_eq!(result.dropped_text, "second");
		assert_eq!(result.surviving.len(), 2);

		let reloaded = load(&dir, "sess-revert").await.unwrap();
		assert_eq!(reloaded.records.len(), 2);
		assert!(matches!(&reloaded.records[0], SessionRecord::User { text, .. } if text == "first"));
		assert!(
			matches!(&reloaded.records[1], SessionRecord::Assistant { content, .. } if content.as_deref() == Some("reply-1"))
		);
		// Header is preserved verbatim.
		assert_eq!(reloaded.header.id, "sess-revert");
	}

	#[tokio::test]
	async fn truncate_before_user_record_first_message_empties_transcript() {
		let (_tmp, dir, header) = make_two_turn_session("sess-revert-all").await;
		let result = truncate_before_user_record(&dir, &header, 0).await.unwrap();
		assert_eq!(result.dropped_text, "first");
		assert!(result.surviving.is_empty());

		let reloaded = load(&dir, "sess-revert-all").await.unwrap();
		assert!(reloaded.records.is_empty());
		// The file still exists with just its header line, so a
		// subsequent send appends rather than re-writing a header.
		assert!(
			tokio::fs::try_exists(session_path(&dir, "sess-revert-all").as_std_path())
				.await
				.unwrap()
		);
	}

	#[tokio::test]
	async fn truncate_before_user_record_rejects_out_of_range() {
		let (_tmp, dir, header) = make_two_turn_session("sess-revert-oob").await;
		// Only 2 user messages (ordinals 0, 1); ordinal 2 is out
		// of range.
		let err = truncate_before_user_record(&dir, &header, 2).await.unwrap_err();
		assert!(matches!(err, CoderError::Internal(_)), "got {err:?}");
		// The transcript is untouched on the error path.
		let reloaded = load(&dir, "sess-revert-oob").await.unwrap();
		assert_eq!(reloaded.records.len(), 4);
	}

	mod coordinator_mode_field {
		use super::*;

		#[test]
		fn coordinator_mode_serializes_and_elides_for_agent() {
			// A coordinator session's header carries `mode:
			// "coordinator"`; an ordinary agent session's header
			// elides the field entirely (byte-compatible with
			// pre-schema-6 sessions).
			let mut coord = make_test_header("sess-coord");
			coord.mode = Some("coordinator".to_string());
			let json = serde_json::to_string(&coord).unwrap();
			assert!(json.contains("\"mode\":\"coordinator\""));

			let agent = make_test_header("sess-agent");
			let json = serde_json::to_string(&agent).unwrap();
			// `mode` field absent for the default agent session.
			assert!(!json.contains("\"mode\""));
		}

		#[test]
		fn coordinator_mode_round_trips_through_serialize_deserialize() {
			let mut coord = make_test_header("sess-coord-rt");
			coord.mode = Some("coordinator".to_string());
			let json = serde_json::to_string(&coord).unwrap();
			let back: SessionHeader = serde_json::from_str(&json).unwrap();
			assert_eq!(back.mode.as_deref(), Some("coordinator"));
		}

		#[test]
		fn absent_mode_deserializes_to_none() {
			// A pre-schema-6 header has no `mode` field; loading it
			// must produce `None`, which `from_top_level_wire` maps
			// to `Agent`.
			let agent = make_test_header("sess-old");
			let json = serde_json::to_string(&agent).unwrap();
			let back: SessionHeader = serde_json::from_str(&json).unwrap();
			assert!(back.mode.is_none());
		}

		#[test]
		fn session_summary_carries_mode() {
			let mut coord = make_test_header("sess-coord-summary");
			coord.mode = Some("coordinator".to_string());
			let summary = SessionSummary {
				id: coord.id.clone(),
				title: coord.title.clone(),
				created_at_ms: coord.created_at_ms,
				updated_at_ms: coord.updated_at_ms,
				worktree_branch: None,
				committed_branch: None,
				mode: coord.mode.clone(),
			};
			let json = serde_json::to_string(&summary).unwrap();
			assert!(json.contains("\"mode\":\"coordinator\""));
			// An agent summary elides it.
			let agent_summary = SessionSummary {
				id: "sess-agent".into(),
				title: "t".into(),
				created_at_ms: 1,
				updated_at_ms: 1,
				worktree_branch: None,
				committed_branch: None,
				mode: None,
			};
			let json = serde_json::to_string(&agent_summary).unwrap();
			assert!(!json.contains("\"mode\""));
		}
	}
}
