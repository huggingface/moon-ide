//! User-added OpenAI-compatible providers — OpenRouter, locally
//! hosted vLLM / Ollama / llama.cpp, …
//!
//! HF (`router.huggingface.co/v1`) stays the implicit default
//! handled by [`crate::auth::Authenticator`] + the OAuth path in
//! [`crate::inference::InferenceClient`]. Anything else lives here:
//!
//! - [`ProviderKeyStore`] manages per-provider keyring entries
//!   under `service=moon-ide, account=coder-provider:<id>`. Empty /
//!   missing entries mean "no auth" (the local-llama.cpp case).
//! - [`ProviderKeyring`] caches the keys in memory so the hot path
//!   (every chat-completions request) doesn't touch the OS
//!   keyring; same shape as
//!   [`crate::web::WebClient`]'s Tavily cache.
//! - [`probe_provider`] verifies a `(base_url, api_key)` pair
//!   before saving: tries `GET <base_url>/models` first, falls
//!   back to a 1-token `chat/completions` ping if that 404s. Used
//!   by the `Add provider` modal's verify gesture.
//! - [`new_provider_id`] generates the opaque ids the picker
//!   addresses providers by. Short, random, URL-safe.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moon_protocol::coder_models::ProviderProbeResult;
use serde::Deserialize;

use crate::error::CoderError;

const KEYRING_SERVICE: &str = "moon-ide";
const KEYRING_ACCOUNT_PREFIX: &str = "coder-provider:";

/// Hard cap on how long probes block. Long enough for a sluggish
/// remote (OpenRouter from a slow link) and short enough that a
/// dead local server fails fast — Ollama on a stopped daemon
/// rejects the TCP connect immediately, while a typo'd hostname
/// would hang on DNS. 15 s leaves comfortable headroom for the
/// former without bothering the user about the latter.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// How many model ids the probe returns to the picker. Anything
/// more clutters the verify-result blurb without telling the user
/// much new. The full catalog comes from `coder_list_models` once
/// the provider is saved.
const PROBE_SAMPLE_LIMIT: usize = 5;

/// Owns the keyring entries for every user-added provider's API
/// key. Stateless — held by [`ProviderKeyring`] so the prefix
/// constant lives in one place.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderKeyStore;

impl ProviderKeyStore {
	pub const fn new() -> Self {
		Self
	}

	fn entry(&self, id: &str) -> Result<keyring::Entry, CoderError> {
		let account = format!("{KEYRING_ACCOUNT_PREFIX}{id}");
		keyring::Entry::new(KEYRING_SERVICE, &account).map_err(CoderError::from)
	}

	pub fn load(&self, id: &str) -> Result<Option<String>, CoderError> {
		match self.entry(id)?.get_password() {
			Ok(s) if s.trim().is_empty() => Ok(None),
			Ok(s) => Ok(Some(s)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(err) => Err(err.into()),
		}
	}

	pub fn save(&self, id: &str, key: &str) -> Result<(), CoderError> {
		self.entry(id)?.set_password(key)?;
		Ok(())
	}

	pub fn clear(&self, id: &str) -> Result<(), CoderError> {
		match self.entry(id)?.delete_credential() {
			Ok(()) => Ok(()),
			Err(keyring::Error::NoEntry) => Ok(()),
			Err(err) => Err(err.into()),
		}
	}
}

/// HTTP + in-memory key cache shared across the runner. Cheap to
/// clone (`Arc` inside). Single source of truth for "what's the
/// current key for provider X?" — the [`InferenceClient`] reads
/// through it on every non-HF request.
///
/// [`InferenceClient`]: crate::inference::InferenceClient
#[derive(Clone)]
pub struct ProviderKeyring {
	store: ProviderKeyStore,
	cache: Arc<RwLock<HashMap<String, String>>>,
}

impl ProviderKeyring {
	/// Build an empty keyring. Use [`Self::warm`] to lazily fill
	/// the cache from the keyring entries for the provider ids
	/// the user persisted across sessions — typically called once
	/// at coder startup with the `AppState`-loaded providers list.
	pub fn new() -> Self {
		Self {
			store: ProviderKeyStore::new(),
			cache: Arc::new(RwLock::new(HashMap::new())),
		}
	}

