//! Web search and fetch tools for the agent.
//!
//! Two primitives, both pure outbound HTTP from the IDE process (no
//! `WorkspaceHost` involvement — there's no fs to touch):
//!
//! - **`search`** — Tavily Search API. Returns a small list of
//!   `{ title, url, snippet, published_date? }` entries. Tavily is
//!   a SERP-as-a-service designed for LLMs; the schema is clean
//!   JSON, the free tier covers 1k searches / month, and the result
//!   bodies are short enough that a typical 8-result SERP fits in
//!   ~1 K tokens. API key per user, stored in the OS keyring under
//!   `service=moon-ide, account=coder-web-search:tavily` (same
//!   pattern the HF OAuth bundle uses).
//! - **`fetch`** — Jina Reader (`https://r.jina.ai/<url>`). Takes a
//!   URL, returns clean markdown extracted from the page. No key
//!   required for the free tier (60 RPM is plenty for an interactive
//!   editor agent). Picked over an in-process HTML→markdown extractor
//!   because (a) it's literally one `reqwest::get` and zero deps,
//!   (b) the extraction quality is consistently good across SPAs /
//!   doc sites / blogs that an embedded readability port would
//!   handle badly.
//!
//! Failure shape: every error becomes a [`CoderError::ToolFailed`]
//! with a human-readable message. The agent loop maps that to
//! `is_error: true` content and the model retries / explains.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::CoderError;

/// Keyring service name — same as the rest of moon-ide.
const KEYRING_SERVICE: &str = "moon-ide";
/// Keyring account name for the Tavily API key. Provider-prefixed
/// so future search providers (Brave, Serper, Exa) drop in as
/// sibling accounts without colliding.
const TAVILY_KEYRING_ACCOUNT: &str = "coder-web-search:tavily";

/// Tavily's REST endpoint. Versionless on purpose — they pin
/// breaking changes behind explicit query params, and the body
/// shape we send is the one documented at <https://docs.tavily.com>.
const TAVILY_SEARCH_ENDPOINT: &str = "https://api.tavily.com/search";

/// Jina Reader takes any URL appended to this prefix and returns
/// the page as markdown. No auth needed on the free tier.
const JINA_READER_PREFIX: &str = "https://r.jina.ai/";

/// Hard cap on Jina Reader response bytes. A single huge doc page
/// shouldn't blow the agent's context — past this we truncate and
/// flag the result. 200 kB ≈ ~50 K tokens on the conventional
/// `bytes / 4` ratio, which is enough for a full MDN-style article
/// without monopolising a 128 K context window.
const JINA_MAX_BYTES: usize = 200_000;

/// Default `max_results` when the agent doesn't pin one. Eight rows
/// is the sweet spot in dogfooding — enough variety to skim, small
/// enough to keep the prompt cheap.
const DEFAULT_SEARCH_MAX_RESULTS: u32 = 8;
/// Hard upper bound; Tavily itself accepts up to 20.
const MAX_SEARCH_MAX_RESULTS: u32 = 20;

/// Per-request timeout. Long enough for Tavily's slowest reasonable
/// response (≤ 10 s in practice) and Jina's full-page extraction
/// on dense doc sites (≤ 20 s observed); short enough that a hung
/// upstream doesn't pin the agent's turn.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// One SERP entry, as returned to the model from the `web_search`
/// tool. Kept narrow — `score` and `raw_content` are deliberately
/// dropped because the agent doesn't need them to pick which URL
/// to `web_fetch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResult {
	pub title: String,
	pub url: String,
	pub snippet: String,
	/// ISO-8601 publication date when Tavily knows it (typically
	/// for news and blog posts). Useful so the model can tell stale
	/// answers from fresh ones.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub published_date: Option<String>,
}

/// Result of one `web_fetch`. `markdown` is the cleaned page body;
/// `truncated` flags when we hit [`JINA_MAX_BYTES`] and chopped the
/// tail so the agent knows to try a narrower follow-up
/// (`web_fetch` on a deeper sub-page, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchResult {
	pub url: String,
	pub markdown: String,
	pub truncated: bool,
}

