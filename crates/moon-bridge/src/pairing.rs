//! Pairing for the mobile companion (Phase 13.3, brought up early).
//!
//! Two pieces, deliberately split so the token logic is unit-testable
//! without a keyring or a network stack:
//!
//! - [`PairingSession`] — the short-lived, single-use pairing code a
//!   phone presents during the pair handshake. Pure in-memory state:
//!   issue a code, verify-and-consume it (rejecting expired or
//!   already-used codes). No I/O.
//! - [`DeviceStore`] — the long-lived per-device tokens minted on a
//!   successful pair, persisted in the OS keyring (`service=moon-ide`,
//!   `account=companion-devices`), with list + revoke.
//!
//! Pairing is the **whole** security boundary for the companion: a
//! paired device can drive the coder, which can run anything via its
//! `bash` tool, so there is no method-level fence behind it (see
//! `specs/companion.md`). The token below is therefore a
//! bearer credential — treat it like the HF / Slack tokens already in
//! the keyring.
//!
//! What's intentionally *not* here yet (lands with 13.2 / 13.3 proper):
//! the WSS listener that checks a presented device token, the QR
//! payload encoder, and the TLS cert. This module is the credential
//! core those build on.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long an issued pairing code stays valid. Device-flow-style
/// short window — long enough to scan a QR and tap "pair", short
/// enough that a leaked code is useless by the time anyone finds it.
pub const PAIRING_CODE_TTL: Duration = Duration::from_secs(120);

/// Keyring coordinates for the device registry. One JSON blob holds
/// every paired device, mirroring how `moon-coder` stores its OAuth
/// triple as a single keyring entry.
const KEYRING_SERVICE: &str = "moon-ide";
const KEYRING_ACCOUNT: &str = "companion-devices";

/// Unix-epoch milliseconds now. Pulled out so tests can reason about
/// expiry without sleeping.
fn now_ms() -> u128 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("system clock before 1970")
		.as_millis()
}

/// A short, human-presentable pairing code plus its expiry. Lives in
/// the bridge process's memory only — never persisted, never logged.
#[derive(Debug, Clone)]
pub struct PairingSession {
	code: String,
	expires_at_ms: u128,
	consumed: bool,
}

impl PairingSession {
	/// Issue a fresh pairing code valid for [`PAIRING_CODE_TTL`].
	///
	/// The code is the first 8 hex chars of a v4 UUID, upper-cased
	/// and split into two 4-char groups (`A1B2-C3D4`) — enough
	/// entropy that guessing one inside a 120 s window is hopeless,
	/// short enough to type if QR scanning fails.
	pub fn issue() -> Self {
		Self::issue_with_ttl(PAIRING_CODE_TTL)
	}

	fn issue_with_ttl(ttl: Duration) -> Self {
		let raw = uuid::Uuid::new_v4().simple().to_string();
		let head = raw[..8].to_uppercase();
		let code = format!("{}-{}", &head[..4], &head[4..]);
		Self {
			code,
			expires_at_ms: now_ms() + ttl.as_millis(),
			consumed: false,
		}
	}

	/// The code to show the user (in the QR payload and as fallback
	/// type-in text).
	pub fn code(&self) -> &str {
		&self.code
	}

	/// True once the code has expired against the wall clock.
	pub fn is_expired(&self) -> bool {
		now_ms() >= self.expires_at_ms
	}

	/// Verify a presented code and consume the session on success.
	/// Single-use: a second correct presentation fails with
	/// [`PairError::AlreadyUsed`]. An expired session fails with
	/// [`PairError::Expired`] regardless of code correctness.
	///
	/// Called by the WSS listener (13.2), which doesn't exist yet —
	/// the `allow` keeps the credential core landing now without a
	/// warning, and is removed the moment the listener wires it in.
	#[allow(dead_code)]
	pub fn verify_and_consume(&mut self, presented: &str) -> Result<(), PairError> {
		if self.consumed {
			return Err(PairError::AlreadyUsed);
		}
		if self.is_expired() {
			return Err(PairError::Expired);
		}
		// Case-insensitive compare so a user typing the fallback
		// code in lowercase still pairs; the canonical form is upper.
		if !presented.trim().eq_ignore_ascii_case(&self.code) {
			return Err(PairError::CodeMismatch);
		}
		self.consumed = true;
		Ok(())
	}
}

/// Why a pairing attempt failed. Surfaced to the phone so it can show
/// the right message ("code expired, scan again" vs "wrong code").
/// Consumed by the WSS listener (13.2); see `verify_and_consume`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
	#[error("pairing code has expired")]
	Expired,
	#[error("pairing code already used")]
	AlreadyUsed,
	#[error("pairing code did not match")]
	CodeMismatch,
}

