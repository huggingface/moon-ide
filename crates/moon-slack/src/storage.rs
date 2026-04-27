//! OS keyring wrapper for the Slack user token.
//!
//! Per `specs/slack-chat.md`, the `xoxp-` user OAuth token is a
//! credential and lives in the OS-native keyring (libsecret on Linux,
//! Keychain on macOS, Credential Manager on Windows). Never in
//! `app_state.json`, never in any session blob.
//!
//! This module is a thin shim around `keyring::Entry`. The Tauri layer
//! depends only on this struct, not on `keyring` directly.

use crate::error::SlackError;

const SERVICE: &str = "moon-ide";
const ACCOUNT_USER_TOKEN: &str = "slack-user-token";

/// Token storage handle. Stateless — held by the Tauri app state for
/// dependency-injection convenience and to keep the keyring service /
/// account names in one place.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenStore;

impl TokenStore {
	pub const fn new() -> Self {
		Self
	}

	pub fn save(&self, token: &str) -> Result<(), SlackError> {
		let entry = keyring::Entry::new(SERVICE, ACCOUNT_USER_TOKEN)?;
		entry.set_password(token)?;
		Ok(())
	}

	pub fn load(&self) -> Result<Option<String>, SlackError> {
		let entry = keyring::Entry::new(SERVICE, ACCOUNT_USER_TOKEN)?;
		match entry.get_password() {
			Ok(token) => Ok(Some(token)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(err) => Err(err.into()),
		}
	}

	pub fn clear(&self) -> Result<(), SlackError> {
		let entry = keyring::Entry::new(SERVICE, ACCOUNT_USER_TOKEN)?;
		match entry.delete_credential() {
			Ok(()) => Ok(()),
			Err(keyring::Error::NoEntry) => Ok(()),
			Err(err) => Err(err.into()),
		}
	}
}
