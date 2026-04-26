use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// All protocol errors share this shape so the UI can render them uniformly.
#[derive(Debug, Clone, Serialize, Deserialize, TS, thiserror::Error)]
#[serde(tag = "code", content = "message")]
#[ts(export)]
pub enum MoonError {
	#[error("not found: {0}")]
	NotFound(String),

	#[error("io error: {0}")]
	IoError(String),

	#[error("permission denied: {0}")]
	PermissionDenied(String),

	/// The remote workspace host (devcontainer agent, ssh agent, ...) is
	/// unreachable. UI should surface a "disconnected" state, not crash.
	#[error("host unavailable: {0}")]
	HostUnavailable(String),

	#[error("invalid argument: {0}")]
	InvalidArgument(String),

	#[error("internal error: {0}")]
	Internal(String),
}

impl MoonError {
	pub fn io(err: impl std::fmt::Display) -> Self {
		Self::IoError(err.to_string())
	}

	pub fn internal(msg: impl Into<String>) -> Self {
		Self::Internal(msg.into())
	}

	pub fn invalid(msg: impl Into<String>) -> Self {
		Self::InvalidArgument(msg.into())
	}
}

impl From<std::io::Error> for MoonError {
	fn from(err: std::io::Error) -> Self {
		match err.kind() {
			std::io::ErrorKind::NotFound => Self::NotFound(err.to_string()),
			std::io::ErrorKind::PermissionDenied => Self::PermissionDenied(err.to_string()),
			_ => Self::IoError(err.to_string()),
		}
	}
}
