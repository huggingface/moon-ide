//! HF Hub bucket sync — pushes per-folder coder session JSONLs
//! to a workspace-scoped HF Hub bucket so they render in the
//! Hub's pi-mono trace viewer.
//!
//! Owned by [`crate::runner`]'s `CoderState::hub_sync` and driven
//! by three external entry points:
//! - [`HubSync::list_namespaces`] — populates the connect modal's
//!   dropdown with the user's login + every org they belong to.
//!   Reads from the cached OAuth identity, no extra Hub round-
//!   trip required (every signed-in user already paid for one at
//!   sign-in).
//! - [`HubSync::create_bucket`] + [`HubSync::write_readme`] —
//!   provisioning called by the connect-modal Tauri command.
//! - [`HubSync::upload_session`] / [`HubSync::enqueue_session_sync`]
//!   — per-session pushes. The first is the synchronous
//!   "upload now and tell me if it worked" path the manual
//!   button uses; the second debounces autosync calls per
//!   `(workspace_id, session_id)` so a flurry of `TurnEnded`
//!   events collapses into one upload.
//!
//! All state lives in this module; the runner only ever calls in.
//! Events back to the panel ride
//! [`crate::CoderEvent::HubSyncStarted`] / [`HubSyncFinished`] so
//! the per-session row decoration ("syncing… / synced 2m ago /
//! failed") doesn't need a separate IPC plumb.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use moon_core::session as core_session;
use moon_protocol::coder_hub::{CoderHubBucket, HubNamespace, UploadedMarker};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::auth::Authenticator;
use crate::defaults::HF_HUB_BASE;
use crate::error::{request_id_of, CoderError};
use crate::event::{CoderEvent, CoderEventEnvelope};
use crate::sessions;

/// Debounce window between an `enqueue_session_sync` call and the
/// actual Hub round-trip. The runner enqueues on every
/// `TurnComplete`, and a chatty turn that ends with several
/// successive append-then-emit cycles would otherwise produce one
/// upload per cycle. 2 s is short enough that "I just typed a
/// follow-up" feels live and long enough to fold tool-result +
/// final-assistant + usage records into one push.
const SYNC_DEBOUNCE: Duration = Duration::from_secs(2);

/// Reasonable content-type for a JSONL pi-mono trace.
const SESSION_CONTENT_TYPE: &str = "application/x-ndjson";

/// Markdown content-type for the bucket README.
const README_CONTENT_TYPE: &str = "text/markdown";

/// State owned by [`crate::runner::CoderState`]. Cloning is cheap
/// — every field is either `Clone` or wrapped in an `Arc`.
#[derive(Clone)]
pub struct HubSync {
	http: reqwest::Client,
	auth: Authenticator,
	events: broadcast::Sender<CoderEventEnvelope>,
	workspaces_dir: Utf8PathBuf,
	coder_sessions_dir: Utf8PathBuf,
	/// One slot per `(workspace_id, session_id)`. Holds the
	/// debounce timer's cancel token so a follow-up enqueue can
	/// reset the window without leaking the previous task.
	pending: Arc<Mutex<HashMap<DebounceKey, CancellationToken>>>,
}

type DebounceKey = (String, String);

