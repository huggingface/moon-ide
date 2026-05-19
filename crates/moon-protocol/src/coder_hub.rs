//! HF Hub bucket sync for coder session traces.
//!
//! When a workspace is connected to a bucket, this module's
//! [`CoderHubBucket`] lives on the workspace's
//! [`crate::session::WorkspaceSession`] and pins the destination
//! the runner pushes session JSONLs to. One bucket per workspace
//! by design — see [`specs/coder.md`](../../../specs/coder.md)
//! § "Bucket sync (HF buckets)" for the rationale and the on-Hub
//! layout (`sessions/<id>.jsonl` + `README.md`).
//!
//! Per-workspace `uploaded` map is the local cache that lets us
//! skip a re-upload when the JSONL on disk hasn't grown since the
//! last successful push. The Hub itself is the source of truth
//! for actually-stored bytes; this map is just an optimisation —
//! re-uploading an unchanged file is correct but wastes a
//! round-trip.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use ts_rs::TS;

/// One workspace's binding to an HF Hub bucket. Persisted on
/// [`crate::session::WorkspaceSession::coder_hub_bucket`].
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(default)]
pub struct CoderHubBucket {
	/// HF namespace the bucket lives under — either the user's
	/// login or one of their orgs' names. Used in every Hub URL
	/// the runner builds (`/api/buckets/<namespace>/<name>/...`).
	pub namespace: String,
	/// Bucket name. Together with `namespace`, uniquely
	/// identifies the bucket on the Hub.
	pub name: String,
	/// Visibility at create time. Carried for display only — the
	/// Hub is the source of truth; flipping visibility on the
	/// web UI doesn't update this flag, which is fine because the
	/// runner never reads it for an access decision (the OAuth
	/// `contribute-repos` scope is the access fence).
	pub private: bool,
	/// When `true`, the runner enqueues a sync after every
	/// `TurnEnded` for sessions in this workspace. Default
	/// `false` after connect — the modal nudges the user to flip
	/// it on but doesn't auto-flip, so connecting a bucket
	/// doesn't trigger surprise uploads.
	pub autosync: bool,
	/// `session_id → marker` of the last successful push. Used by
	/// the sync loop to skip an upload when the local JSONL
	/// length matches `marker.bytes` (i.e. nothing has been
	/// appended since the last sync). Cleared on disconnect.
	pub uploaded: HashMap<String, UploadedMarker>,
}

impl Default for CoderHubBucket {
	fn default() -> Self {
		Self {
			namespace: String::new(),
			name: String::new(),
			private: true,
			autosync: false,
			uploaded: HashMap::new(),
		}
	}
}

/// Bookkeeping for one session's last successful Hub push.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct UploadedMarker {
	/// Length in bytes of the local JSONL when it was last
	/// pushed. The runner short-circuits a re-upload when the
	/// current on-disk length matches — Xet would dedup the
	/// content anyway, but skipping the call entirely saves the
	/// `xet-write-token` round-trip + the `batch` POST.
	pub bytes: u64,
	/// Wall-clock time (Unix ms) when the push landed. Surfaced
	/// in the session-list "Synced 2m ago" tooltip. Purely
	/// informational — sync decisions key off `bytes`.
	pub at_ms: i64,
}

/// Namespace summary returned by `coder_hub_list_namespaces`.
/// Populates the connect modal's dropdown with the user's login
/// and every org they belong to.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HubNamespace {
	/// The signed-in user's own namespace.
	User { name: String },
	/// An organisation the user is a member of.
	Org { name: String },
}

impl HubNamespace {
	pub fn name(&self) -> &str {
		match self {
			HubNamespace::User { name } | HubNamespace::Org { name } => name,
		}
	}
}

/// Result of `coder_hub_upload_all_sessions` — the bulk "push
/// every local session JSONL into the bound bucket" affordance
/// the settings modal exposes.
///
/// The op groups every folder bound to the workspace under one
/// `xet-write-token` fetch and one `/batch` POST (the Hub's NDJSON
/// add-file endpoint accepts a stream of entries), so a workspace
/// with N stale sessions does roughly **two** Hub-API round-trips
/// plus N parallel Xet CAS uploads — instead of the 3·N round-
/// trips the per-row "Upload" button would cost.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq, Default)]
#[ts(export)]
pub struct HubUploadAllSummary {
	/// Sessions whose CAS upload + `addFile` landed cleanly.
	/// Already-synced sessions (matching length in the workspace's
	/// `uploaded` marker map) don't count here — they bypass the
	/// upload entirely and are reported in [`skipped`] instead.
	pub uploaded: u32,
	/// Sessions skipped because the local JSONL hadn't grown since
	/// the last successful push. The Hub already has the bytes
	/// thanks to Xet dedup; we just avoid the round-trip.
	pub skipped: u32,
	/// Per-session failure details. Best-effort partial success:
	/// every uploadable session is attempted independently, so
	/// one failure doesn't poison the rest.
	pub failed: Vec<HubUploadFailure>,
}

/// One session that errored out during `coder_hub_upload_all_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct HubUploadFailure {
	pub session_id: String,
	pub error: String,
}
