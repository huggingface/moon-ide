//! Sign-in and token custody for `kind=chat_gpt` providers (ADR 0101).
//!
//! The login is the Codex CLI's device-code flow against
//! `auth.openai.com`, run by the workspace process so it works the same
//! for local and remote workspaces. The resulting bundle (access +
//! refresh token, expiry, account id) is stored as JSON in the
//! provider's regular keyring slot (`coder-provider:<id>`), so
//! `has_api_key` doubles as "signed in" and deleting the provider drops
//! the credentials with no extra plumbing.
//!
//! Refresh tokens rotate and are single-use. Sibling workspace
//! processes on one host share the keyring slot, so a refresh always
//! starts from the freshly re-read keyring entry, and a rejected
//! refresh re-reads once more before giving up: a sibling may have
//! rotated the token between our read and our request.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use moon_protocol::coder_models::{ChatGptAccount, ChatGptDeviceCode};
use serde::{Deserialize, Serialize};

use crate::auth::decode_body;
use crate::error::{request_id_of, CoderError};
use crate::providers::ProviderKeyring;

/// The Codex CLI's public OAuth client. Used by every third-party
/// harness on this route (OpenCode, Pi, OpenClaw).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const AUTH_CLAIMS: &str = "https://api.openai.com/auth";

/// Refresh this long before the access token's expiry, matching the
/// Codex CLI's window.
const REFRESH_LEAD_SECS: u64 = 5 * 60;
/// The device code is abandoned after this long, as the Codex CLI does.
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What the keyring slot holds for a `chat_gpt` provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ChatGptTokens {
	pub(crate) access_token: String,
	pub(crate) refresh_token: String,
	pub(crate) expires_at: u64,
	pub(crate) account_id: String,
	#[serde(default)]
	pub(crate) email: Option<String>,
	#[serde(default)]
	pub(crate) plan_type: Option<String>,
}

impl ChatGptTokens {
	fn needs_refresh(&self) -> bool {
		unix_now().saturating_add(REFRESH_LEAD_SECS) >= self.expires_at
	}

	pub(crate) fn account(&self) -> ChatGptAccount {
		ChatGptAccount {
			email: self.email.clone(),
			plan_type: self.plan_type.clone(),
		}
	}
}

/// What one request needs to authenticate against the Codex backend.
#[derive(Debug, Clone)]
pub(crate) struct ChatGptCredentials {
	pub(crate) access_token: String,
	pub(crate) account_id: String,
}