impl HubSync {
	pub fn new(
		auth: Authenticator,
		events: broadcast::Sender<CoderEventEnvelope>,
		workspaces_dir: Utf8PathBuf,
		coder_sessions_dir: Utf8PathBuf,
	) -> Result<Self, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(CoderError::from)?;
		Ok(Self {
			http,
			auth,
			events,
			workspaces_dir,
			coder_sessions_dir,
			pending: Arc::new(Mutex::new(HashMap::new())),
		})
	}

	/// Namespaces the signed-in user can create a bucket under:
	/// their own login plus every org they belong to. Built from
	/// the cached [`HfIdentity`] so the connect modal opens
	/// instantly — no extra `/api/whoami-v2` round-trip needed.
	/// Errors only if the cached identity is missing or stale
	/// beyond recovery (caller re-prompts the device flow).
	pub async fn list_namespaces(&self) -> Result<Vec<HubNamespace>, CoderError> {
		let identity = self.auth.identity().await?.ok_or(CoderError::NotSignedIn)?;
		let mut out = Vec::with_capacity(1 + identity.orgs.len());
		out.push(HubNamespace::User {
			name: identity.username.clone(),
		});
		for org in &identity.orgs {
			let slug = org.slug.clone().unwrap_or_else(|| org.name.clone());
			out.push(HubNamespace::Org { name: slug });
		}
		Ok(out)
	}

	/// Create the bucket on the Hub and write the per-workspace
	/// README in one shot, returning the bound
	/// [`CoderHubBucket`] the Tauri command persists onto
	/// `WorkspaceSession::coder_hub_bucket`. `409 Conflict` is
	/// treated as success: the bucket already exists (we created
	/// it before, or the user created an identically-named one
	/// out-of-band) and we just adopt it. Surfacing 409 as a
	/// hard error would force the user to pick a different name
	/// even when their `contribute-repos` OAuth scope already
	/// owns the existing one — and we can't distinguish "we own
	/// it" from "someone else owns it" without an extra
	/// permission probe, so the simplest behaviour is "adopt and
	/// move on; subsequent uploads either work or 403 cleanly".
	pub async fn create_bucket(
		&self,
		namespace: &str,
		name: &str,
		private: bool,
		workspace_basename: &str,
	) -> Result<CoderHubBucket, CoderError> {
		create_bucket_inner(&self.http, &self.auth, namespace, name, private).await?;
		let bucket = CoderHubBucket {
			namespace: namespace.to_string(),
			name: name.to_string(),
			private,
			autosync: false,
			uploaded: HashMap::new(),
		};
		self.write_readme(&bucket, workspace_basename).await?;
		Ok(bucket)
	}

	/// Compose + push the bucket's README. Called exactly once at
	/// connect time. The README content is intentionally short —
	/// the bucket exists to back the pi-mono trace viewer; the
	/// landing page is just enough context for a user who
	/// stumbles onto the bucket from search.
	pub async fn write_readme(&self, bucket: &CoderHubBucket, workspace_basename: &str) -> Result<(), CoderError> {
		let body = compose_readme(workspace_basename);
		upload_file(
			&self.http,
			&self.auth,
			&bucket.namespace,
			&bucket.name,
			"README.md",
			body.into_bytes(),
			README_CONTENT_TYPE,
		)
		.await
	}

	/// Push one session JSONL synchronously. Used by the manual
	/// "Upload to Hub" button (always available, regardless of
	/// `autosync`) and by the debounced autosync timer. Emits
	/// `HubSyncStarted` before the round-trip and
	/// `HubSyncFinished` after — the panel decoration keys off
	/// that pair, the runner doesn't need a second event channel.
	///
	/// On success the workspace's `coder_hub_bucket.uploaded` map
	/// is updated and persisted back to `session.json`, so a
	/// follow-up enqueue with the same local length is a no-op.
	pub async fn upload_session(
		&self,
		workspace_id: &str,
		folder_path: &Utf8Path,
		session_id: &str,
	) -> Result<(), CoderError> {
		self.emit(
			folder_path,
			CoderEvent::HubSyncStarted {
				session_id: session_id.to_string(),
			},
		);
		let result = self.upload_session_inner(workspace_id, folder_path, session_id).await;
		let (ok, error) = match &result {
			Ok(()) => (true, None),
			Err(err) => (false, Some(err.to_string())),
		};
		self.emit(
			folder_path,
			CoderEvent::HubSyncFinished {
				session_id: session_id.to_string(),
				ok,
				error,
			},
		);
		result
	}

	async fn upload_session_inner(
		&self,
		workspace_id: &str,
		folder_path: &Utf8Path,
		session_id: &str,
	) -> Result<(), CoderError> {
		sessions::validate_session_id(session_id)?;
		let mut workspace_session = core_session::load(&self.workspaces_dir, workspace_id).await?;
		let Some(bucket) = workspace_session.coder_hub_bucket.as_ref().cloned() else {
			return Err(CoderError::Internal(
				"no Hugging Face bucket connected for this workspace".into(),
			));
		};

		let path = self.resolve_session_path(folder_path, session_id).await?;
		let bytes = tokio::fs::read(path.as_std_path())
			.await
			.map_err(|err| CoderError::Internal(format!("could not read session jsonl {path}: {err}")))?;
		let len = bytes.len() as u64;

		if bucket
			.uploaded
			.get(session_id)
			.map(|marker| marker.bytes == len)
			.unwrap_or(false)
		{
			// Already pushed at this length — skip the round-trip
			// entirely. Xet would dedup the chunks anyway, but
			// avoiding the call also avoids burning a fresh
			// `xet-write-token`.
			tracing::debug!(
				workspace = workspace_id,
				session = session_id,
				"hub sync skipped (already at length)"
			);
			return Ok(());
		}

		let bucket_path = bucket_path_for_session(folder_path, session_id);

		upload_file(
			&self.http,
			&self.auth,
			&bucket.namespace,
			&bucket.name,
			&bucket_path,
			bytes,
			SESSION_CONTENT_TYPE,
		)
		.await?;

		if let Some(b) = workspace_session.coder_hub_bucket.as_mut() {
			b.uploaded.insert(
				session_id.to_string(),
				UploadedMarker {
					bytes: len,
					at_ms: now_ms(),
				},
			);
			core_session::save(&self.workspaces_dir, workspace_id, &workspace_session).await?;
		}
		Ok(())
	}

	/// Find the JSONL path for `session_id`. Handles both
	/// top-level sessions and sub-agent sessions (which live one
	/// level deeper under the parent's id).
	async fn resolve_session_path(&self, folder_path: &Utf8Path, session_id: &str) -> Result<Utf8PathBuf, CoderError> {
		let dir = sessions::sessions_dir(&self.coder_sessions_dir, folder_path);
		let direct = sessions::session_path(&dir, session_id);
		if tokio::fs::try_exists(direct.as_std_path()).await.unwrap_or(false) {
			return Ok(direct);
		}
		if let Some(found) = sessions::find_subagent_session(&dir, session_id).await {
			return Ok(found);
		}
		Err(CoderError::Internal(format!("session jsonl not on disk yet: {direct}")))
	}

	/// Enqueue an autosync push. Coalesces back-to-back calls for
	/// the same `(workspace_id, session_id)` within
	/// [`SYNC_DEBOUNCE`] — a new enqueue cancels the prior
	/// timer's task and starts a fresh one. The upload itself
	/// is fire-and-forget; the caller (a `TurnComplete` emit
	/// site) never blocks on it.
	pub fn enqueue_session_sync(&self, workspace_id: String, folder_path: Utf8PathBuf, session_id: String) {
		let key: DebounceKey = (workspace_id.clone(), session_id.clone());
		let cancel = CancellationToken::new();
		let cancel_for_task = cancel.clone();
		let pending = self.pending.clone();
		let this = self.clone();

		// Replace any existing slot under the same key; the prior
		// timer notices the cancellation and exits before it
		// fires.
		tokio::spawn(async move {
			{
				let mut guard = pending.lock().await;
				if let Some(prev) = guard.insert(key.clone(), cancel) {
					prev.cancel();
				}
			}
			let sleep = tokio::time::sleep(SYNC_DEBOUNCE);
			tokio::pin!(sleep);
			let fired = tokio::select! {
				() = &mut sleep => true,
				() = cancel_for_task.cancelled() => false,
			};
			if !fired {
				return;
			}
			{
				// Clear the slot before running so a brand-new
				// enqueue arriving during the upload starts a
				// fresh debounce rather than being dropped.
				let mut guard = pending.lock().await;
				guard.remove(&key);
			}
			if let Err(err) = this.upload_session(&workspace_id, &folder_path, &session_id).await {
				tracing::warn!(error = %err, "hub autosync upload failed");
			}
		});
	}

	fn emit(&self, folder_path: &Utf8Path, event: CoderEvent) {
		let _ = self.events.send(CoderEventEnvelope {
			folder: folder_path.as_str().to_string(),
			event,
		});
	}
}

