//! Per-folder one-line descriptions used to seed the parent agent's
//! "Bound folders" system-prompt section.
//!
//! The parent stays single-folder for tools (today's invariant), but
//! sub-agents (Phase C of the multi-project plan) can target any
//! bound folder. The model can't intelligently delegate without
//! knowing what each folder *is*, so this module produces and caches
//! a 2–3 sentence description per folder.
//!
//! Generation strategy: read a small bundle of metadata files from
//! the folder root (`README.md`, `Cargo.toml`, `package.json`,
//! `pyproject.toml`), feed the concatenated bytes to the **fast**
//! model, persist the answer at
//! `<XDG_DATA_HOME>/moon-ide/folder-summaries/<slug>.json` keyed by
//! a 64-bit FNV-1a of the inputs. Cache hit when the inputs
//! signature still matches; cache miss whenever any of the source
//! manifests changed (or the user dropped a new one in).
//!
//! The service is fail-soft: a missing folder, a model error, or a
//! corrupt cache file all produce a "no summary available" outcome
//! that the prompt renderer treats as the
//! "(summary still generating)" placeholder. We never block a turn
//! waiting for a summary; if the cache is cold, this turn ships
//! without descriptions and the next turn picks up whichever
//! summaries finished in the meantime.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::event::{CoderEvent, CoderEventEnvelope};
use crate::inference::{ChatMessage, InferenceClient};
use crate::sessions::{current_time_ms, project_slug};
use tokio::sync::broadcast;

/// Canonical manifest filenames we look for in the folder root,
/// in **read order**. AGENTS.md leads because it's literally
/// authored for agents — when both AGENTS.md and a README exist,
/// the agent guidance should anchor the prompt before the user-
/// facing prose. Then the README and the language manifests so
/// the model can infer the stack from `Cargo.toml` /
/// `package.json` / `pyproject.toml`.
///
/// Casing handling is **not** done by enumerating every spelling
/// here. We list the folder root once and match each canonical
/// name against directory entries case-insensitively, so
/// `AGENTS.MD` / `agents.md` / `Readme.md` / `README.MD` all
/// resolve to the canonical entry without a hardcoded variant
/// list. Anything outside the canonical-spelling-with-extension
/// convention (`README` no extension, `Readme.txt`, etc.) is
/// intentionally ignored — those are outside our scope.
const CANONICAL_MANIFEST_NAMES: &[&str] = &[
	"AGENTS.md",
	"CLAUDE.md",
	"README.md",
	"Cargo.toml",
	"package.json",
	"pyproject.toml",
];

/// Per-file byte cap when concatenating inputs. Keeps the prompt
/// size bounded — a 2 MB README would dominate the model's context
/// without adding signal.
const PER_FILE_CAP: usize = 5_000;

/// Aggregate cap on the concatenated input blob handed to the
/// model. The fast model has plenty of context budget but we don't
/// want to spend it on every bound folder's full README.
const TOTAL_INPUT_CAP: usize = 20_000;

/// Maximum length of the description we accept from the model. The
/// prompt explicitly asks for 2–3 sentences but a malicious /
/// confused model might dump prose; clamp before persisting.
const MAX_DESCRIPTION_BYTES: usize = 800;

/// System prompt for the summarising call. Tells the fast model
/// what we want and what we *don't* want (no markdown, no code,
/// no fluff).
const SUMMARY_SYSTEM_PROMPT: &str = "You write short, factual descriptions of software projects. Read the project's README and manifest files (Cargo.toml, package.json, pyproject.toml) and reply with 2 to 3 plain sentences describing what the project is and what stack it uses. No markdown, no code, no preamble like \"This project is\". Aim for under 80 words. If the inputs are inconclusive, say so honestly in one sentence.";

/// Cache record persisted to disk. The frontend doesn't read this
/// file directly — the runner serves it via Tauri commands — so we
/// keep the schema internal and out of the moon-protocol crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderSummary {
	/// 2–3 sentence plaintext description.
	pub description: String,
	/// 16-char hex of `fnv1a64(concatenated inputs)`. Cache is
	/// considered stale when the recomputed signature differs.
	pub inputs_signature: String,
	/// Wall-clock when the description was generated. Used by tests
	/// and by future "regenerate if older than N days" affordances;
	/// not relied on for invalidation today. Matches the `i64`
	/// shape the rest of moon-coder uses for timestamps (see
	/// [`crate::sessions::current_time_ms`]).
	pub generated_at_ms: i64,
}

/// Owner of the on-disk cache + the in-flight dedupe set. One per
/// `Coder` instance; cheap to clone via `Arc`.
pub struct FolderSummaryService {
	cache_root: Utf8PathBuf,
	inflight: Mutex<HashSet<String>>,
}