/// Token custody for every `chat_gpt` provider in this process. Cheap
/// to clone.
#[derive(Clone)]
pub struct ChatGptAuth {
	keys: ProviderKeyring,
	/// Serialises refreshes inside this process; the keyring re-read
	/// covers sibling processes.
	refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ChatGptAuth {
	pub fn new(keys: ProviderKeyring) -> Self {
		Self {
			keys,
			refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
		}
	}

	fn cached(&self, id: &str) -> Option<ChatGptTokens> {
		parse_bundle(id, self.keys.get(id)?)
	}

	fn reloaded(&self, id: &str) -> Option<ChatGptTokens> {
		match self.keys.reload(id) {
			Ok(raw) => parse_bundle(id, raw?),
			Err(err) => {
				tracing::warn!(error = %err, provider = %id, "could not re-read ChatGPT tokens from keyring");
				self.cached(id)
			}
		}
	}

	pub(crate) fn save(&self, id: &str, tokens: &ChatGptTokens) -> Result<(), CoderError> {
		let raw = serde_json::to_string(tokens).map_err(|err| CoderError::Internal(err.to_string()))?;
		self.keys.set(id, &raw)
	}

	pub(crate) fn account(&self, id: &str) -> Option<ChatGptAccount> {
		self.cached(id).map(|t| t.account())
	}

	/// A usable access token for provider `id`, refreshing when it is
	/// near expiry or when `force_refresh` (the backend just 401'd).
	pub(crate) async fn credentials(
		&self,
		http: &reqwest::Client,
		id: &str,
		force_refresh: bool,
	) -> Result<ChatGptCredentials, CoderError> {
		let current = self.cached(id).ok_or_else(signed_out)?;
		if !force_refresh && !current.needs_refresh() {
			return Ok(credentials_of(&current));
		}
		let _guard = self.refresh_lock.lock().await;
		let stored = self.reloaded(id).ok_or_else(signed_out)?;
		// A sibling process (or a concurrent task here) already rotated.
		if stored.access_token != current.access_token && !stored.needs_refresh() {
			return Ok(credentials_of(&stored));
		}
		match refresh(http, &stored).await {
			Ok(fresh) => {
				self.save(id, &fresh)?;
				Ok(credentials_of(&fresh))
			}
			Err(RefreshError::Rejected(body)) => {
				if let Some(rotated) = self.reloaded(id) {
					if rotated.refresh_token != stored.refresh_token {
						return Ok(credentials_of(&rotated));
					}
				}
				tracing::warn!(provider = %id, body = %body, "ChatGPT refresh rejected; clearing stored sign-in");
				let _ = self.keys.clear(id);
				Err(CoderError::Internal(
					"ChatGPT sign-in expired. Sign in again from the model settings.".into(),
				))
			}
			Err(RefreshError::Other(err)) => Err(err),
		}
	}
}

fn signed_out() -> CoderError {
	CoderError::Internal("ChatGPT provider is not signed in. Sign in from the model settings.".into())
}

fn credentials_of(tokens: &ChatGptTokens) -> ChatGptCredentials {
	ChatGptCredentials {
		access_token: tokens.access_token.clone(),
		account_id: tokens.account_id.clone(),
	}
}

fn parse_bundle(id: &str, raw: String) -> Option<ChatGptTokens> {
	match serde_json::from_str(&raw) {
		Ok(tokens) => Some(tokens),
		Err(err) => {
			tracing::warn!(error = %err, provider = %id, "keyring entry is not a ChatGPT token bundle");
			None
		}
	}
}

/// Ask for a device code. The user enters it at `verification_uri`.
pub(crate) async fn start_device_login(http: &reqwest::Client) -> Result<ChatGptDeviceCode, CoderError> {
	#[derive(Deserialize)]
	struct UserCode {
		device_auth_id: String,
		#[serde(alias = "usercode")]
		user_code: String,
		#[serde(default)]
		interval: Option<serde_json::Value>,
	}
	let endpoint = format!("{ISSUER}/api/accounts/deviceauth/usercode");
	let response = http
		.post(&endpoint)
		.timeout(AUTH_REQUEST_TIMEOUT)
		.json(&serde_json::json!({ "client_id": CLIENT_ID }))
		.send()
		.await
		.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(CoderError::from)?;
	if status == reqwest::StatusCode::NOT_FOUND {
		return Err(CoderError::Internal(
			"ChatGPT refused device-code sign-in. Enable device code login for Codex in ChatGPT's security settings, then retry."
				.into(),
		));
	}
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}
	let raw: UserCode = decode_body(&endpoint, &body)?;
	// The server sends the interval as a string; accept a number too.
	let interval = raw
		.interval
		.and_then(|v| match v {
			serde_json::Value::Number(n) => n.as_u64(),
			serde_json::Value::String(s) => s.trim().parse().ok(),
			_ => None,
		})
		.unwrap_or(5)
		.clamp(1, 60) as u32;
	Ok(ChatGptDeviceCode {
		device_auth_id: raw.device_auth_id,
		user_code: raw.user_code,
		verification_uri: DEVICE_VERIFICATION_URI.to_owned(),
		interval,
	})
}