	/// Pre-populate the cache from the keyring for every provider
	/// id in `ids`. Missing entries are silently treated as
	/// "no key" (the local-llama.cpp case). Keyring failures are
	/// logged but don't fail startup — the user just sees the
	/// provider as keyless and can re-enter the key from the
	/// picker.
	pub fn warm(&self, ids: impl IntoIterator<Item = String>) {
		let Ok(mut cache) = self.cache.write() else {
			tracing::warn!("provider keyring cache poisoned; skipping warm");
			return;
		};
		for id in ids {
			match self.store.load(&id) {
				Ok(Some(key)) => {
					cache.insert(id, key);
				}
				Ok(None) => {}
				Err(err) => {
					tracing::warn!(error = %err, provider = %id, "could not load provider key from keyring");
				}
			}
		}
	}

	/// `true` iff the keyring currently holds a non-empty key for
	/// `id`. Read off the in-memory cache — the
	/// [`crate::inference`] hot path can call this every request
	/// without an OS round-trip.
	pub fn has_key(&self, id: &str) -> bool {
		self.cache.read().map(|g| g.contains_key(id)).unwrap_or(false)
	}

	/// Snapshot the key for `id`. Returns `None` for "no key
	/// configured" (the local-no-auth case); the inference layer
	/// suppresses the `Authorization` header in that case.
	pub fn get(&self, id: &str) -> Option<String> {
		self.cache.read().ok().and_then(|g| g.get(id).cloned())
	}

	/// Persist a new API key. Empty / whitespace-only keys are
	/// rejected — same trap as Tavily's: a silently-empty entry
	/// would make `has_key()` true but every downstream call
	/// 401, with the error pointing at the request rather than
	/// the missing key.
	pub fn set(&self, id: &str, key: &str) -> Result<(), CoderError> {
		let trimmed = key.trim();
		if trimmed.is_empty() {
			return Err(CoderError::invalid_args(
				"coder_set_provider_api_key",
				"api key must not be empty",
			));
		}
		self.store.save(id, trimmed)?;
		if let Ok(mut cache) = self.cache.write() {
			cache.insert(id.to_owned(), trimmed.to_owned());
		}
		Ok(())
	}