/// One paired device's record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedDevice {
	/// Stable opaque id for the device (UI shows this in the
	/// paired-devices list; revoke takes it).
	pub id: String,
	/// User-facing label the phone sends at pair time ("Eli's iPhone").
	pub label: String,
	/// The bearer token this device presents on every connection.
	/// Long, random, never shown again after issue.
	pub token: String,
	/// When the device paired, Unix-epoch ms.
	pub paired_at_ms: u128,
}

impl PairedDevice {
	/// Mint a new device record with a fresh random token. The token
	/// is two concatenated v4 UUIDs (256 bits of entropy), hex, no
	/// dashes — opaque to everything but an equality check.
	pub fn mint(label: impl Into<String>) -> Self {
		let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
		Self {
			id: uuid::Uuid::new_v4().simple().to_string(),
			label: label.into(),
			token,
			paired_at_ms: now_ms(),
		}
	}
}

/// Keyring-backed registry of paired devices. The whole list is one
/// JSON blob under a single keyring entry — cheap, and atomic enough
/// for the handful of devices a single user pairs.
pub struct DeviceStore {
	entry: keyring::Entry,
}

impl DeviceStore {
	/// Open the keyring-backed store. Fails only if the platform
	/// keyring can't be reached at all.
	pub fn open() -> anyhow::Result<Self> {
		let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
		Ok(Self { entry })
	}

	/// Load the current device list. A missing entry (nobody's paired
	/// yet) is an empty list, not an error.
	pub fn list(&self) -> anyhow::Result<Vec<PairedDevice>> {
		match self.entry.get_password() {
			Ok(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
			Err(keyring::Error::NoEntry) => Ok(Vec::new()),
			Err(err) => Err(err.into()),
		}
	}

	/// Persist the full device list back to the keyring.
	fn save(&self, devices: &[PairedDevice]) -> anyhow::Result<()> {
		let json = serde_json::to_string(devices)?;
		self.entry.set_password(&json)?;
		Ok(())
	}

	/// Add a freshly-minted device and persist. Returns the stored
	/// record (its token is the caller's to hand back to the phone
	/// exactly once).
	pub fn add(&self, device: PairedDevice) -> anyhow::Result<PairedDevice> {
		let mut devices = self.list()?;
		devices.push(device.clone());
		self.save(&devices)?;
		Ok(device)
	}

	/// Revoke a device by id. Returns true if a device was removed,
	/// false if the id was unknown (already revoked / never existed).
	pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
		let mut devices = self.list()?;
		let before = devices.len();
		devices.retain(|d| d.id != id);
		let removed = devices.len() != before;
		if removed {
			self.save(&devices)?;
		}
		Ok(removed)
	}

	/// Resolve a presented bearer token to the matching device, if
	/// any. This is the check the WSS listener (13.2) runs on every
	/// connection. Constant-time-ish comparison isn't worth it here —
	/// the token space is 256 bits, so a timing oracle buys nothing.
	///
	/// Wired in by the listener (13.2); `allow` until then.
	#[allow(dead_code)]
	pub fn device_for_token(&self, token: &str) -> anyhow::Result<Option<PairedDevice>> {
		Ok(self.list()?.into_iter().find(|d| d.token == token))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn issued_code_is_grouped_hex() {
		let s = PairingSession::issue();
		let code = s.code();
		// Shape: XXXX-XXXX, all uppercase hex.
		assert_eq!(code.len(), 9);
		assert_eq!(&code[4..5], "-");
		assert!(code
			.chars()
			.filter(|c| *c != '-')
			.all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
	}

	#[test]
	fn verify_consumes_single_use() {
		let mut s = PairingSession::issue();
		let code = s.code().to_string();
		assert_eq!(s.verify_and_consume(&code), Ok(()));
		// Second attempt with the right code is rejected.
		assert_eq!(s.verify_and_consume(&code), Err(PairError::AlreadyUsed));
	}

	#[test]
	fn verify_is_case_insensitive_and_trims() {
		let mut s = PairingSession::issue();
		let code = s.code().to_lowercase();
		assert_eq!(s.verify_and_consume(&format!("  {code}  ")), Ok(()));
	}

	#[test]
	fn wrong_code_rejected_without_consuming() {
		let mut s = PairingSession::issue();
		assert_eq!(s.verify_and_consume("0000-0000"), Err(PairError::CodeMismatch));
		// A mismatch must not consume the session — the real code
		// still works afterwards.
		let code = s.code().to_string();
		assert_eq!(s.verify_and_consume(&code), Ok(()));
	}

	#[test]
	fn expired_code_rejected() {
		let mut s = PairingSession::issue_with_ttl(Duration::from_millis(0));
		let code = s.code().to_string();
		assert_eq!(s.verify_and_consume(&code), Err(PairError::Expired));
	}

	#[test]
	fn minted_devices_have_distinct_tokens_and_ids() {
		let a = PairedDevice::mint("phone A");
		let b = PairedDevice::mint("phone B");
		assert_ne!(a.token, b.token);
		assert_ne!(a.id, b.id);
		assert_eq!(a.token.len(), 64); // two 32-char simple UUIDs
	}
}
