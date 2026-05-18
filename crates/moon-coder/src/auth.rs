//! Hugging Face OAuth — device authorization grant (RFC 8628).
//!
//! Flow shape, exactly as `specs/coder.md` § Authentication describes:
//!
//! 1. POST `/oauth/authorize/device` with `client_id` + `scope`.
//!    HF returns `{ device_code, user_code, verification_uri, expires_in,
//!    interval }`.
//! 2. The UI shows the user code + opens `verification_uri` in the
//!    system browser. Meanwhile the loop polls
//!    POST `/oauth/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`
//!    every `interval` seconds.
//! 3. The token endpoint replies with `authorization_pending` /
//!    `slow_down` until the user approves; then `{ access_token,
//!    refresh_token, expires_in, token_type, scope }`.
//! 4. We persist the token bundle to the OS keyring under
//!    `service=moon-ide`, `account=hf-oauth` and call
//!    `/oauth/userinfo` to learn who they are.
//!
//! Refresh: when an access token is within 60 s of expiry, or a 401
//! comes back from a downstream call, we POST `/oauth/token` with
//! `grant_type=refresh_token` and rotate the bundle in the keyring.

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::defaults::{HF_HUB_BASE, HF_OAUTH_CLIENT_ID, HF_OAUTH_SCOPES};
use crate::error::{request_id_of, CoderError};

const KEYRING_SERVICE: &str = "moon-ide";
const KEYRING_ACCOUNT: &str = "hf-oauth";

/// Refresh the access token this many seconds before it actually
/// expires. Long enough to absorb clock skew + a slow first request;
/// short enough that we don't waste tokens we already have.
const REFRESH_LEAD_TIME_SECS: u64 = 60;

/// Hard ceiling on how long we'll poll the device endpoint without
/// hearing back. Belt-and-braces: HF's `expires_in` already covers
/// this, but if the response is malformed we don't want to busy-loop
/// forever.
const POLL_TIMEOUT_FALLBACK: Duration = Duration::from_secs(15 * 60);

/// What the server returns from the device-authorization endpoint.
/// The `device_code` is the long secret the loop polls with; the
/// `user_code` is the short one the UI shows. We keep both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCode {
	/// Short user-facing code (typically 8 chars). The panel displays
	/// this; HF's verification page asks the user to type it.
	pub user_code: String,
	/// URL the user opens in the browser. The IDE pops this via
	/// `tauri-plugin-opener`. Falls back to `verification_uri` if
	/// the server doesn't supply the `_complete` variant.
	pub verification_uri: String,
	/// `verification_uri_complete` from HF — same URL with the user
	/// code pre-filled in the query string. UI prefers this when set
	/// so the user doesn't have to copy-paste manually.
	pub verification_uri_complete: Option<String>,
	/// How many seconds the device code stays valid.
	pub expires_in: u64,
	/// Server-suggested polling interval in seconds. RFC 8628 says
	/// 5 if missing.
	pub interval: u64,
	/// Opaque blob the loop polls the token endpoint with. Not shown
	/// to the user; not persisted (only valid until completion).
	pub device_code: String,
}

/// Persisted token bundle. Stored as a JSON blob in the keyring.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenBundle {
	access_token: String,
	refresh_token: Option<String>,
	/// Wall-clock seconds since the unix epoch when `access_token`
	/// stops being valid. We compute this from `expires_in` at
	/// receive time so refresh logic is pure arithmetic later.
	expires_at_unix: u64,
	#[serde(default)]
	scope: String,
	#[serde(default)]
	token_type: String,
}

