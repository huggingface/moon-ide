//! Filesystem-shaped operations exposed by the workspace host.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
	File,
	Dir,
	Symlink,
	Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DirEntry {
	pub name: String,
	pub path: String,
	pub kind: EntryKind,
	/// Size in bytes for files. None for directories or when stat is skipped.
	pub size: Option<u64>,
	/// Modification time as Unix milliseconds. None when stat is skipped.
	pub mtime_ms: Option<i64>,
	pub is_hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ReadFileResult {
	/// UTF-8 text content. Binary files surface a separate API later.
	pub text: String,
	pub mtime_ms: Option<i64>,
	/// Best-effort detection: true if the file looked binary and was not decoded.
	pub is_binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WriteFileResult {
	pub mtime_ms: Option<i64>,
	pub bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct StatResult {
	pub kind: EntryKind,
	pub size: u64,
	pub mtime_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum FsEventKind {
	Create,
	Modify,
	Remove,
	Rename,
	Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FsEvent {
	pub subscription_id: String,
	pub kind: FsEventKind,
	pub path: String,
}
