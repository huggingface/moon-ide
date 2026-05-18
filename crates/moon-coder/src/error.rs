//! Error type for moon-coder.
//!
//! Mapped to [`moon_protocol::MoonError`] at the Tauri command boundary
//! so the UI renders coder failures in the same toast pipeline as
//! every other command. The split is deliberate (mirror of the slack
//! crate): `CoderError` carries provider-specific context for backend
//! callers, `MoonError` flattens to a single display string.

use moon_protocol::MoonError;

#[derive(Debug, thiserror::Error)]
pub enum CoderError {
	/// Network / TLS / DNS failure on the way to either huggingface.co
	/// or the inference router. Caller should treat these as transient
	/// and surface "couldn't reach Hugging Face" to the user.
	#[error("network error: {0}")]
	Transport(String),

	/// Non-2xx HTTP status from any HF endpoint. Carries the failing
	/// URL + body so the connect modal can show a meaningful message
	/// during the device flow ("invalid_client", "expired_token", …).
	///
	/// `request_id` is the `x-request-id` response header when the
	/// server sent one. Hub-side support traces requests by that
	/// id, so leaking it into the user-visible error string makes
	/// "please attach the request id" a one-copy operation.
	#[error("HTTP {status} from {endpoint}: {body}")]
	Http {
		endpoint: String,
		status: u16,
		body: String,
		request_id: Option<String>,
	},

	/// JSON decode failure (or a response shape we didn't expect).
	#[error("could not parse response from {endpoint}: {message}")]
	Decode { endpoint: String, message: String },

	/// User isn't signed in (no token in keyring, refresh failed).
	/// The UI should re-prompt with the device flow.
	#[error("not signed in")]
	NotSignedIn,

	/// Device flow has been pending for too long without the user
	/// completing the consent screen. Authenticator gives up after
	/// the `expires_in` window from the device-code response.
	#[error("device-code authorization expired before user approved")]
	DeviceFlowExpired,

	/// User actively denied the consent screen. Distinguished from
	/// `DeviceFlowExpired` so the UI can show a "you said no" message
	/// rather than "we timed out".
	#[error("device-code authorization was denied by the user")]
	DeviceFlowDenied,

	/// OS keyring (libsecret / Keychain / Credential Manager) failed.
	/// Wrapped as a string so callers don't pull in `keyring` themselves.
	#[error("keyring error: {0}")]
	Keyring(String),

	/// No active workspace folder when a tool call needs one. The UI
	/// should never let the panel be open without a folder, but the
	/// loop checks anyway because tools are dispatched out-of-band.
	#[error("no active workspace folder")]
	NoActiveFolder,

	/// A tool the LLM asked for isn't registered. The loop reports
	/// this back to the model as `isError: true` content rather than
	/// failing the whole turn.
	#[error("unknown tool: {0}")]
	UnknownTool(String),

	/// Tool arguments didn't match the declared schema.
	#[error("invalid tool arguments for {tool}: {message}")]
	InvalidToolArgs { tool: String, message: String },

	/// Tool failed during execution. Propagated back to the LLM as
	/// `isError: true` content; the model retries or explains.
	#[error("tool {tool} failed: {message}")]
	ToolFailed { tool: String, message: String },

	/// A mutating tool was called from a sub-agent running in
	/// `Research` mode. Surfaces back to the model as the tool's
	/// `is_error: true` result so it learns to either spawn a
	/// `coder`-mode sub-agent for the edit or report findings only.
	/// Lives at the dispatch boundary as a hard gate (rather than
	/// just a system-prompt instruction) so a confused model can't
	/// silently mutate when the parent told the sub-agent not to.
	#[error("tool {tool} not available in research mode (read-only sub-agent)")]
	ReadOnlyMode { tool: String },

	/// User aborted the turn via the panel's stop button / Esc. The
	/// loop returns this as the `Err(_)` of the `prompt` future so
	/// the caller can emit `CoderEvent::Aborted` with no further
	/// fanfare.
	#[error("turn was aborted")]
	Aborted,

	/// Errors propagated from the workspace host (filesystem ops,
	/// editorconfig, git). Pre-flattened to a string so the loop's
	/// per-tool error path stays uniform.
	#[error("host error: {0}")]
	Host(String),

	/// Catch-all for unexpected internal state.
	#[error("internal error: {0}")]
	Internal(String),
}