/// Owns the keyring entry for the Tavily key. Stateless — held by
/// [`WebClient`] so the service / account constants live in one
/// place.
#[derive(Debug, Clone, Copy)]
pub struct TavilyKeyStore;

impl TavilyKeyStore {
	const fn new() -> Self {
		Self
	}

	fn entry(&self) -> Result<keyring::Entry, CoderError> {
		keyring::Entry::new(KEYRING_SERVICE, TAVILY_KEYRING_ACCOUNT).map_err(CoderError::from)
	}

	pub fn load(&self) -> Result<Option<String>, CoderError> {
		match self.entry()?.get_password() {
			Ok(s) if s.trim().is_empty() => Ok(None),
			Ok(s) => Ok(Some(s)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(err) => Err(err.into()),
		}
	}

	pub fn save(&self, key: &str) -> Result<(), CoderError> {
		self.entry()?.set_password(key)?;
		Ok(())
	}

	pub fn clear(&self) -> Result<(), CoderError> {
		match self.entry()?.delete_credential() {
			Ok(()) => Ok(()),
			Err(keyring::Error::NoEntry) => Ok(()),
			Err(err) => Err(err.into()),
		}
	}
}

/// HTTP client + cached Tavily key, shared across the tool
/// registry. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct WebClient {
	http: reqwest::Client,
	store: TavilyKeyStore,
	/// In-memory cache of the keyring entry so the hot path
	/// (`has_tavily_key` from the sync `definitions()`) doesn't
	/// hit the OS keyring on every chat-completions request.
	/// `None` = no key configured; `Some(_)` = configured.
	/// Wrapped in a `std::sync::RwLock` (not tokio's) because the
	/// only readers are sync code paths and writes are rare.
	key: Arc<RwLock<Option<String>>>,
}

