//! IDE enrollment for the remote / relay bridge (Phase 14.0).
//!
//! Symmetric counterpart to [`crate::pairing`]. Phase 13 paired
//! *phones* to a host-local bridge; Phase 14 enrolls *IDEs* with a
//! (possibly remote) relay bridge so the IDE can dial out, register its
//! workspaces, and have the bridge forward phone `call`/`subscribe`
//! frames to it. The two relationships are deliberately the same
//! vocabulary, so there is one security model, not two — see
//! [ADR 0031](../../../specs/decisions/0031-remote-bridge-relay.md).
//!
//! Two pieces, mirroring [`crate::pairing`] so the symmetry is visible
//! in the code:
//!
//! - [`EnrollmentSession`] — the short-lived, single-use enrollment
//!   code an IDE presents during the enroll handshake. Pure in-memory,
//!   unit-testable without a keyring or a network stack. 1:1 mirror of
//!   [`crate::pairing::PairingSession`].
//! - [`IdeStore`] — the long-lived per-IDE tokens minted on a
//!   successful enroll, persisted in the OS keyring
//!   (`service=moon-ide`, `account=companion-ides`), with list +
//!   revoke + token-check. 1:1 mirror of
//!   [`crate::pairing::DeviceStore`], just for IDEs.
//!
//! Enrollment is the **whole** security boundary for the IDE↔bridge
//! relationship, exactly as pairing is for the phone↔bridge one: an
//! enrolled IDE can drive the coder, which runs anything via `bash`,
//! so there is no method-level fence behind it (same threat model as
//! the desktop). The token below is a bearer credential — treat it
//! like the HF / Slack / companion-device tokens already in the keyring.
//!
//! What's intentionally *not* here yet (lands with 14.1): the WSS
//! listener that checks a presented IDE token, and the enrollment
//! payload encoder. This module is the credential core those build on.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long an issued enrollment code stays valid. Same TTL as the
/// pairing code — long enough to share the code and paste it into the
/// IDE's enroll UI, short enough that a leaked code is useless by the
/// time anyone finds it.
pub const ENROLLMENT_CODE_TTL: Duration = Duration::from_secs(120);

/// Keyring coordinates for the IDE registry. One JSON blob holds every
/// enrolled IDE, mirroring how [`crate::pairing::DeviceStore`] stores
/// the phone list as a single keyring entry.
const KEYRING_SERVICE: &str = "moon-ide";
const KEYRING_ACCOUNT: &str = "companion-ides";

/// Unix-epoch milliseconds now. Pulled out so tests can reason about
/// expiry without sleeping. Mirrors `pairing::now_ms`.
fn now_ms() -> u128 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("system clock before 1970")
		.as_millis()
}

/// A short, human-presentable enrollment code plus its expiry. Lives in
/// the bridge process's memory only — never persisted, never logged.
/// 1:1 mirror of [`crate::pairing::PairingSession`].
#[derive(Debug, Clone)]
pub struct EnrollmentSession {
	code: String,
	expires_at_ms: u128,
	consumed: bool,
}