/// What `/oauth/userinfo` returns, trimmed to what we render +
/// the `orgs` array we now consume to feed the model picker's
/// "Bill to" dropdown.
///
/// Note `username` (`preferred_username`) and `name` are
/// **separate** OIDC claims and HF returns both — `username` is
/// the login slug (`eliheros`), `name` is the display name
/// ("Eli Hero"). Originally we had `alias = "name"` on
/// `preferred_username`, which made serde collapse them and report
/// "duplicate field `preferred_username`". Don't be tempted to
/// re-add the alias.
///
/// `orgs` is read straight from the OAuth userinfo response —
/// previously we hit `/api/whoami-v2` for the same data, but
/// userinfo already carries it and we already have an OAuth token
/// to authenticate with, so it's one less endpoint + scope to
/// reason about. The set of orgs is determined by what the user
/// consented to share at OAuth time; an org the user is a member of
/// but didn't expose to moon-ide simply won't show up in the
/// picker. If the userinfo response omits `orgs` entirely the field
/// defaults to an empty list — the "Personal account" option always
/// works regardless.
// Renames are `deserialize`-only on every field below: HF emits
// `preferred_username` / `picture` / `canPay` / `roleInOrg` /
// `isEnterprise` on the wire, but we forward the parsed struct
// straight to the frontend via Tauri's IPC and the TS side expects
// the Rust field names. A bidirectional `rename = "…"` makes the
// Tauri serializer write `preferred_username` etc. back out, which
// the TS reads as `undefined` — every renamed field arrives empty
// and the picker silently breaks (empty parens after the org name,
// "Bill to" sending the display name instead of the slug).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfIdentity {
	#[serde(rename(deserialize = "preferred_username"))]
	pub username: String,
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default, rename(deserialize = "picture"))]
	pub avatar_url: Option<String>,
	#[serde(default)]
	pub email: Option<String>,
	/// Orgs the user belongs to, as reported by `/oauth/userinfo`.
	/// Sorted server-side; we preserve that order. Each entry is
	/// safe to send to the router as `X-HF-Bill-To: <name>`
	/// provided the org has `can_pay: true` (the picker greys out
	/// orgs that can't pay and explains why on hover).
	#[serde(default)]
	pub orgs: Vec<HfOrg>,
}

/// One entry of [`HfIdentity::orgs`]. Field names mirror HF's
/// userinfo payload (camelCase on the wire); we rename to
/// `snake_case` for the Rust + TS surface. Unknown fields are
/// dropped silently — we only consume what the picker needs.
///
/// **Slug vs. display name**: HF returns `preferred_username` as
/// the URL slug (`huggingface`) and `name` as the display string
/// (`"Hugging Face"`). The router's `X-HF-Bill-To` header expects
/// the slug, so the picker sends `slug` while showing `name` in
/// the dropdown row. Old userinfo payloads (or denied-scope cases)
/// might omit `preferred_username`; we fall back to `name` in
/// that case to keep the dropdown functional even though billing
/// will likely fail — the router error reaches the panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfOrg {
	/// Display name as HF shows it everywhere
	/// (`"Hugging Face"`). Surfaced in the picker dropdown row.
	pub name: String,
	/// URL slug (`huggingface`) — what `X-HF-Bill-To` actually
	/// wants. `None` when the userinfo payload omitted it (older
	/// HF builds or a scope-denied response that only carried
	/// `name`); the picker falls back to `name` in that case so
	/// billing at least attempts a string the user typed by hand
	/// would too.
	#[serde(default, rename(deserialize = "preferred_username"))]
	pub slug: Option<String>,
	/// Display avatar for the org. `None` falls back to the
	/// auto-generated identicon HF renders elsewhere.
	#[serde(default, rename(deserialize = "picture"))]
	pub avatar_url: Option<String>,
	/// Authoritative flag: `true` iff the user can bill inference
	/// calls to this org.
	///
	/// Populated by HF whenever the `read-billing` OAuth scope was
	/// granted *and* the user authorized this specific org at consent
	/// time. A `false` value is a real "cannot pay" signal (no
	/// credits, role doesn't permit billing, etc.) — the picker
	/// disables the corresponding `<option>` so the user can't pick
	/// it. Without `read-billing` the field is absent server-side and
	/// we deserialize to `false`; same effect, which is fine because
	/// we never reach this code path for orgs that aren't in the
	/// authorized set anyway (those get filtered out earlier — see
	/// [`role_in_org`](Self::role_in_org)).
	#[serde(default, rename(deserialize = "canPay"))]
	pub can_pay: bool,
	/// User's role inside the org, e.g. `"admin"` / `"contributor"`.
	///
	/// **Per-org consent signal.** HF only emits `roleInOrg` (and the
	/// other org-scoped fields like `canPay`) for orgs the user
	/// explicitly authorized moon-ide for at the OAuth consent
	/// screen. A `None` value means the user is a member of that org
	/// but didn't tick its checkbox at consent time, and the entry
	/// carries no usable signal. The picker filters those rows out
	/// of the bill-to dropdown — if a user expects an org and doesn't
	/// see it, they sign out + back in and re-tick at the consent
	/// screen.
	#[serde(default, rename(deserialize = "roleInOrg"))]
	pub role_in_org: Option<String>,
	/// True for enterprise orgs. Drives a small badge in the
	/// picker so users running on a personal + work account can
	/// tell them apart at a glance.
	#[serde(default, rename(deserialize = "isEnterprise"))]
	pub is_enterprise: bool,
}