impl FolderSummaryService {
	pub fn new(cache_root: Utf8PathBuf) -> Self {
		Self {
			cache_root,
			inflight: Mutex::new(HashSet::new()),
		}
	}

	/// Return the cached summary for `folder_root` *iff* the cache
	/// file exists, deserialises, and its signature still matches
	/// the current on-disk inputs. Any failure mode returns `None`
	/// — callers treat that as "summary not ready yet".
	pub async fn cached(&self, folder_root: &Utf8Path) -> Option<FolderSummary> {
		let path = self.cache_path(folder_root);
		let bytes = fs::read(path.as_std_path()).await.ok()?;
		let cached: FolderSummary = serde_json::from_slice(&bytes).ok()?;
		let (_, sig_now) = read_inputs(folder_root).await;
		if cached.inputs_signature == sig_now {
			Some(cached)
		} else {
			None
		}
	}

	/// Schedule a background regeneration for `folder_root`'s
	/// summary if one isn't already in flight. Idempotent — repeat
	/// calls during the same generation window are no-ops. Errors
	/// inside the spawned task log at debug level and are
	/// otherwise swallowed; the next caller will retry on demand.
	/// On success, fans a `FolderSummaryReady` event out through
	/// `events` so the UI can update without polling.
	///
	/// The envelope's `folder` field carries `folder_root` —
	/// `FolderSummaryReady` is a "this folder's description
	/// updated" signal, not a per-session event, so it's
	/// self-tagged with the target folder regardless of which
	/// session triggered the regeneration. The frontend
	/// dispatcher routes this event kind to a global cache update
	/// rather than a per-folder bucket.
	pub fn spawn_regenerate(
		self: &Arc<Self>,
		folder_root: Utf8PathBuf,
		inference: InferenceClient,
		cheap_model: String,
		events: broadcast::Sender<CoderEventEnvelope>,
		cancel: CancellationToken,
	) {
		let slug = project_slug(folder_root.as_path());
		let this = self.clone();
		tokio::spawn(async move {
			{
				let mut inflight = this.inflight.lock().await;
				if !inflight.insert(slug.clone()) {
					return;
				}
			}
			let outcome = generate_and_cache(&this.cache_root, &folder_root, &inference, &cheap_model, &cancel).await;
			match outcome {
				Ok(summary) => {
					tracing::debug!(slug, "folder summary refreshed");
					let _ = events.send(CoderEventEnvelope {
						folder: folder_root.to_string(),
						event: CoderEvent::FolderSummaryReady {
							folder: folder_root.to_string(),
							description: summary.description,
						},
					});
				}
				Err(err) => {
					tracing::debug!(slug, error = %err, "folder summary generation failed");
				}
			}
			let mut inflight = this.inflight.lock().await;
			inflight.remove(&slug);
		});
	}

	pub fn cache_root(&self) -> &Utf8Path {
		&self.cache_root
	}

	fn cache_path(&self, folder_root: &Utf8Path) -> Utf8PathBuf {
		let slug = project_slug(folder_root);
		self.cache_root.join(format!("{slug}.json"))
	}
}

async fn generate_and_cache(
	cache_root: &Utf8Path,
	folder_root: &Utf8Path,
	inference: &InferenceClient,
	cheap_model: &str,
	cancel: &CancellationToken,
) -> Result<FolderSummary, String> {
	let (input_text, sig) = read_inputs(folder_root).await;
	if input_text.trim().is_empty() {
		// No README / manifest at all — best we can do is the
		// folder's basename; cache that so we don't re-spam the
		// model on every turn for an empty checkout.
		let basename = folder_root.file_name().unwrap_or("workspace").to_owned();
		let summary = FolderSummary {
			description: format!("No README or manifest found in {basename}. Likely a fresh checkout or scratch directory."),
			inputs_signature: sig,
			generated_at_ms: current_time_ms(),
		};
		write_cache(cache_root, folder_root, &summary)
			.await
			.map_err(|e| e.to_string())?;
		return Ok(summary);
	}
	let basename = folder_root.file_name().unwrap_or("workspace").to_owned();
	let user_prompt = format!(
		"Project folder name: {basename}\n\nManifest excerpts (truncated to {} bytes total):\n\n{}",
		TOTAL_INPUT_CAP, input_text,
	);
	let messages = vec![
		ChatMessage::System {
			content: SUMMARY_SYSTEM_PROMPT.to_string(),
		},
		ChatMessage::user(user_prompt),
	];
	let response = inference
		.chat_completion(cheap_model, &messages, &[], cancel)
		.await
		.map_err(|err| err.to_string())?;
	let raw = response.content.unwrap_or_default();
	let description = sanitise_description(&raw);
	if description.is_empty() {
		return Err("empty description from model".into());
	}
	let summary = FolderSummary {
		description,
		inputs_signature: sig,
		generated_at_ms: current_time_ms(),
	};
	write_cache(cache_root, folder_root, &summary)
		.await
		.map_err(|e| e.to_string())?;
	Ok(summary)
}