/// `POST /api/buckets/<namespace>/<name>` body. Idempotent: 409
/// is mapped to `Ok(())` by the caller. The Hub returns 200 on
/// fresh creation with a payload we don't currently consume —
/// we only need the side-effect.
async fn create_bucket_inner(
	http: &reqwest::Client,
	auth: &Authenticator,
	namespace: &str,
	name: &str,
	private: bool,
) -> Result<(), CoderError> {
	let token = auth.current_access_token().await?;
	let endpoint = format!("{HF_HUB_BASE}/api/buckets/{namespace}/{name}");
	let response = http
		.post(&endpoint)
		.bearer_auth(token)
		.json(&serde_json::json!({ "private": private }))
		.send()
		.await
		.map_err(CoderError::from)?;
	let status = response.status();
	if status.is_success() || status.as_u16() == 409 {
		return Ok(());
	}
	let request_id = request_id_of(&response);
	let body = response.text().await.unwrap_or_default();
	Err(CoderError::http(endpoint, status.as_u16(), body, request_id))
}

/// Upload `bytes` to `<namespace>/<name>/<path_in_bucket>`.
///
/// Three-step dance:
/// 1. `GET /api/buckets/<ns>/<name>/xet-write-token` for a
///    short-lived CAS upload token + the Xet endpoint URL.
/// 2. Build a Xet upload commit with that token, push the bytes,
///    `commit()` to land them in CAS, harvest the resulting
///    Merkle hash off [`XetFileInfo::hash`].
/// 3. `POST /api/buckets/<ns>/<name>/batch` an NDJSON
///    `addFile` entry that binds the hash at `path_in_bucket`.
///
/// The upload is async-friendly: the Xet client's `build()` /
/// `commit()` async paths return immediately to the executor
/// when the CAS chunks are in flight, so a turn's autosync
/// doesn't pin a tokio worker.
async fn upload_file(
	http: &reqwest::Client,
	auth: &Authenticator,
	namespace: &str,
	name: &str,
	path_in_bucket: &str,
	bytes: Vec<u8>,
	content_type: &str,
) -> Result<(), CoderError> {
	let token = fetch_xet_write_token(http, auth, namespace, name).await?;
	let hash = xet_upload_bytes(&token, path_in_bucket, bytes).await?;
	post_add_file(http, auth, namespace, name, path_in_bucket, &hash, content_type).await
}