impl CoderError {
	pub fn http(endpoint: impl Into<String>, status: u16, body: impl Into<String>, request_id: Option<String>) -> Self {
		// We bake the request id into the body string so the
		// `Display` impl carries it without a custom format; the
		// structured `request_id` field is preserved for callers
		// that want to grab it programmatically. Empty / missing
		// ids are dropped (no `[request id: ]` clutter).
		let raw_body = body.into();
		let trimmed = request_id.as_deref().map(str::trim).filter(|id| !id.is_empty());
		let display_body = match trimmed {
			Some(id) => format!("{raw_body} [request id: {id}]"),
			None => raw_body,
		};
		Self::Http {
			endpoint: endpoint.into(),
			status,
			body: display_body,
			request_id,
		}
	}

	pub fn decode(endpoint: impl Into<String>, message: impl Into<String>) -> Self {
		Self::Decode {
			endpoint: endpoint.into(),
			message: message.into(),
		}
	}

	pub fn invalid_args(tool: impl Into<String>, message: impl Into<String>) -> Self {
		Self::InvalidToolArgs {
			tool: tool.into(),
			message: message.into(),
		}
	}

	pub fn tool_failed(tool: impl Into<String>, message: impl Into<String>) -> Self {
		Self::ToolFailed {
			tool: tool.into(),
			message: message.into(),
		}
	}

	pub fn read_only_mode(tool: impl Into<String>) -> Self {
		Self::ReadOnlyMode { tool: tool.into() }
	}
}

/// Extract the HF Hub's `x-request-id` response header, when
/// present. The Hub stamps every API response with a trace handle
/// — propagating it into [`CoderError::Http`] lets users hand the
/// id directly to HF support rather than having us re-derive the
/// failing call from a timestamp.
///
/// Returned as an owned `String` so the caller can consume the
/// response (e.g. via `.text().await`) without juggling header
/// lifetimes.
pub fn request_id_of(response: &reqwest::Response) -> Option<String> {
	response
		.headers()
		.get("x-request-id")
		.and_then(|v| v.to_str().ok())
		.map(|s| s.to_string())
}

impl From<reqwest::Error> for CoderError {
	fn from(err: reqwest::Error) -> Self {
		Self::Transport(err.to_string())
	}
}

impl From<keyring::Error> for CoderError {
	fn from(err: keyring::Error) -> Self {
		Self::Keyring(err.to_string())
	}
}

impl From<std::io::Error> for CoderError {
	fn from(err: std::io::Error) -> Self {
		// I/O errors at this layer come from session storage
		// (`tokio::fs::*`). The host classification is the closest
		// match — these are filesystem failures, not network
		// failures, and the `MoonError` flattening downstream maps
		// `Host` onto `Internal` which is what the panel expects.
		Self::Host(err.to_string())
	}
}

impl From<serde_json::Error> for CoderError {
	fn from(err: serde_json::Error) -> Self {
		Self::Internal(format!("serde_json: {err}"))
	}
}

impl From<MoonError> for CoderError {
	fn from(err: MoonError) -> Self {
		Self::Host(err.to_string())
	}
}

impl From<CoderError> for MoonError {
	fn from(err: CoderError) -> Self {
		match err {
			CoderError::Transport(_) | CoderError::Http { .. } => Self::HostUnavailable(err.to_string()),
			CoderError::NotSignedIn
			| CoderError::DeviceFlowExpired
			| CoderError::DeviceFlowDenied
			| CoderError::NoActiveFolder => Self::invalid(err.to_string()),
			CoderError::Aborted => Self::invalid(err.to_string()),
			_ => Self::Internal(err.to_string()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn http_error_appends_request_id_when_present() {
		let err = CoderError::http("https://hf.co/api/x", 500, "boom", Some("req_abc".into()));
		let display = err.to_string();
		assert!(display.contains("boom"), "body should be preserved: {display}");
		assert!(
			display.contains("[request id: req_abc]"),
			"display should mention the request id: {display}"
		);

		match err {
			CoderError::Http { request_id, .. } => {
				assert_eq!(request_id.as_deref(), Some("req_abc"));
			}
			_ => panic!("expected CoderError::Http"),
		}
	}

	#[test]
	fn http_error_without_request_id_keeps_body_clean() {
		let err = CoderError::http("https://hf.co/api/x", 401, "unauthorised", None);
		let display = err.to_string();
		assert!(display.contains("unauthorised"));
		assert!(
			!display.contains("[request id:"),
			"should not emit the marker when no id is known: {display}"
		);
	}

	#[test]
	fn http_error_treats_empty_request_id_as_missing() {
		// Hub occasionally sends back an empty header value;
		// we shouldn't render `[request id: ]` for it.
		let err = CoderError::http("https://hf.co/api/x", 502, "bad gateway", Some("   ".into()));
		assert!(!err.to_string().contains("[request id:"));
	}
}