/// Owns the keyring entry for the HF OAuth tokens. Stateless — held
/// by `Authenticator` (and indirectly by `AppState`) so the keyring
/// service/account constants live in one place.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenStore;

impl TokenStore {
	pub const fn new() -> Self {
		Self
	}

	fn entry(&self) -> Result<keyring::Entry, CoderError> {
		keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(CoderError::from)
	}

	fn save(&self, bundle: &TokenBundle) -> Result<(), CoderError> {
		let json =
			serde_json::to_string(bundle).map_err(|err| CoderError::Internal(format!("token serialize failed: {err}")))?;
		self.entry()?.set_password(&json)?;
		Ok(())
	}

	fn load(&self) -> Result<Option<TokenBundle>, CoderError> {
		let entry = self.entry()?;
		match entry.get_password() {
			Ok(blob) => match serde_json::from_str::<TokenBundle>(&blob) {
				Ok(b) => Ok(Some(b)),
				Err(err) => {
					// Old/corrupt blob: treat as "not signed in" rather
					// than surfacing a parse error to the user. They'll
					// re-auth and the rotation overwrites the bad blob.
					tracing::warn!(error = %err, "stored hf-oauth token blob unparseable; clearing");
					self.clear().ok();
					Ok(None)
				}
			},
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(err) => Err(err.into()),
		}
	}

	pub fn clear(&self) -> Result<(), CoderError> {
		match self.entry()?.delete_credential() {
			Ok(()) => Ok(()),
			Err(keyring::Error::NoEntry) => Ok(()),
			Err(err) => Err(err.into()),
		}
	}
}

/// Authenticator owns the token bundle, the keyring, and the HTTP
/// client used for OAuth-only calls (`/oauth/*` endpoints). Inference
/// gets a separate client wrapped in middleware that defers to
/// `current_access_token` — the auth machine's only public surface
/// against the rest of the crate.
#[derive(Clone)]
pub struct Authenticator {
	http: reqwest::Client,
	store: TokenStore,
	/// In-memory cache of the last loaded bundle. Wrapped in
	/// `Arc<RwLock>` so token rotation in one task is visible to all
	/// other tasks immediately (no "kept refreshing because we read a
	/// stale cache" footgun).
	cached: Arc<RwLock<Option<TokenBundle>>>,
}