#[derive(Debug, Deserialize)]
struct XetWriteToken {
	#[serde(rename = "casUrl")]
	cas_url: String,
	#[serde(rename = "accessToken")]
	access_token: String,
	/// Wall-clock seconds-since-epoch the `access_token` stops
	/// being valid. Per-token, returned alongside the token by
	/// the Hub.
	exp: u64,
}

async fn fetch_xet_write_token(
	http: &reqwest::Client,
	auth: &Authenticator,
	namespace: &str,
	name: &str,
) -> Result<XetWriteToken, CoderError> {
	let token = auth.current_access_token().await?;
	let endpoint = format!("{HF_HUB_BASE}/api/buckets/{namespace}/{name}/xet-write-token");
	let response = http
		.get(&endpoint)
		.bearer_auth(token)
		.send()
		.await
		.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}
	serde_json::from_str::<XetWriteToken>(&body).map_err(|err| CoderError::decode(endpoint, err.to_string()))
}

/// Push `bytes` into Xet CAS and return the resulting Merkle
/// hash. The session itself is created fresh per upload — the
/// Hub's write tokens are scoped per-bucket, and creating a
/// session is cheap (it's just a config builder; no network
/// round-trip).
async fn xet_upload_bytes(token: &XetWriteToken, tracking_name: &str, bytes: Vec<u8>) -> Result<String, CoderError> {
	use xet::xet_session::{Sha256Policy, XetSessionBuilder};

	let session = XetSessionBuilder::new()
		.build()
		.map_err(|err| CoderError::Internal(format!("xet session build failed: {err}")))?;
	let commit = session
		.new_upload_commit()
		.map_err(|err| CoderError::Internal(format!("xet upload commit init failed: {err}")))?
		.with_endpoint(token.cas_url.clone())
		.with_token_info(token.access_token.clone(), token.exp)
		.build()
		.await
		.map_err(|err| CoderError::Internal(format!("xet upload commit build failed: {err}")))?;
	let handle = commit
		.upload_bytes(bytes, Sha256Policy::Compute, Some(tracking_name.to_string()))
		.await
		.map_err(|err| CoderError::Internal(format!("xet upload_bytes failed: {err}")))?;
	let meta = handle
		.finalize_ingestion()
		.await
		.map_err(|err| CoderError::Internal(format!("xet finalize_ingestion failed: {err}")))?;
	commit
		.commit()
		.await
		.map_err(|err| CoderError::Internal(format!("xet commit failed: {err}")))?;
	Ok(meta.xet_info.hash().to_string())
}