async fn write_cache(cache_root: &Utf8Path, folder_root: &Utf8Path, summary: &FolderSummary) -> std::io::Result<()> {
	fs::create_dir_all(cache_root.as_std_path()).await?;
	let path = cache_root.join(format!("{}.json", project_slug(folder_root)));
	let bytes = serde_json::to_vec_pretty(summary).map_err(std::io::Error::other)?;
	fs::write(path.as_std_path(), bytes).await
}

/// Read up to `PER_FILE_CAP` bytes from each known manifest in
/// `folder_root` (capping the aggregate at `TOTAL_INPUT_CAP`),
/// concatenated with file-name banners and a stable separator.
/// Returns `(bundle, signature)` where signature is a 16-char hex
/// FNV-1a of the bundle bytes — so two calls return matching
/// signatures iff the same bytes are still on disk.
///
/// Canonical names are matched case-insensitively against the
/// folder's top-level directory listing, so `AGENTS.MD` and
/// `Readme.md` resolve to the canonical entries without a
/// hardcoded casing list. The actual on-disk casing is preserved
/// in the banner so the prompt's "---- README.md ----" header
/// reflects what the user actually has.
async fn read_inputs(folder_root: &Utf8Path) -> (String, String) {
	// Single top-level dir read; index by lowercased basename so
	// each canonical name is a single map lookup. Skipping the
	// directory entirely (folder doesn't exist, no permission)
	// returns an empty bundle — same outcome as "no recognised
	// manifests", and the cache key is stable so we don't loop on
	// generation for an unreadable folder.
	let mut by_lower: HashMap<String, std::path::PathBuf> = HashMap::new();
	if let Ok(mut iter) = fs::read_dir(folder_root.as_std_path()).await {
		while let Ok(Some(entry)) = iter.next_entry().await {
			let file_name = entry.file_name();
			let Some(name_str) = file_name.to_str() else {
				continue;
			};
			by_lower.insert(name_str.to_lowercase(), entry.path());
		}
	}

	let mut out = String::new();
	for canonical in CANONICAL_MANIFEST_NAMES {
		if out.len() >= TOTAL_INPUT_CAP {
			break;
		}
		let Some(actual_path) = by_lower.get(&canonical.to_lowercase()) else {
			continue;
		};
		let bytes = match fs::read(actual_path).await {
			Ok(b) => b,
			Err(_) => continue,
		};
		let truncated = bytes.len() > PER_FILE_CAP;
		let slice = if truncated { &bytes[..PER_FILE_CAP] } else { &bytes[..] };
		// Lossy is fine — these are human-edited config files; any
		// bad bytes are an authoring bug and the model can cope.
		let text = String::from_utf8_lossy(slice);
		// Preserve the on-disk casing in the banner — `Readme.md`
		// stays `Readme.md`, the canonical name is just our
		// lookup key.
		let banner = actual_path.file_name().and_then(|n| n.to_str()).unwrap_or(canonical);
		out.push_str("---- ");
		out.push_str(banner);
		if truncated {
			out.push_str(" (truncated)");
		}
		out.push_str(" ----\n");
		out.push_str(&text);
		if !text.ends_with('\n') {
			out.push('\n');
		}
		if out.len() >= TOTAL_INPUT_CAP {
			out.truncate(TOTAL_INPUT_CAP);
			break;
		}
	}
	let sig = fnv1a64_hex(out.as_bytes());
	(out, sig)
}

/// Trim model output: collapse whitespace runs, drop common
/// "This project is …" preamble fragments, enforce the byte cap.
/// Conservative — we'd rather store a slightly verbose answer than
/// reject every borderline reply.
fn sanitise_description(raw: &str) -> String {
	let mut text = raw.trim().trim_matches('"').to_string();
	// Some providers wrap the answer in "Here is the description:"
	// scaffolding. Strip it if the *first* line is obviously meta.
	if let Some((first, rest)) = text.split_once('\n') {
		let lower = first.trim().to_lowercase();
		if lower.ends_with(':') && (lower.contains("description") || lower.contains("summary")) {
			text = rest.trim().to_string();
		}
	}
	if text.len() > MAX_DESCRIPTION_BYTES {
		// Truncate at a char boundary so the cached JSON isn't
		// surprising to load.
		let mut idx = MAX_DESCRIPTION_BYTES;
		while idx > 0 && !text.is_char_boundary(idx) {
			idx -= 1;
		}
		text.truncate(idx);
		text.push('…');
	}
	text
}