	/// Drop the keyring entry + cache slot. Idempotent.
	pub fn clear(&self, id: &str) -> Result<(), CoderError> {
		self.store.clear(id)?;
		if let Ok(mut cache) = self.cache.write() {
			cache.remove(id);
		}
		Ok(())
	}
}

impl Default for ProviderKeyring {
	fn default() -> Self {
		Self::new()
	}
}

/// Opaque id for a new provider entry. `prov-<unix-ms>-<nanos>`
/// follows the same shape [`crate::sessions::new_session_id`] uses
/// — short enough not to crowd the keyring inspector, long enough
/// to keep collisions implausible inside one millisecond. URL-safe
/// so future per-provider callback paths (Anthropic OAuth lands
/// here later) don't have to escape it.
pub fn new_provider_id() -> String {
	let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
	let ms = now.as_millis() as u64;
	let nanos = now.subsec_nanos();
	// Xor-shift the nanos so two calls in the same millisecond
	// don't land on adjacent suffixes (the timestamp already
	// covers most collisions; this is belt-and-braces).
	let mut x = nanos ^ 0x9e37_79b1;
	x ^= x << 13;
	x ^= x >> 17;
	x ^= x << 5;
	format!("prov-{ms:013}-{x:08x}")
}

/// Verify a `(base_url, api_key)` pair against the provider before
/// the picker commits it.
///
/// Order of attempts:
///
/// 1. `GET <base_url>/models` — the canonical OpenAI-compat
///    catalog endpoint. OpenRouter, LiteLLM, vLLM, Ollama, and
///    llama.cpp all expose it. Returns a `ProviderProbeResult`
///    with the model count and a few sample ids the picker can
///    show inline.
/// 2. If step 1 returns 404 (a few minimal servers skip
///    `/models`), fall through to a 1-token `POST /chat/completions`
///    ping with model `"probe"`. Most servers will respond with a
///    "no such model" error which still counts as "the endpoint
///    is reachable + auth is valid" — we accept any
///    non-auth-failure status.
///
/// `api_key` empty = no `Authorization` header sent. The probe
/// fails fast on 401 / 403 so a key typo is loud at save time.
pub async fn probe_provider(
	http: &reqwest::Client,
	base_url: &str,
	api_key: Option<&str>,
) -> Result<ProviderProbeResult, CoderError> {
	let trimmed = base_url.trim_end_matches('/');
	if trimmed.is_empty() {
		return Err(CoderError::invalid_args(
			"coder_probe_provider",
			"base_url must not be empty",
		));
	}

	match probe_models(http, trimmed, api_key).await {
		Ok(result) => Ok(result),
		Err(CoderError::Http { status: 404, .. }) => probe_chat_ping(http, trimmed, api_key).await,
		Err(err) => Err(err),
	}
}

async fn probe_models(
	http: &reqwest::Client,
	base_url: &str,
	api_key: Option<&str>,
) -> Result<ProviderProbeResult, CoderError> {
	let endpoint = format!("{base_url}/models");
	let mut req = http.get(&endpoint).timeout(PROBE_TIMEOUT);
	if let Some(key) = api_key {
		req = req.bearer_auth(key);
	}
	let response = req.send().await.map_err(CoderError::from)?;
	let status = response.status();
	let body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body));
	}

	#[derive(Deserialize)]
	struct ModelsBody {
		#[serde(default)]
		data: Vec<ModelEntry>,
	}
	#[derive(Deserialize)]
	struct ModelEntry {
		id: String,
	}

	let parsed: ModelsBody = serde_json::from_str(&body)
		.map_err(|err| CoderError::decode(&endpoint, format!("could not parse /models body: {err}")))?;
	let model_count = u32::try_from(parsed.data.len()).unwrap_or(u32::MAX);
	let sample_model_ids = parsed.data.into_iter().take(PROBE_SAMPLE_LIMIT).map(|m| m.id).collect();
	Ok(ProviderProbeResult {
		model_count,
		sample_model_ids,
	})
}

async fn probe_chat_ping(
	http: &reqwest::Client,
	base_url: &str,
	api_key: Option<&str>,
) -> Result<ProviderProbeResult, CoderError> {
	// 1-token completion against a sentinel model. We accept any
	// non-auth response: an "unknown model" 400 still proves
	// "endpoint is reachable and the auth header parsed". The
	// only outright failure we surface is 401/403 (so a key
	// typo is loud at save time) or a network-level error.
	let endpoint = format!("{base_url}/chat/completions");
	let body = serde_json::json!({
		"model": "probe",
		"messages": [{"role": "user", "content": "ping"}],
		"max_tokens": 1,
		"stream": false,
	});
	let mut req = http.post(&endpoint).timeout(PROBE_TIMEOUT).json(&body);
	if let Some(key) = api_key {
		req = req.bearer_auth(key);
	}
	let response = req.send().await.map_err(CoderError::from)?;
	let status = response.status();
	if status.as_u16() == 401 || status.as_u16() == 403 {
		let text = response.text().await.unwrap_or_default();
		return Err(CoderError::http(endpoint, status.as_u16(), text));
	}
	Ok(ProviderProbeResult {
		model_count: 0,
		sample_model_ids: Vec::new(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ids_are_url_safe_and_unique() {
		let a = new_provider_id();
		std::thread::sleep(Duration::from_millis(2));
		let b = new_provider_id();
		assert_ne!(a, b);
		assert!(a.starts_with("prov-"));
		assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
	}
}