/// `POST /api/buckets/<ns>/<name>/batch` with a single-line
/// NDJSON body binding `xet_hash` at `path_in_bucket`. The
/// endpoint accepts a stream of `addFile` / `copyFile` /
/// `deleteFile` lines; we only ever push one at a time because
/// each file's CAS upload is independent.
async fn post_add_file(
	http: &reqwest::Client,
	auth: &Authenticator,
	namespace: &str,
	name: &str,
	path_in_bucket: &str,
	xet_hash: &str,
	content_type: &str,
) -> Result<(), CoderError> {
	let token = auth.current_access_token().await?;
	let endpoint = format!("{HF_HUB_BASE}/api/buckets/{namespace}/{name}/batch");
	let entry = serde_json::json!({
		"type": "addFile",
		"path": path_in_bucket,
		"xetHash": xet_hash,
		"mtime": now_ms(),
		"contentType": content_type,
	});
	let mut body =
		serde_json::to_string(&entry).map_err(|err| CoderError::Internal(format!("ndjson encode failed: {err}")))?;
	body.push('\n');
	let response = http
		.post(&endpoint)
		.bearer_auth(token)
		.header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
		.body(body)
		.send()
		.await
		.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let response_body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), response_body, request_id));
	}
	// The batch endpoint always returns a `BATCH_RESPONSE`
	// shape with per-entry failures even on 200. Surface the
	// first entry's error if the single push didn't land — the
	// Hub returns 200 OK with `success: false` rather than a
	// 4xx in that case, so a raw status check isn't enough.
	let parsed: BatchResponse =
		serde_json::from_str(&response_body).map_err(|err| CoderError::decode(endpoint.clone(), err.to_string()))?;
	if !parsed.success {
		let reason = parsed
			.failed
			.into_iter()
			.next()
			.map(|f| format!("{}: {}", f.path, f.error))
			.unwrap_or_else(|| "batch addFile failed (no detail)".to_string());
		return Err(CoderError::http(endpoint, status.as_u16(), reason, request_id));
	}
	Ok(())
}

#[derive(Debug, Deserialize)]
struct BatchResponse {
	success: bool,
	#[serde(default)]
	failed: Vec<BatchFailure>,
}