fn fnv1a64_hex(bytes: &[u8]) -> String {
	const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
	const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
	let mut h: u64 = FNV_OFFSET;
	for b in bytes {
		h ^= u64::from(*b);
		h = h.wrapping_mul(FNV_PRIME);
	}
	format!("{h:016x}")
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempfile::TempDir;

	#[tokio::test]
	async fn read_inputs_returns_empty_for_empty_folder() {
		let dir = TempDir::new().unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let (bundle, sig) = read_inputs(&root).await;
		assert_eq!(bundle, "");
		// Empty-input signature is stable so a cache hit on
		// "still empty" doesn't re-trigger generation forever.
		assert_eq!(sig, fnv1a64_hex(b""));
	}

	#[tokio::test]
	async fn read_inputs_concatenates_known_manifests_with_banners() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("README.md"), "# My project\nDoes things.\n").unwrap();
		std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"thing\"\n").unwrap();

		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let (bundle, _) = read_inputs(&root).await;
		assert!(bundle.contains("---- README.md ----"));
		assert!(bundle.contains("---- Cargo.toml ----"));
		assert!(bundle.contains("Does things."));
		assert!(bundle.contains("name = \"thing\""));
	}

	#[tokio::test]
	async fn read_inputs_puts_agents_md_before_readme() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("AGENTS.md"), "agent rules go here\n").unwrap();
		std::fs::write(dir.path().join("README.md"), "user-facing prose\n").unwrap();

		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let (bundle, _) = read_inputs(&root).await;
		// AGENTS.md is more authoritative for the model and should
		// appear before README in the concatenated bundle.
		let agents_at = bundle.find("---- AGENTS.md ----").expect("AGENTS.md missing");
		let readme_at = bundle.find("---- README.md ----").expect("README.md missing");
		assert!(
			agents_at < readme_at,
			"AGENTS.md should precede README.md in the bundle"
		);
	}

	#[tokio::test]
	async fn read_inputs_matches_canonical_names_case_insensitively() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("Readme.md"), "# uppercase R\n").unwrap();
		std::fs::write(dir.path().join("Agents.md"), "# title-case Agents\n").unwrap();

		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let (bundle, _) = read_inputs(&root).await;
		// Banner preserves on-disk casing, but lookup matched the
		// canonical entry regardless of how the file is spelled.
		assert!(bundle.contains("---- Readme.md ----"));
		assert!(bundle.contains("---- Agents.md ----"));
	}

	#[tokio::test]
	async fn read_inputs_signature_changes_when_manifest_changes() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join("README.md"), "v1\n").unwrap();
		let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
		let (_, sig_v1) = read_inputs(&root).await;

		std::fs::write(dir.path().join("README.md"), "v2 — added more\n").unwrap();
		let (_, sig_v2) = read_inputs(&root).await;

		assert_ne!(sig_v1, sig_v2);
	}

	#[tokio::test]
	async fn cache_hit_returns_cached_summary_only_when_signature_matches() {
		let cache_dir = TempDir::new().unwrap();
		let folder_dir = TempDir::new().unwrap();
		std::fs::write(folder_dir.path().join("README.md"), "v1\n").unwrap();
		let cache_root = Utf8PathBuf::from_path_buf(cache_dir.path().to_path_buf()).unwrap();
		let folder_root = Utf8PathBuf::from_path_buf(folder_dir.path().to_path_buf()).unwrap();

		let svc = FolderSummaryService::new(cache_root.clone());
		let (_, sig) = read_inputs(&folder_root).await;
		let summary = FolderSummary {
			description: "A test project".into(),
			inputs_signature: sig.clone(),
			generated_at_ms: 0,
		};
		write_cache(&cache_root, &folder_root, &summary).await.unwrap();

		assert_eq!(svc.cached(&folder_root).await, Some(summary));

		// Mutate the manifest — cache should now miss.
		std::fs::write(folder_dir.path().join("README.md"), "v2\n").unwrap();
		assert!(svc.cached(&folder_root).await.is_none());
	}

	#[test]
	fn sanitise_description_strips_meta_preamble_lines() {
		let raw = "Here is the description:\nA Tauri-based IDE.";
		assert_eq!(sanitise_description(raw), "A Tauri-based IDE.");
	}

	#[test]
	fn sanitise_description_caps_at_max_bytes() {
		let raw = "x".repeat(MAX_DESCRIPTION_BYTES + 100);
		let out = sanitise_description(&raw);
		// "…" is 3 bytes in UTF-8; the prefix sits at the cap.
		assert!(out.len() <= MAX_DESCRIPTION_BYTES + 3);
		assert!(out.ends_with('…'));
	}
}