/// Poll until the user approves `code`, then trade the authorization
/// code for tokens.
pub(crate) async fn poll_device_login(
	http: &reqwest::Client,
	code: &ChatGptDeviceCode,
) -> Result<ChatGptTokens, CoderError> {
	#[derive(Deserialize)]
	struct Approved {
		authorization_code: String,
		code_verifier: String,
	}
	let endpoint = format!("{ISSUER}/api/accounts/deviceauth/token");
	let deadline = Instant::now() + DEVICE_LOGIN_TIMEOUT;
	let mut interval = Duration::from_secs(u64::from(code.interval.max(1)));
	loop {
		if Instant::now() >= deadline {
			return Err(CoderError::DeviceFlowExpired);
		}
		tokio::time::sleep(interval).await;
		let response = http
			.post(&endpoint)
			.timeout(AUTH_REQUEST_TIMEOUT)
			.json(&serde_json::json!({
				"device_auth_id": code.device_auth_id,
				"user_code": code.user_code,
			}))
			.send()
			.await
			.map_err(CoderError::from)?;
		let status = response.status();
		let request_id = request_id_of(&response);
		let body = response.text().await.map_err(CoderError::from)?;
		if status.is_success() {
			let approved: Approved = decode_body(&endpoint, &body)?;
			return exchange_code(http, &approved.authorization_code, &approved.code_verifier).await;
		}
		if body.contains("slow_down") {
			interval += Duration::from_secs(5);
			continue;
		}
		let pending = status == reqwest::StatusCode::FORBIDDEN
			|| status == reqwest::StatusCode::NOT_FOUND
			|| body.contains("authorization_pending");
		if pending {
			continue;
		}
		if body.contains("access_denied") {
			return Err(CoderError::DeviceFlowDenied);
		}
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}
}

#[derive(Deserialize)]
struct TokenResponse {
	#[serde(default)]
	id_token: Option<String>,
	#[serde(default)]
	access_token: Option<String>,
	#[serde(default)]
	refresh_token: Option<String>,
	#[serde(default)]
	expires_in: Option<u64>,
}

async fn exchange_code(http: &reqwest::Client, code: &str, verifier: &str) -> Result<ChatGptTokens, CoderError> {
	let endpoint = format!("{ISSUER}/oauth/token");
	let response = http
		.post(&endpoint)
		.timeout(AUTH_REQUEST_TIMEOUT)
		.form(&[
			("grant_type", "authorization_code"),
			("code", code),
			("redirect_uri", DEVICE_REDIRECT_URI),
			("client_id", CLIENT_ID),
			("code_verifier", verifier),
		])
		.send()
		.await
		.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}
	let raw: TokenResponse = decode_body(&endpoint, &body)?;
	tokens_from_response(raw, None).ok_or_else(|| {
		CoderError::decode(
			endpoint,
			"token response lacked an access token, refresh token, or ChatGPT account id",
		)
	})
}

enum RefreshError {
	/// The token endpoint said no (rotated, expired, revoked). Body
	/// kept for the log.
	Rejected(String),
	Other(CoderError),
}

async fn refresh(http: &reqwest::Client, stored: &ChatGptTokens) -> Result<ChatGptTokens, RefreshError> {
	let endpoint = format!("{ISSUER}/oauth/token");
	let response = http
		.post(&endpoint)
		.timeout(AUTH_REQUEST_TIMEOUT)
		.form(&[
			("grant_type", "refresh_token"),
			("refresh_token", stored.refresh_token.as_str()),
			("client_id", CLIENT_ID),
		])
		.send()
		.await
		.map_err(|err| RefreshError::Other(err.into()))?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(|err| RefreshError::Other(err.into()))?;
	if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED {
		return Err(RefreshError::Rejected(body));
	}
	if !status.is_success() {
		return Err(RefreshError::Other(CoderError::http(
			endpoint,
			status.as_u16(),
			body,
			request_id,
		)));
	}
	let raw: TokenResponse = decode_body(&endpoint, &body).map_err(RefreshError::Other)?;
	tokens_from_response(raw, Some(stored))
		.ok_or_else(|| RefreshError::Other(CoderError::decode(endpoint, "refresh response lacked an access token")))
}