#[derive(Debug, Deserialize)]
struct BatchFailure {
	path: String,
	error: String,
}

fn bucket_path_for_session(folder_path: &Utf8Path, session_id: &str) -> String {
	// Mirror the on-disk layout: a workspace can hold many
	// folders, so we group traces by folder slug rather than
	// piling every JSONL at the bucket root. The slug is the
	// same `<basename>-<fnv8>` we use under
	// `coder-sessions/`, so local↔Hub paths line up 1:1. The
	// `sessions/` directory the older draft put inside the
	// bucket is gone — the bucket itself is single-purpose and
	// already named `*-traces`, so the extra level was just
	// noise. Sub-agent ids carry their `sub-` prefix verbatim
	// and live alongside their parents inside the same folder
	// slug — the JSONL header is what binds them to a parent.
	let slug = sessions::project_slug(folder_path);
	format!("{slug}/{session_id}.jsonl")
}

fn compose_readme(workspace_basename: &str) -> String {
	let today = today_iso();
	format!(
		"# moon-ide traces — {basename}\n\
		\n\
		This bucket stores coder session traces from moon-ide for the workspace `{basename}`.\n\
		\n\
		Each folder bound to the workspace gets its own directory at the bucket root (`<folder-slug>/`), holding one JSONL per coder session in [pi-mono](https://github.com/badlogic/pi-mono) trace shape. Hugging Face renders each file inline through the Hub's pi trace viewer.\n\
		\n\
		Generated by moon-ide on {today}.\n",
		basename = workspace_basename,
		today = today,
	)
}

fn today_iso() -> String {
	// `YYYY-MM-DD` in UTC. We don't pull `chrono` for this — one
	// helper does the formatting from the unix timestamp.
	let secs = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0) as i64;
	format_ymd_utc(secs)
}

fn format_ymd_utc(unix_secs: i64) -> String {
	// Algorithm taken from Howard Hinnant's "date" — converts a
	// signed seconds-since-epoch to civil (Y, M, D) without
	// pulling chrono. Good enough for a README date stamp.
	let days = unix_secs.div_euclid(86_400);
	let z = days + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let doe = z - era * 146_097;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let year = y + i64::from(m <= 2);
	format!("{year:04}-{m:02}-{d:02}")
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn bucket_path_groups_by_folder_slug() {
		let folder = Utf8PathBuf::from("/home/me/work");
		let slug = sessions::project_slug(&folder);
		assert_eq!(
			bucket_path_for_session(&folder, "sess-12345"),
			format!("{slug}/sess-12345.jsonl")
		);
		assert_eq!(
			bucket_path_for_session(&folder, "sub-abc-12345"),
			format!("{slug}/sub-abc-12345.jsonl")
		);
	}

	#[test]
	fn bucket_paths_for_different_folders_disambiguate() {
		// Two folders with the same basename but different
		// absolute paths land under distinct slugs in the
		// bucket — same fence the local layout uses.
		let a = Utf8PathBuf::from("/home/me/code/moon-ide");
		let b = Utf8PathBuf::from("/srv/projects/moon-ide");
		let pa = bucket_path_for_session(&a, "sess-1");
		let pb = bucket_path_for_session(&b, "sess-1");
		assert_ne!(pa, pb);
	}

	#[test]
	fn readme_mentions_workspace_and_pi() {
		let text = compose_readme("powergrid");
		assert!(text.contains("powergrid"));
		assert!(text.contains("pi-mono"));
		assert!(text.contains("folder-slug"));
	}

	#[test]
	fn format_ymd_known_dates() {
		// 2024-01-01 00:00:00 UTC
		assert_eq!(format_ymd_utc(1_704_067_200), "2024-01-01");
		// 1970-01-01
		assert_eq!(format_ymd_utc(0), "1970-01-01");
		// 2000-02-29 (leap day)
		assert_eq!(format_ymd_utc(951_782_400), "2000-02-29");
	}
}
