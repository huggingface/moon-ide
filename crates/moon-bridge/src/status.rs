//! Local control channel between the desktop IDE and the bridge
//! (Phase 13.4b).
//!
//! The bridge is a separate process that owns state the IDE can't see
//! directly — the live pairing code (in memory) and the keyring
//! device list. Earlier this crossed the process boundary via files
//! (`companion-status.json` / `companion-revoke.json`), but a file is
//! state without a heartbeat: a crashed/killed bridge leaves a stale
//! "running" file behind, so the IDE had to TCP-probe the port to
//! tell if the bridge was really alive.
//!
//! Instead the bridge listens on a Unix socket at
//! `<bridge_dir>/control.sock` and answers `status` / `revoke` /
//! `shutdown` requests. Liveness is intrinsic: if the IDE's
//! `connect()` succeeds, the bridge is alive; if it's refused, it
//! isn't. No file to go stale, same single-instance-detection shape
//! as the per-workspace `instance.sock` (ADR 0014). The bridge stays
//! the sole keyring writer — the IDE only *asks* it to revoke.
//!
//! Wire format: one compact-JSON [`ControlRequest`] line in, one
//! compact-JSON [`ControlResponse`] line out, connection closed.

use serde::{Deserialize, Serialize};

/// Control socket filename under the bridge dir.
pub const CONTROL_SOCK: &str = "control.sock";

/// What the IDE asks the bridge to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ControlRequest {
	/// Report current pairing payload + paired devices.
	Status,
	/// Revoke a paired device by id.
	Revoke { device_id: String },
	/// Ask the bridge to exit (e.g. before a rebuild).
	Shutdown,
}

/// The bridge's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ControlResponse {
	Status(CompanionStatus),
	/// `revoked: true` if a device was removed, false if unknown id.
	Revoked {
		revoked: bool,
	},
	Ok,
	Error {
		message: String,
	},
}

/// The companion state the IDE's panel renders. `running` is implied
/// by a successful connect now, but kept in the struct so the IDE has
/// one shape to hold (set true when the response arrives).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompanionStatus {
	pub running: bool,
	/// Pairing QR payload (compact JSON), or `null` when pairing is
	/// closed (consumed / `--no-pairing`).
	pub pairing_payload: Option<String>,
	pub pairing_url: Option<String>,
	pub pairing_code: Option<String>,
	/// `.local` URL when mDNS is up.
	pub mdns_url: Option<String>,
	pub fingerprint: String,
	pub devices: Vec<DeviceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
	pub id: String,
	pub label: String,
	pub paired_at_ms: u128,
}

/// Path of the control socket under `bridge_dir`.
pub fn control_sock_path(bridge_dir: &camino::Utf8Path) -> std::path::PathBuf {
	bridge_dir.join(CONTROL_SOCK).into_std_path_buf()
}

/// Encode a response as a single framed line for the server side.
pub fn encode_response(resp: &ControlResponse) -> Vec<u8> {
	let mut line = serde_json::to_vec(resp).unwrap_or_else(|_| br#"{"kind":"error","message":"encode failed"}"#.to_vec());
	line.push(b'\n');
	line
}

/// Parse one framed request line on the server side.
pub fn parse_request(buf: &[u8]) -> Option<ControlRequest> {
	let end = buf.iter().position(|&b| b == b'\n')?;
	serde_json::from_slice(&buf[..end]).ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn request_round_trips() {
		for req in [
			ControlRequest::Status,
			ControlRequest::Revoke {
				device_id: "abc".into(),
			},
			ControlRequest::Shutdown,
		] {
			let mut line = serde_json::to_vec(&req).unwrap();
			line.push(b'\n');
			let parsed = parse_request(&line).unwrap();
			// Compare via serialised form (no PartialEq needed).
			assert_eq!(
				serde_json::to_string(&parsed).unwrap(),
				serde_json::to_string(&req).unwrap()
			);
		}
	}

	#[test]
	fn response_round_trips() {
		let resp = ControlResponse::Status(CompanionStatus {
			running: true,
			fingerprint: "aa:bb".into(),
			..Default::default()
		});
		let bytes = encode_response(&resp);
		assert!(bytes.ends_with(b"\n"));
		let end = bytes.iter().position(|&b| b == b'\n').unwrap();
		let back: ControlResponse = serde_json::from_slice(&bytes[..end]).unwrap();
		assert!(matches!(back, ControlResponse::Status(s) if s.running && s.fingerprint == "aa:bb"));
	}
}