/// Build a bundle from a token response. On refresh, fields the
/// response omits keep their `previous` value.
fn tokens_from_response(raw: TokenResponse, previous: Option<&ChatGptTokens>) -> Option<ChatGptTokens> {
	let access_token = raw.access_token?;
	let refresh_token = raw
		.refresh_token
		.or_else(|| previous.map(|p| p.refresh_token.clone()))?;
	let id_claims = raw.id_token.as_deref().and_then(jwt_claims);
	let access_claims = jwt_claims(&access_token);
	let claims = [id_claims.as_ref(), access_claims.as_ref()];
	let account_id = claims
		.iter()
		.flatten()
		.find_map(|c| account_id_of(c))
		.or_else(|| previous.map(|p| p.account_id.clone()))?;
	let plan_type = claims
		.iter()
		.flatten()
		.find_map(|c| {
			c.get(AUTH_CLAIMS)?
				.get("chatgpt_plan_type")?
				.as_str()
				.map(str::to_owned)
		})
		.or_else(|| previous.and_then(|p| p.plan_type.clone()));
	let email = id_claims
		.as_ref()
		.and_then(|c| c.get("email")?.as_str().map(str::to_owned))
		.or_else(|| previous.and_then(|p| p.email.clone()));
	let expires_at = raw
		.expires_in
		.map(|secs| unix_now().saturating_add(secs))
		.or_else(|| access_claims.as_ref()?.get("exp")?.as_u64())
		.unwrap_or_else(|| unix_now().saturating_add(3600));
	Some(ChatGptTokens {
		access_token,
		refresh_token,
		expires_at,
		account_id,
		email,
		plan_type,
	})
}

fn account_id_of(claims: &serde_json::Value) -> Option<String> {
	let auth = claims.get(AUTH_CLAIMS);
	auth
		.and_then(|a| a.get("chatgpt_account_id"))
		.or_else(|| claims.get("chatgpt_account_id"))
		.and_then(|v| v.as_str())
		.filter(|s| !s.is_empty())
		.map(str::to_owned)
		.or_else(|| {
			claims
				.get("organizations")?
				.as_array()?
				.first()?
				.get("id")?
				.as_str()
				.map(str::to_owned)
		})
}

/// Payload of a JWT, unverified: we only read our own token's claims.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
	let payload = token.split('.').nth(1)?;
	let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
		.decode(payload.trim_end_matches('='))
		.ok()?;
	serde_json::from_slice(&bytes).ok()
}

fn unix_now() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn jwt(claims: serde_json::Value) -> String {
		let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
		format!(
			"{}.{}.sig",
			enc.encode(br#"{"alg":"none"}"#),
			enc.encode(claims.to_string())
		)
	}

	#[test]
	fn reads_account_plan_and_email_from_claims() {
		let id_token = jwt(serde_json::json!({
			"email": "dev@example.com",
			"https://api.openai.com/auth": { "chatgpt_account_id": "acct-1", "chatgpt_plan_type": "pro" },
		}));
		let raw = TokenResponse {
			id_token: Some(id_token),
			access_token: Some(jwt(serde_json::json!({ "exp": 4_000_000_000u64 }))),
			refresh_token: Some("rt".into()),
			expires_in: None,
		};
		let tokens = tokens_from_response(raw, None).unwrap();
		assert_eq!(tokens.account_id, "acct-1");
		assert_eq!(tokens.plan_type.as_deref(), Some("pro"));
		assert_eq!(tokens.email.as_deref(), Some("dev@example.com"));
		assert_eq!(tokens.expires_at, 4_000_000_000);
	}

	#[test]
	fn refresh_keeps_fields_the_response_omits() {
		let previous = ChatGptTokens {
			access_token: "old".into(),
			refresh_token: "rt-old".into(),
			expires_at: 0,
			account_id: "acct-1".into(),
			email: Some("dev@example.com".into()),
			plan_type: Some("plus".into()),
		};
		let raw = TokenResponse {
			id_token: None,
			access_token: Some("opaque-access".into()),
			refresh_token: None,
			expires_in: Some(3600),
		};
		let tokens = tokens_from_response(raw, Some(&previous)).unwrap();
		assert_eq!(tokens.access_token, "opaque-access");
		assert_eq!(tokens.refresh_token, "rt-old");
		assert_eq!(tokens.account_id, "acct-1");
		assert_eq!(tokens.plan_type.as_deref(), Some("plus"));
		assert!(tokens.expires_at > unix_now());
	}

	#[test]
	fn missing_account_id_is_rejected_on_first_sign_in() {
		let raw = TokenResponse {
			id_token: None,
			access_token: Some("opaque".into()),
			refresh_token: Some("rt".into()),
			expires_in: Some(3600),
		};
		assert!(tokens_from_response(raw, None).is_none());
	}
}