impl Authenticator {
	pub fn new() -> Result<Self, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(CoderError::from)?;
		let store = TokenStore::new();
		let cached = Arc::new(RwLock::new(store.load()?));
		Ok(Self { http, store, cached })
	}

	/// True iff a non-expired access token is cached or refreshable.
	/// Note: doesn't validate the token against `/oauth/userinfo` —
	/// callers wanting that signal should call [`identity`] instead.
	pub async fn has_valid_session(&self) -> bool {
		let cached = self.cached.read().await;
		match cached.as_ref() {
			Some(bundle) => bundle.refresh_token.is_some() || !is_expired(bundle),
			None => false,
		}
	}

	/// Drop the keyring entry and the cache. Idempotent.
	pub async fn sign_out(&self) -> Result<(), CoderError> {
		self.store.clear()?;
		*self.cached.write().await = None;
		Ok(())
	}

	/// Step 1: ask HF for a device code. The caller pops
	/// `verification_uri` (or `_complete`) in the system browser and
	/// hands the `DeviceCode` back to [`poll_device_code`] when ready.
	///
	/// Endpoint is `/oauth/device` per the discoverable
	/// [`.well-known/openid-configuration`](https://huggingface.co/.well-known/openid-configuration)
	/// (key `device_authorization_endpoint`). The earlier guess of
	/// `/oauth/authorize/device` was a 404.
	pub async fn start_device_flow(&self) -> Result<DeviceCode, CoderError> {
		let endpoint = format!("{HF_HUB_BASE}/oauth/device");
		let params = [("client_id", HF_OAUTH_CLIENT_ID), ("scope", HF_OAUTH_SCOPES)];

		let response = self
			.http
			.post(&endpoint)
			.form(&params)
			.send()
			.await
			.map_err(CoderError::from)?;

		let status = response.status();
		let request_id = request_id_of(&response);
		let body = response.text().await.map_err(CoderError::from)?;
		if !status.is_success() {
			return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
		}

		// HF docs: the body is JSON shaped per RFC 8628 with the optional
		// `verification_uri_complete` extension. We accept both the
		// snake_case spelling and `verification_url` as some HF doc
		// pages have used the latter — tolerate both for forward
		// compatibility.
		#[derive(Deserialize)]
		struct DeviceCodeRaw {
			device_code: String,
			user_code: String,
			#[serde(default, alias = "verification_url")]
			verification_uri: String,
			#[serde(default, alias = "verification_url_complete")]
			verification_uri_complete: Option<String>,
			expires_in: u64,
			#[serde(default = "default_interval")]
			interval: u64,
		}
		fn default_interval() -> u64 {
			5
		}

		let raw: DeviceCodeRaw = decode_body(&endpoint, &body)?;
		Ok(DeviceCode {
			user_code: raw.user_code,
			verification_uri: raw.verification_uri,
			verification_uri_complete: raw.verification_uri_complete,
			expires_in: raw.expires_in,
			interval: raw.interval,
			device_code: raw.device_code,
		})
	}

	/// Step 2: poll the token endpoint until either the user
	/// approves, denies, or the device code expires. The future
	/// resolves with the userinfo when the bundle has landed in the
	/// keyring; the caller is meant to send the panel into "signed
	/// in" state at that point.
	pub async fn poll_device_code(&self, code: &DeviceCode) -> Result<HfIdentity, CoderError> {
		let endpoint = format!("{HF_HUB_BASE}/oauth/token");
		let mut interval = Duration::from_secs(code.interval.max(1));
		let max_wait = Duration::from_secs(code.expires_in).min(POLL_TIMEOUT_FALLBACK);
		let started = std::time::Instant::now();

		loop {
			if started.elapsed() >= max_wait {
				return Err(CoderError::DeviceFlowExpired);
			}
			tokio::time::sleep(interval).await;

			let params = [
				("client_id", HF_OAUTH_CLIENT_ID),
				("device_code", &code.device_code),
				("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
			];
			let response = self
				.http
				.post(&endpoint)
				.form(&params)
				.send()
				.await
				.map_err(CoderError::from)?;

			let status = response.status();
			let request_id = request_id_of(&response);
			let body = response.text().await.map_err(CoderError::from)?;

			if status.is_success() {
				let bundle = parse_token_response(&body, &endpoint)?;
				let identity = self.fetch_userinfo(&bundle.access_token).await?;
				self.store.save(&bundle)?;
				*self.cached.write().await = Some(bundle);
				return Ok(identity);
			}

			// RFC 8628 puts the OAuth error code inside the JSON body
			// even on 4xx. `authorization_pending` and `slow_down` are
			// the "keep going" cases.
			let code_str = parse_oauth_error_code(&body);
			match code_str.as_deref() {
				Some("authorization_pending") => {}
				Some("slow_down") => {
					interval += Duration::from_secs(5);
				}
				Some("expired_token") => return Err(CoderError::DeviceFlowExpired),
				Some("access_denied") => return Err(CoderError::DeviceFlowDenied),
				_ => return Err(CoderError::http(endpoint, status.as_u16(), body, request_id)),
			}
		}
	}

	/// Hand out a usable access token, refreshing if needed.
	/// Returns [`CoderError::NotSignedIn`] when there's no bundle and
	/// no refresh token to recover with.
	pub async fn current_access_token(&self) -> Result<String, CoderError> {
		{
			let cached = self.cached.read().await;
			if let Some(bundle) = cached.as_ref() {
				if !is_near_expiry(bundle) {
					return Ok(bundle.access_token.clone());
				}
			} else {
				return Err(CoderError::NotSignedIn);
			}
		}
		// Fall through: bundle exists but is near/over expiry. Refresh
		// under the write lock so two concurrent callers don't trigger
		// two refresh requests.
		self.refresh_now().await
	}

	/// Force a refresh round trip. Used by the inference middleware on
	/// 401 responses. Returns the new access token.
	pub async fn refresh_now(&self) -> Result<String, CoderError> {
		let mut cached = self.cached.write().await;
		let Some(bundle) = cached.as_ref().cloned() else {
			return Err(CoderError::NotSignedIn);
		};
		let Some(refresh_token) = bundle.refresh_token.as_ref() else {
			return Err(CoderError::NotSignedIn);
		};

		let endpoint = format!("{HF_HUB_BASE}/oauth/token");
		let params = [
			("client_id", HF_OAUTH_CLIENT_ID),
			("grant_type", "refresh_token"),
			("refresh_token", refresh_token.as_str()),
		];
		let response = self
			.http
			.post(&endpoint)
			.form(&params)
			.send()
			.await
			.map_err(CoderError::from)?;
		let status = response.status();
		let request_id = request_id_of(&response);
		let body = response.text().await.map_err(CoderError::from)?;
		if !status.is_success() {
			// Refresh token has been revoked or expired. Drop the bundle
			// from both keyring and cache so the next call surfaces
			// `NotSignedIn` and the panel re-prompts.
			tracing::warn!(status = %status, "hf-oauth refresh failed; clearing stored token");
			self.store.clear().ok();
			*cached = None;
			return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
		}

		let new_bundle = parse_token_response(&body, &endpoint)?;
		self.store.save(&new_bundle)?;
		let access = new_bundle.access_token.clone();
		*cached = Some(new_bundle);
		Ok(access)
	}

	/// Fetch the signed-in user's profile. Returns `Ok(None)` when
	/// there's no bundle to send (caller renders the empty state);
	/// `Err` only on real network / decode failure.
	pub async fn identity(&self) -> Result<Option<HfIdentity>, CoderError> {
		let token = match self.current_access_token().await {
			Ok(t) => t,
			Err(CoderError::NotSignedIn) => return Ok(None),
			Err(err) => return Err(err),
		};
		let identity = self.fetch_userinfo(&token).await?;
		Ok(Some(identity))
	}

	async fn fetch_userinfo(&self, access_token: &str) -> Result<HfIdentity, CoderError> {
		let endpoint = format!("{HF_HUB_BASE}/oauth/userinfo");
		let response = self
			.http
			.get(&endpoint)
			.bearer_auth(access_token)
			.send()
			.await
			.map_err(CoderError::from)?;
		let status = response.status();
		let request_id = request_id_of(&response);
		let body = response.text().await.map_err(CoderError::from)?;
		if !status.is_success() {
			return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
		}

		// Debug-only verbatim body. Contains the user's HF-registered
		// email + email_verified + per-org payload, so it's gated
		// behind `RUST_LOG=moon_coder::auth=debug` — not on by default.
		// Useful when an org's `slug` arrives as `null` to confirm
		// whether HF actually omitted `preferred_username` or we
		// renamed the field on our side.
		tracing::debug!(body = %body, "/oauth/userinfo raw response");

		let identity: HfIdentity = decode_body(&endpoint, &body)?;

		// Info-level summary of what we deserialised. Just the fields
		// the picker reads — username, the orgs each as
		// `name (slug)` or `name (<no slug>)` so a missing
		// `preferred_username` per-entry is loud in the log.
		let org_summary: Vec<String> = identity
			.orgs
			.iter()
			.map(|o| match &o.slug {
				Some(slug) => format!("{} ({slug})", o.name),
				None => format!("{} (<no slug>)", o.name),
			})
			.collect();
		tracing::info!(
			username = %identity.username,
			orgs_count = identity.orgs.len(),
			orgs = ?org_summary,
			"parsed /oauth/userinfo"
		);
		Ok(identity)
	}
}