impl EnrollmentSession {
	/// Issue a fresh enrollment code valid for
	/// [`ENROLLMENT_CODE_TTL`]. The code shape matches the pairing
	/// code (`A1B2-C3D4`) so the operator's muscle memory carries
	/// over.
	pub fn issue() -> Self {
		Self::issue_with_ttl(ENROLLMENT_CODE_TTL)
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

	/// The code to show the operator (printed by `moon-bridge enroll-code`
	/// and the IDE's enroll UI passes it back over WSS).
	pub fn code(&self) -> &str {
		&self.code
	}

	/// True once the code has expired against the wall clock.
	pub fn is_expired(&self) -> bool {
		now_ms() >= self.expires_at_ms
	}

	/// Verify a presented code and consume the session on success.
	/// Single-use: a second correct presentation fails with
	/// [`EnrollError::AlreadyUsed`]. An expired session fails with
	/// [`EnrollError::Expired`] regardless of code correctness.
	///
	/// Wired by the WSS listener's `enroll` handler (14.1); `allow`
	/// until then so the credential core lands without a warning.
	#[allow(dead_code)]
	pub fn verify_and_consume(&mut self, presented: &str) -> Result<(), EnrollError> {
		if self.consumed {
			return Err(EnrollError::AlreadyUsed);
		}
		if self.is_expired() {
			return Err(EnrollError::Expired);
		}
		// Case-insensitive compare so a user typing the code in
		// lowercase still enrolls; the canonical form is upper.
		if !presented.trim().eq_ignore_ascii_case(&self.code) {
			return Err(EnrollError::CodeMismatch);
		}
		self.consumed = true;
		Ok(())
	}
}

/// Why an enrollment attempt failed. Surfaced to the IDE so its enroll
/// UI can show the right message ("code expired, ask for a new one" vs
/// "wrong code"). Mirror of [`crate::pairing::PairError`].
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EnrollError {
	#[error("enrollment code has expired")]
	Expired,
	#[error("enrollment code already used")]
	AlreadyUsed,
	#[error("enrollment code did not match")]
	CodeMismatch,
}

/// One enrolled IDE's record. Mirror of
/// [`crate::pairing::PairedDevice`], with one deliberate difference:
/// the `id` is the **IDE's self-assigned `ide_id`** (so reconnections
/// rebind to the same registry entry), not a bridge-minted random — a
/// phone has no stable identity to offer, but an IDE does (it persists
/// across restarts in its own keyring).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrolledIde {
	/// The IDE's self-assigned id (stable across reconnects). The
	/// bridge uses it as the registry key and the phone's switcher
	/// groups workspaces by it.
	pub id: String,
	/// User-facing label the IDE sends at enroll time ("eli-laptop").
	pub label: String,
	/// The bearer token this IDE presents on every connection. Long,
	/// random, never shown again after issue.
	pub token: String,
	/// When the IDE enrolled, Unix-epoch ms.
	pub enrolled_at_ms: u128,
}

impl EnrolledIde {
	/// Mint a new IDE record with a fresh random token. The token is
	/// two concatenated v4 UUIDs (256 bits of entropy), hex, no
	/// dashes — same construction as `PairedDevice::mint`. The `id` is
	/// taken from the IDE (not minted) so reconnects rebind.
	///
	/// Wired by the WSS listener's `enroll` handler (14.1); `allow`
	/// until then so the credential core lands without a warning.
	#[allow(dead_code)]
	pub fn mint(id: impl Into<String>, label: impl Into<String>) -> Self {
		let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
		Self {
			id: id.into(),
			label: label.into(),
			token,
			enrolled_at_ms: now_ms(),
		}
	}
}

/// Keyring-backed registry of enrolled IDEs. The whole list is one JSON
/// blob under a single keyring entry — cheap, and atomic enough for the
/// handful of IDEs a single bridge enrolls. Mirror of
/// [`crate::pairing::DeviceStore`].
pub struct IdeStore {
	entry: keyring::Entry,
}

impl IdeStore {
	/// Open the keyring-backed store. Fails only if the platform
	/// keyring can't be reached at all.
	pub fn open() -> anyhow::Result<Self> {
		let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
		Ok(Self { entry })
	}

