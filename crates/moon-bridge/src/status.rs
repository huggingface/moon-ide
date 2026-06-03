//! Cross-process state the desktop IDE reads to drive its Companion
//! panel (Phase 13.4b).
//!
//! The bridge is a separate process from the IDE, and the live
//! pairing code only exists in the running bridge's memory. So the
//! bridge writes a small `companion-status.json` under the bridge dir
//! that the IDE reads for display (pairing payload + paired devices),
//! and watches a `companion-revoke.json` the IDE drops to request a
//! revoke. File-based because it's the lowest-machinery channel
//! between two co-host processes that already share the bridge dir —
//! no second socket, and the bridge stays the single keyring writer
//! (no cross-process keyring races).

use camino::Utf8Path;
use serde::{Deserialize, Serialize};

pub const STATUS_FILE: &str = "companion-status.json";
pub const REVOKE_FILE: &str = "companion-revoke.json";

/// What the IDE's Companion panel renders. Written by the bridge on
/// startup and whenever the device list changes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompanionStatus {
	/// `true` while the bridge is serving (the file exists).
	pub running: bool,
	/// The pairing QR payload as compact JSON, or `null` when pairing
	/// is closed (already consumed / `--no-pairing`).
	pub pairing_payload: Option<String>,
	/// Human type-in fallback: the `wss://…` URL and the code.
	pub pairing_url: Option<String>,
	pub pairing_code: Option<String>,
	/// `.local` URL when mDNS advertising is up (IP URL otherwise).
	pub mdns_url: Option<String>,
	/// Cert fingerprint (colon-hex) for the trust step.
	pub fingerprint: String,
	/// Paired devices (id + label + paired-at), tokens omitted.
	pub devices: Vec<DeviceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
	pub id: String,
	pub label: String,
	pub paired_at_ms: u128,
}

/// A revoke request the IDE drops for the bridge to act on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeRequest {
	pub device_id: String,
}

/// Write the status atomically (temp file + rename) so the IDE never
/// reads a half-written document.
pub fn write_status(bridge_dir: &Utf8Path, status: &CompanionStatus) -> std::io::Result<()> {
	let path = bridge_dir.join(STATUS_FILE);
	let tmp = bridge_dir.join(format!("{STATUS_FILE}.tmp"));
	let json = serde_json::to_vec_pretty(status).unwrap_or_default();
	std::fs::write(&tmp, json)?;
	std::fs::rename(&tmp, &path)
}

/// Remove the status file (called on bridge exit so the IDE sees
/// "not running").
pub fn clear_status(bridge_dir: &Utf8Path) {
	let _ = std::fs::remove_file(bridge_dir.join(STATUS_FILE));
}

/// Take a pending revoke request, if any (removes the file).
pub fn take_revoke_request(bridge_dir: &Utf8Path) -> Option<RevokeRequest> {
	let path = bridge_dir.join(REVOKE_FILE);
	let bytes = std::fs::read(&path).ok()?;
	let _ = std::fs::remove_file(&path);
	serde_json::from_slice(&bytes).ok()
}