fn parse_token_response(body: &str, endpoint: &str) -> Result<TokenBundle, CoderError> {
	#[derive(Deserialize)]
	struct TokenResponseRaw {
		access_token: String,
		#[serde(default)]
		refresh_token: Option<String>,
		#[serde(default)]
		expires_in: Option<u64>,
		#[serde(default)]
		scope: String,
		#[serde(default)]
		token_type: String,
	}
	let raw: TokenResponseRaw = decode_body(endpoint, body)?;
	let now = unix_now();
	// HF's docs document `expires_in` for access tokens; if the
	// server omits it, fall back to one hour. Conservative — refresh
	// will kick in well before HF's actual cutoff.
	let lifetime = raw.expires_in.unwrap_or(3600);
	Ok(TokenBundle {
		access_token: raw.access_token,
		refresh_token: raw.refresh_token,
		expires_at_unix: now.saturating_add(lifetime),
		scope: raw.scope,
		token_type: raw.token_type,
	})
}

fn parse_oauth_error_code(body: &str) -> Option<String> {
	#[derive(Deserialize)]
	struct OauthErr {
		error: Option<String>,
	}
	serde_json::from_str::<OauthErr>(body).ok().and_then(|e| e.error)
}

/// Wrap `serde_json::from_str` so a parse failure logs the raw body
/// (truncated for sanity) before bubbling. This used to be inline at
/// every callsite, which meant decode failures were a black box —
/// the surfaced error was just `serde_json`'s "duplicate field foo"
/// or "missing field bar" with no way to inspect what HF actually
/// sent. Centralising it keeps the device-flow / token / userinfo /
/// inference paths uniformly debuggable.
pub(crate) fn decode_body<T: DeserializeOwned>(endpoint: &str, body: &str) -> Result<T, CoderError> {
	match serde_json::from_str::<T>(body) {
		Ok(value) => Ok(value),
		Err(err) => {
			// `body` may contain the user's email (userinfo) or a
			// fresh access token (token endpoint). We log it anyway
			// because (a) the alternative is debugging blind, (b)
			// this only fires when HF returns something we don't
			// understand — i.e. a real failure mode, not the happy
			// path. Filter `moon_coder=warn` if you don't want it.
			tracing::warn!(
				endpoint,
				error = %err,
				body = %truncate_for_log(body),
				"failed to decode response body",
			);
			Err(CoderError::decode(endpoint, err.to_string()))
		}
	}
}