impl WebClient {
	pub fn new() -> Result<Self, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.timeout(HTTP_TIMEOUT)
			.build()
			.map_err(CoderError::from)?;
		let store = TavilyKeyStore::new();
		let initial = store.load().unwrap_or_else(|err| {
			// Keyring failure at boot is non-fatal — the user just
			// sees "no key configured" until they set one. Log so
			// we can correlate later complaints.
			tracing::warn!(error = %err, "failed to load tavily key from keyring; treating as unconfigured");
			None
		});
		Ok(Self {
			http,
			store,
			key: Arc::new(RwLock::new(initial)),
		})
	}

	/// Sync flag for the tool advertiser. When false, the
	/// [`super::tools::ToolRegistry`] omits `web_search` from the
	/// definition list it ships to the model — no point telling
	/// the agent about a tool that's guaranteed to fail.
	pub fn has_tavily_key(&self) -> bool {
		self.key.read().map(|g| g.is_some()).unwrap_or(false)
	}

	/// Persist a new Tavily API key. Empty / whitespace-only keys
	/// are rejected — silently storing an empty string would mean
	/// `has_tavily_key()` returns `true` and search calls fail
	/// loudly at the wrong layer.
	pub fn set_tavily_key(&self, key: &str) -> Result<(), CoderError> {
		let trimmed = key.trim();
		if trimmed.is_empty() {
			return Err(CoderError::invalid_args("web_search", "tavily key must not be empty"));
		}
		self.store.save(trimmed)?;
		if let Ok(mut guard) = self.key.write() {
			*guard = Some(trimmed.to_owned());
		}
		Ok(())
	}

	/// Drop the keyring entry and the cache. Idempotent.
	pub fn clear_tavily_key(&self) -> Result<(), CoderError> {
		self.store.clear()?;
		if let Ok(mut guard) = self.key.write() {
			*guard = None;
		}
		Ok(())
	}

	fn current_tavily_key(&self) -> Option<String> {
		self.key.read().ok().and_then(|g| g.clone())
	}

	/// One Tavily search. Returns the raw result list at the
	/// agreed `WebSearchResult` shape. Cancellation: the outer
	/// future is dropped if `cancel` fires, which aborts the
	/// in-flight `reqwest` future.
	pub async fn search(
		&self,
		query: &str,
		max_results: u32,
		cancel: &CancellationToken,
	) -> Result<Vec<WebSearchResult>, CoderError> {
		let Some(key) = self.current_tavily_key() else {
			return Err(CoderError::tool_failed(
				"web_search",
				"no Tavily API key configured — set one in Coder settings → Web search",
			));
		};
		let trimmed = query.trim();
		if trimmed.is_empty() {
			return Err(CoderError::invalid_args("web_search", "query must not be empty"));
		}
		let capped = max_results.clamp(1, MAX_SEARCH_MAX_RESULTS);

		// Tavily's docs: POST JSON to /search, key in the body.
		// `search_depth: "basic"` is the cheap path; `include_answer`
		// off because we want the SERP rows, not their LLM-written
		// summary (that's the C-design we explicitly opted out of).
		let body = serde_json::json!({
			"api_key": key,
			"query": trimmed,
			"search_depth": "basic",
			"include_answer": false,
			"include_raw_content": false,
			"include_images": false,
			"max_results": capped,
		});

		let request = self.http.post(TAVILY_SEARCH_ENDPOINT).json(&body).send();
		let response = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			result = request => result.map_err(CoderError::from)?,
		};
		let status = response.status();
		let text = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			result = response.text() => result.map_err(CoderError::from)?,
		};

		if !status.is_success() {
			// Tavily surfaces `{"detail": "Invalid API key"}` and
			// similar on 4xx — surface that verbatim so the user
			// sees what to fix.
			let detail = parse_error_detail(&text).unwrap_or_else(|| text.clone());
			return Err(CoderError::tool_failed(
				"web_search",
				format!("Tavily {status}: {detail}"),
			));
		}

		let raw: TavilyResponse = serde_json::from_str(&text)
			.map_err(|err| CoderError::tool_failed("web_search", format!("could not parse Tavily response: {err}")))?;

		let results = raw
			.results
			.into_iter()
			.map(|r| WebSearchResult {
				title: r.title,
				url: r.url,
				snippet: r.content,
				published_date: r.published_date.filter(|d| !d.is_empty()),
			})
			.collect();
		Ok(results)
	}

	/// One Jina Reader fetch. Returns clean markdown for the page
	/// at `url`, truncated to [`JINA_MAX_BYTES`] if the body is
	/// huge.
	pub async fn fetch(&self, url: &str, cancel: &CancellationToken) -> Result<WebFetchResult, CoderError> {
		let trimmed = url.trim();
		if trimmed.is_empty() {
			return Err(CoderError::invalid_args("web_fetch", "url must not be empty"));
		}
		// Defensive: Jina Reader will follow whatever we hand it,
		// but `javascript:` / `file:` / `data:` schemes have no
		// business inside the agent's tool surface. Force http/https
		// at the entry point.
		let parsed =
			url::Url::parse(trimmed).map_err(|err| CoderError::invalid_args("web_fetch", format!("invalid URL: {err}")))?;
		let scheme = parsed.scheme();
		if scheme != "http" && scheme != "https" {
			return Err(CoderError::invalid_args(
				"web_fetch",
				format!("only http and https URLs are allowed, got {scheme}"),
			));
		}

		let endpoint = format!("{JINA_READER_PREFIX}{trimmed}");
		// `Accept: text/plain` asks Jina to return the markdown
		// body directly rather than its JSON envelope — saves a
		// parse step and matches what the agent wants to see.
		let request = self.http.get(&endpoint).header("Accept", "text/plain").send();
		let response = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			result = request => result.map_err(CoderError::from)?,
		};
		let status = response.status();
		let text = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			result = response.text() => result.map_err(CoderError::from)?,
		};

		if !status.is_success() {
			return Err(CoderError::tool_failed(
				"web_fetch",
				format!("Jina Reader {status} fetching {trimmed}: {}", truncate_for_error(&text)),
			));
		}

		let (markdown, truncated) = if text.len() > JINA_MAX_BYTES {
			let mut end = JINA_MAX_BYTES;
			while !text.is_char_boundary(end) {
				end -= 1;
			}
			(text[..end].to_owned(), true)
		} else {
			(text, false)
		};

		Ok(WebFetchResult {
			url: trimmed.to_owned(),
			markdown,
			truncated,
		})
	}

	pub fn default_search_max_results() -> u32 {
		DEFAULT_SEARCH_MAX_RESULTS
	}

	pub fn max_search_max_results() -> u32 {
		MAX_SEARCH_MAX_RESULTS
	}
}

