//! Error type for the Slack client.
//!
//! Mapped to [`moon_protocol::MoonError`] at the Tauri command boundary so the
//! frontend renders Slack failures in the same toast/dialog pipeline as fs and
//! search errors. The split is deliberate: `SlackError` carries Slack-specific
//! context (the API method that failed, the wire-level error code) so backend
//! callers can branch on `Auth` vs `RateLimited`, while `MoonError` flattens
//! everything into one display string for the UI.

use moon_protocol::MoonError;

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
	/// Networking / TLS / DNS failure. The user is offline or Slack is down.
	#[error("network error: {0}")]
	Transport(String),

	/// Slack returned a non-2xx HTTP status. Distinct from `Api` because
	/// these usually mean "Slack itself is unhealthy" rather than "your
	/// request was malformed".
	#[error("Slack returned HTTP {status}: {body}")]
	Http { status: u16, body: String },

	/// Slack returned `{ "ok": false, "error": "..." }`. The `code` is the
	/// raw Slack error string (`invalid_auth`, `not_authed`, `missing_scope`,
	/// `account_inactive`, `token_revoked`, `ratelimited`, ...). Callers
	/// should match on it to decide whether to clear the keyring entry.
	///
	/// `needed` carries Slack's `needed` envelope field when the error is
	/// `missing_scope` (Slack's API tells us exactly which scope it
	/// wanted). Surfacing it in the Display string turns
	/// `missing_scope` from a head-scratcher into "right, add
	/// `im:history` and reinstall".
	#[error("Slack API error ({method}): {code}{}", needed.as_ref().map(|n| format!(" (need {n})")).unwrap_or_default())]
	Api {
		method: String,
		code: String,
		needed: Option<String>,
	},

	/// JSON parse error. Either a Slack response shape changed on us, or
	/// the response body wasn't JSON at all.
	#[error("could not parse Slack response: {0}")]
	Decode(String),

	/// OS keyring (libsecret / Keychain / Credential Manager) failed.
	/// Wrapped as a string so callers don't pull in `keyring` themselves.
	#[error("keyring error: {0}")]
	Keyring(String),
}

impl SlackError {
	/// True for the family of errors that mean "the stored token is no
	/// longer valid". Callers should `TokenStore::clear()` in response.
	pub fn is_auth_failure(&self) -> bool {
		match self {
			Self::Api { code, .. } => matches!(
				code.as_str(),
				"invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" | "token_expired"
			),
			_ => false,
		}
	}
}

impl From<keyring::Error> for SlackError {
	fn from(err: keyring::Error) -> Self {
		Self::Keyring(err.to_string())
	}
}

impl From<SlackError> for MoonError {
	fn from(err: SlackError) -> Self {
		match err {
			SlackError::Transport(msg) => Self::HostUnavailable(msg),
			SlackError::Http { .. } => Self::HostUnavailable(err.to_string()),
			SlackError::Api { .. } => Self::Internal(err.to_string()),
			SlackError::Decode(_) => Self::Internal(err.to_string()),
			SlackError::Keyring(_) => Self::Internal(err.to_string()),
		}
	}
}