/// Cap log lines at ~4 kB so a stray HTML error page from a proxy
/// doesn't flood the trace. Truncates on a char boundary.
fn truncate_for_log(body: &str) -> String {
	const MAX: usize = 4_000;
	if body.len() <= MAX {
		return body.to_owned();
	}
	let mut end = MAX;
	while !body.is_char_boundary(end) {
		end -= 1;
	}
	format!("{}…[truncated, {} bytes total]", &body[..end], body.len())
}

fn unix_now() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

fn is_expired(bundle: &TokenBundle) -> bool {
	unix_now() >= bundle.expires_at_unix
}

fn is_near_expiry(bundle: &TokenBundle) -> bool {
	unix_now().saturating_add(REFRESH_LEAD_TIME_SECS) >= bundle.expires_at_unix
}

#[cfg(test)]
mod tests {
	use super::*;

	// Round-trip pins the bidirectional-rename trap: HF's wire shape
	// uses `preferred_username`/`picture`/`canPay`/`roleInOrg`/
	// `isEnterprise`, the TS side reads our Rust field names, and a
	// bare `rename = "…"` would emit the wire names on the outbound
	// serialize path — leaving every renamed field `undefined` on the
	// frontend. Renames must be `rename(deserialize = "…")`.
	#[test]
	fn hf_org_serializes_with_rust_field_names() {
		let wire = r#"{
			"name": "Hugging Face",
			"preferred_username": "huggingface",
			"picture": "https://example.test/avatar.png",
			"canPay": true,
			"roleInOrg": "admin",
			"isEnterprise": false
		}"#;
		let org: HfOrg = serde_json::from_str(wire).expect("parses wire shape");
		assert_eq!(org.slug.as_deref(), Some("huggingface"));
		assert_eq!(org.avatar_url.as_deref(), Some("https://example.test/avatar.png"));
		assert!(org.can_pay);
		assert_eq!(org.role_in_org.as_deref(), Some("admin"));
		assert!(!org.is_enterprise);

		let out = serde_json::to_value(&org).expect("serialises");
		// Outbound path uses Rust field names so the frontend (which
		// expects `slug`, `avatar_url`, `can_pay`, `role_in_org`,
		// `is_enterprise`) sees populated fields.
		assert_eq!(out["slug"], "huggingface");
		assert_eq!(out["avatar_url"], "https://example.test/avatar.png");
		assert_eq!(out["can_pay"], true);
		assert_eq!(out["role_in_org"], "admin");
		assert_eq!(out["is_enterprise"], false);
		// Wire names must not leak through on the outbound side.
		assert!(out.get("preferred_username").is_none(), "wire name leaked");
		assert!(out.get("canPay").is_none(), "wire name leaked");
	}

	#[test]
	fn hf_identity_serializes_with_rust_field_names() {
		let wire = r#"{
			"sub": "abc",
			"preferred_username": "coyotte508",
			"name": "Eliott Coyac",
			"picture": "https://example.test/me.png"
		}"#;
		let id: HfIdentity = serde_json::from_str(wire).expect("parses wire shape");
		assert_eq!(id.username, "coyotte508");
		assert_eq!(id.avatar_url.as_deref(), Some("https://example.test/me.png"));

		let out = serde_json::to_value(&id).expect("serialises");
		assert_eq!(out["username"], "coyotte508");
		assert_eq!(out["avatar_url"], "https://example.test/me.png");
		assert!(out.get("preferred_username").is_none());
		assert!(out.get("picture").is_none());
	}
}