/// Tavily's response shape — just the fields the picker reads.
/// Extra fields (e.g. `answer`, `query`, `response_time`) are
/// dropped by serde at deserialize time.
#[derive(Debug, Deserialize)]
struct TavilyResponse {
	#[serde(default)]
	results: Vec<TavilyResultRaw>,
}

#[derive(Debug, Deserialize)]
struct TavilyResultRaw {
	#[serde(default)]
	title: String,
	#[serde(default)]
	url: String,
	#[serde(default)]
	content: String,
	#[serde(default)]
	published_date: Option<String>,
}

/// Best-effort: pull the `detail` field out of Tavily's error body.
/// They occasionally also use `error` or `message`; cover the
/// common shapes so the surfaced error reads cleanly.
fn parse_error_detail(body: &str) -> Option<String> {
	let v: Value = serde_json::from_str(body).ok()?;
	let obj = v.as_object()?;
	for key in ["detail", "error", "message"] {
		if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
			return Some(s.to_owned());
		}
	}
	None
}

/// Cap an error body at 500 chars so the surfaced `ToolFailed`
/// message stays readable. The agent gets to see only the head,
/// which is where Jina puts the actionable bit ("Rate limited", "URL
/// not reachable", …); the long retry advice / HTML body that some
/// providers append never makes it.
fn truncate_for_error(body: &str) -> String {
	const MAX: usize = 500;
	if body.len() <= MAX {
		return body.to_owned();
	}
	let mut end = MAX;
	while !body.is_char_boundary(end) {
		end -= 1;
	}
	format!("{}… ({} bytes total)", &body[..end], body.len())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_error_detail_handles_common_shapes() {
		assert_eq!(
			parse_error_detail(r#"{"detail":"Invalid API key"}"#).as_deref(),
			Some("Invalid API key"),
		);
		assert_eq!(
			parse_error_detail(r#"{"error":"quota exceeded"}"#).as_deref(),
			Some("quota exceeded"),
		);
		assert_eq!(
			parse_error_detail(r#"{"message":"rate limited"}"#).as_deref(),
			Some("rate limited"),
		);
		assert!(parse_error_detail("not json").is_none());
		assert!(parse_error_detail("{}").is_none());
	}

	#[test]
	fn truncate_keeps_short_bodies_intact() {
		assert_eq!(truncate_for_error("short"), "short");
	}

	#[test]
	fn truncate_lops_huge_bodies_and_reports_total() {
		let huge = "a".repeat(2_000);
		let out = truncate_for_error(&huge);
		assert!(out.starts_with(&"a".repeat(500)));
		assert!(out.contains("2000 bytes total"));
	}

	#[test]
	fn tavily_response_tolerates_extra_fields() {
		let body = r#"{
			"answer": "ignored",
			"query": "rust async",
			"response_time": 1.23,
			"results": [
				{"title":"A","url":"https://a","content":"a body","published_date":"2025-01-01"},
				{"title":"B","url":"https://b","content":"b body"}
			]
		}"#;
		let parsed: TavilyResponse = serde_json::from_str(body).unwrap();
		assert_eq!(parsed.results.len(), 2);
		assert_eq!(parsed.results[0].published_date.as_deref(), Some("2025-01-01"));
		assert_eq!(parsed.results[1].published_date, None);
	}

	#[test]
	fn web_client_starts_without_key_cached() {
		// On a CI box with no prior keyring entry, the cache is
		// `None` and `has_tavily_key()` returns `false`. Can't
		// assert the positive case without polluting the user's
		// real keyring; the round-trip path is covered manually
		// when somebody runs the IDE.
		let client = WebClient::new().expect("client constructs");
		// We can't assert `false` here unconditionally — a dev who
		// has actually configured a key on their dev box will see
		// `true`. But the constructor must succeed and not panic
		// regardless of keyring state.
		let _ = client.has_tavily_key();
	}
}
