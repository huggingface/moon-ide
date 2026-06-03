//! Tauri commands backing the desktop Companion panel (Phase 13.4b).
//!
//! The mobile-companion bridge runs as a separate process (ADR 0024)
//! and owns the live pairing code + the keyring device store. It
//! publishes both to `companion-status.json` in the bridge dir and
//! watches `companion-revoke.json` for revoke requests. These
//! commands are the IDE's read/write side of that file channel — no
//! direct bridge IPC, no shared keyring writer.

use camino::Utf8PathBuf;
use moon_protocol::MoonError;
use serde::{Deserialize, Serialize};

const STATUS_FILE: &str = "companion-status.json";
const REVOKE_FILE: &str = "companion-revoke.json";

/// Mirror of `moon_bridge::status::CompanionStatus`. Kept as a local
/// copy rather than depending on the bridge binary crate; the bridge
/// owns the canonical shape and both are tiny.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompanionStatus {
	pub running: bool,
	pub pairing_payload: Option<String>,
	pub pairing_url: Option<String>,
	pub pairing_code: Option<String>,
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

#[derive(Debug, Serialize)]
struct RevokeRequest {
	device_id: String,
}

fn bridge_dir() -> Result<Utf8PathBuf, MoonError> {
	let raw = dirs::data_local_dir().ok_or_else(|| MoonError::internal("could not resolve local data dir"))?;
	let utf8 =
		Utf8PathBuf::from_path_buf(raw).map_err(|p| MoonError::internal(format!("non-utf8 data dir: {}", p.display())))?;
	Ok(utf8.join("moon-ide").join("bridge"))
}

/// Read the companion status the bridge published. A missing file
/// means the bridge isn't running — return a default `running:false`
/// status rather than erroring, so the panel renders the "not running"
/// state cleanly.
#[tauri::command]
pub async fn companion_status() -> Result<CompanionStatus, MoonError> {
	let path = bridge_dir()?.join(STATUS_FILE);
	let Ok(bytes) = tokio::fs::read(path.as_std_path()).await else {
		return Ok(CompanionStatus::default());
	};
	match serde_json::from_slice(&bytes) {
		Ok(status) => Ok(status),
		Err(err) => {
			tracing::warn!(error = %err, "companion status parse failed");
			Ok(CompanionStatus::default())
		}
	}
}

/// Drop a revoke request for the bridge to pick up. The bridge polls
/// for this file, revokes the device from the keyring, and refreshes
/// the status file — so the panel should re-fetch `companion_status`
/// shortly after.
#[tauri::command]
pub async fn companion_revoke_device(device_id: String) -> Result<(), MoonError> {
	let path = bridge_dir()?.join(REVOKE_FILE);
	let json = serde_json::to_vec(&RevokeRequest { device_id }).map_err(|e| MoonError::internal(e.to_string()))?;
	tokio::fs::write(path.as_std_path(), json)
		.await
		.map_err(|e| MoonError::internal(format!("could not write revoke request: {e}")))
}
