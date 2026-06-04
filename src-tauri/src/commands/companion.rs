//! Tauri commands backing the desktop Companion panel (Phase 13.4b).
//!
//! The mobile-companion bridge runs as a separate process (ADR 0024)
//! and owns the live pairing code + the keyring device store. It
//! serves a local control socket (`<bridge_dir>/control.sock`); these
//! commands are the IDE's client. Liveness is intrinsic — a refused
//! connect means the bridge isn't running, so there's no status file
//! to go stale.

use camino::Utf8PathBuf;
use moon_protocol::MoonError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const CONTROL_SOCK: &str = "control.sock";

/// Mirror of `moon_bridge::status::CompanionStatus`. Local copy
/// rather than a dependency on the bridge binary crate; the bridge
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

/// Control request wire shape (matches `moon_bridge::status`).
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ControlRequest {
	Status,
	Revoke { device_id: String },
}

/// Control response wire shape.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ControlResponse {
	Status(CompanionStatus),
	// The bridge reports `{ revoked: bool }`; the IDE doesn't act on
	// the flag (it re-fetches status), so the payload is ignored.
	Revoked {},
	Ok,
	Error { message: String },
}

fn control_sock_path() -> Result<Utf8PathBuf, MoonError> {
	let raw = dirs::data_local_dir().ok_or_else(|| MoonError::internal("could not resolve local data dir"))?;
	let utf8 =
		Utf8PathBuf::from_path_buf(raw).map_err(|p| MoonError::internal(format!("non-utf8 data dir: {}", p.display())))?;
	Ok(utf8.join("moon-ide").join("bridge").join(CONTROL_SOCK))
}

/// Send one framed-JSON request to the bridge's control socket and
/// read the framed-JSON response. A connect error is the "bridge not
/// running" signal — callers map it to a default status.
async fn control_request(req: &ControlRequest) -> std::io::Result<ControlResponse> {
	let path = control_sock_path().map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
	let mut stream = UnixStream::connect(path.as_std_path()).await?;

	let mut line = serde_json::to_vec(req)?;
	line.push(b'\n');
	stream.write_all(&line).await?;
	stream.flush().await?;

	let mut buf = Vec::with_capacity(1024);
	let mut tmp = [0u8; 4096];
	loop {
		if buf.len() > 64 * 1024 {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				"control response too large",
			));
		}
		let n = stream.read(&mut tmp).await?;
		if n == 0 {
			break;
		}
		buf.extend_from_slice(&tmp[..n]);
		if buf.contains(&b'\n') {
			break;
		}
	}
	let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
	serde_json::from_slice(&buf[..end]).map_err(std::io::Error::from)
}

/// Report the companion bridge's status. A connect failure (bridge
/// not running) yields a default `running: false` status so the panel
/// renders the not-running state cleanly — the connect *is* the
/// liveness check, so nothing can go stale.
#[tauri::command]
pub async fn companion_status() -> Result<CompanionStatus, MoonError> {
	match control_request(&ControlRequest::Status).await {
		Ok(ControlResponse::Status(mut status)) => {
			status.running = true;
			Ok(status)
		}
		Ok(_) => Ok(CompanionStatus::default()),
		Err(_) => Ok(CompanionStatus::default()),
	}
}

/// Ask the bridge to revoke a paired device.
#[tauri::command]
pub async fn companion_revoke_device(device_id: String) -> Result<(), MoonError> {
	match control_request(&ControlRequest::Revoke { device_id }).await {
		Ok(ControlResponse::Revoked { .. }) | Ok(ControlResponse::Ok) => Ok(()),
		Ok(ControlResponse::Error { message }) => Err(MoonError::internal(message)),
		Ok(_) => Ok(()),
		Err(err) => Err(MoonError::internal(format!("bridge not reachable: {err}"))),
	}
}