	/// Load the current IDE list. A missing entry (nobody's enrolled
	/// yet) is an empty list, not an error.
	pub fn list(&self) -> anyhow::Result<Vec<EnrolledIde>> {
		match self.entry.get_password() {
			Ok(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
			Err(keyring::Error::NoEntry) => Ok(Vec::new()),
			Err(err) => Err(err.into()),
		}
	}

	/// Persist the full IDE list back to the keyring.
	fn save(&self, ides: &[EnrolledIde]) -> anyhow::Result<()> {
		let json = serde_json::to_string(ides)?;
		self.entry.set_password(&json)?;
		Ok(())
	}

	/// Add a freshly-minted IDE and persist. If an IDE with the same
	/// id is already enrolled (re-enroll), its record is replaced —
	/// the operator effectively re-issues a token for that IDE.
	/// Returns the stored record (its token is the caller's to hand
	/// back to the IDE exactly once).
	///
	/// Wired by the WSS listener's `enroll` handler (14.1); `allow`
	/// until then so the credential core lands without a warning.
	#[allow(dead_code)]
	pub fn add(&self, ide: EnrolledIde) -> anyhow::Result<EnrolledIde> {
		let mut ides = self.list()?;
		// Replace-in-place if the id already exists: a re-enroll
		// (operator issued a new code for an IDE whose token was
		// lost) overwrites the old record rather than producing a
		// duplicate id.
		if let Some(slot) = ides.iter_mut().find(|d| d.id == ide.id) {
			*slot = ide.clone();
		} else {
			ides.push(ide.clone());
		}
		self.save(&ides)?;
		Ok(ide)
	}

	/// Revoke an IDE by id. Returns true if an IDE was removed, false
	/// if the id was unknown (already revoked / never existed).
	pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
		let mut ides = self.list()?;
		let before = ides.len();
		ides.retain(|d| d.id != id);
		let removed = ides.len() != before;
		if removed {
			self.save(&ides)?;
		}
		Ok(removed)
	}

	/// Resolve a presented bearer token to the matching IDE, if any.
	/// This is the check the WSS listener (14.1) runs on every IDE
	/// connection. Mirror of `DeviceStore::device_for_token`.
	///
	/// Wired by the listener (14.1); `allow` until then.
	#[allow(dead_code)]
	pub fn ide_for_token(&self, token: &str) -> anyhow::Result<Option<EnrolledIde>> {
		Ok(self.list()?.into_iter().find(|d| d.token == token))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn issued_code_is_grouped_hex() {
		let s = EnrollmentSession::issue();
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
		let mut s = EnrollmentSession::issue();
		let code = s.code().to_string();
		assert_eq!(s.verify_and_consume(&code), Ok(()));
		// Second attempt with the right code is rejected.
		assert_eq!(s.verify_and_consume(&code), Err(EnrollError::AlreadyUsed));
	}

	#[test]
	fn verify_is_case_insensitive_and_trims() {
		let mut s = EnrollmentSession::issue();
		let code = s.code().to_lowercase();
		assert_eq!(s.verify_and_consume(&format!("  {code}  ")), Ok(()));
	}

	#[test]
	fn wrong_code_rejected_without_consuming() {
		let mut s = EnrollmentSession::issue();
		assert_eq!(s.verify_and_consume("0000-0000"), Err(EnrollError::CodeMismatch));
		// A mismatch must not consume the session — the real code
		// still works afterwards.
		let code = s.code().to_string();
		assert_eq!(s.verify_and_consume(&code), Ok(()));
	}

	#[test]
	fn expired_code_rejected() {
		let mut s = EnrollmentSession::issue_with_ttl(Duration::from_millis(0));
		let code = s.code().to_string();
		assert_eq!(s.verify_and_consume(&code), Err(EnrollError::Expired));
	}

	#[test]
	fn minted_ides_have_distinct_tokens_and_stable_ids() {
		// The id is IDE-supplied (stable across reconnects), so two
		// IDEs with different ids get different tokens; two mints
		// with the same id also get distinct tokens (a re-enroll).
		let a = EnrolledIde::mint("eli-laptop", "Eli's laptop");
		let b = EnrolledIde::mint("funk-desktop", "Funk's desktop");
		assert_ne!(a.token, b.token);
		assert_ne!(a.id, b.id);
		assert_eq!(a.token.len(), 64); // two 32-char simple UUIDs
		let a2 = EnrolledIde::mint("eli-laptop", "Eli's laptop");
		assert_eq!(a.id, a2.id); // id stable
		assert_ne!(a.token, a2.token); // token re-rolled
	}
}
