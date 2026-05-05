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
	#[error("HTTP {status} from {endpoint}: {body}")]
	Http {
		endpoint: String,
		status: u16,
		body: String,
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
	pub fn http(endpoint: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
		Self::Http {
			endpoint: endpoint.into(),
			status,
			body: body.into(),
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
