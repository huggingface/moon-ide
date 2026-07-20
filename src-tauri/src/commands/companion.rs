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
	/// The `wss://…` URL phones connect to. Pairing codes are minted
	/// on demand (`companion_pair_code`), not at bridge startup.
	#[serde(default)]
	pub url: String,
	pub mdns_url: Option<String>,
	pub fingerprint: String,
	pub devices: Vec<DeviceEntry>,
	/// Enrolled IDEs (Phase 14, ADR 0031). Mirror of `devices` for the
	/// IDE↔bridge relationship.
	#[serde(default)]
	pub ides: Vec<IdeEntry>,
	#[serde(default)]
	pub build_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
	pub id: String,
	pub label: String,
	pub paired_at_ms: u128,
}

/// One enrolled IDE (Phase 14). Mirror of `DeviceEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdeEntry {
	pub id: String,
	pub label: String,
	pub enrolled_at_ms: u128,
}

/// Control request wire shape (matches `moon_bridge::status`).
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum ControlRequest {
	Status,
	Revoke {
		device_id: String,
	},
	/// Revoke an enrolled IDE (Phase 14). Mirror of `Revoke`.
	RevokeIde {
		ide_id: String,
	},
	/// Mint a fresh phone-pairing code (Phase 14.5).
	PairCode,
}

/// Control response wire shape.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ControlResponse {
	Status(CompanionStatus),
	// The bridge reports `{ revoked: bool }`; the IDE doesn't act on
	// the flag (it re-fetches status), so the payload is ignored.
	Revoked {},
	/// A freshly-minted phone-pairing payload (Phase 14.5).
	PairCode {
		payload: String,
		url: String,
		code: String,
		fingerprint: String,
	},
	Ok,
	Error {
		message: String,
	},
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

/// Ask the bridge to revoke an enrolled IDE (Phase 14, ADR 0031). Mirror
/// of `companion_revoke_device` for the IDE↔bridge relationship.
#[tauri::command]
pub async fn companion_revoke_ide(ide_id: String) -> Result<(), MoonError> {
	match control_request(&ControlRequest::RevokeIde { ide_id }).await {
		Ok(ControlResponse::Revoked { .. }) | Ok(ControlResponse::Ok) => Ok(()),
		Ok(ControlResponse::Error { message }) => Err(MoonError::internal(message)),
		Ok(_) => Ok(()),
		Err(err) => Err(MoonError::internal(format!("bridge not reachable: {err}"))),
	}
}

/// Mint a fresh phone-pairing code from the local bridge (Phase 14.5).
/// The panel renders the returned payload as a QR. Pairing is
/// on-demand everywhere — there is no startup pairing window.
#[tauri::command]
pub async fn companion_pair_code() -> Result<crate::remote_bridge::PairingQr, MoonError> {
	match control_request(&ControlRequest::PairCode).await {
		Ok(ControlResponse::PairCode {
			payload,
			url,
			code,
			fingerprint,
		}) => Ok(crate::remote_bridge::PairingQr {
			payload,
			url,
			code,
			fingerprint,
		}),
		Ok(ControlResponse::Error { message }) => Err(MoonError::internal(message)),
		Ok(_) => Err(MoonError::internal("unexpected bridge reply")),
		Err(err) => Err(MoonError::internal(format!("bridge not reachable: {err}"))),
	}
}

// --- Remote / relay bridge client commands (Phase 14.3, ADR 0031) ---
// The IDE dials out to a remote bridge. These commands drive the
// outbound WS client in `crate::remote_bridge`.

/// Enroll this IDE with a remote bridge. Connects to `bridge_url`,
/// presents the enrollment `code`, and stores the resulting token in
/// the keyring. The `bridge_rpc` state holds the `BridgeRpcHandler`
/// the forwarded calls dispatch to.
#[tauri::command]
pub async fn companion_enroll(
	bridge_url: String,
	code: String,
	label: String,
	bridge_rpc: tauri::State<'_, std::sync::Arc<dyn crate::focus_socket::BridgeRpcHandler>>,
	state: tauri::State<'_, crate::state::AppState>,
) -> Result<(), MoonError> {
	let ide_id = match crate::remote_bridge::load_credential() {
		Ok(Some(c)) => c.ide_id, // reuse existing id on re-enroll
		_ => crate::remote_bridge::generate_ide_id(),
	};
	// Register under this workspace's real identity (slug + catalog
	// name) so the phone's switcher shows the workspace the user
	// named, not the enroll label.
	let slug = state.workspaces.workspace_id().await;
	let meta = moon_core::app_state::load(&state.config_dir)
		.await
		.ok()
		.and_then(|s| s.workspaces.into_iter().find(|m| m.id == slug));
	let workspace = crate::remote_bridge::RemoteWorkspace {
		id: slug.clone(),
		name: meta.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| slug.clone()),
		last_active_at: meta.map(|m| m.last_active_at),
	};
	let handle = crate::remote_bridge::spawn(bridge_url, code, ide_id, label, workspace, bridge_rpc.inner().clone());
	let mut status_rx = handle.status_receiver();
	// Store the handle so the status/disconnect commands can reach it.
	*state.remote_bridge.lock().await = Some(handle);

	// Wait for the handshake outcome so the UI gets real feedback: a
	// bad code / unreachable bridge is an error here, not a silent
	// `Ok` while the task fails in the background.
	let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
	loop {
		{
			let status = status_rx.borrow();
			if status.connected {
				return Ok(());
			}
			if let Some(err) = &status.error {
				return Err(MoonError::internal(err.clone()));
			}
		}
		match tokio::time::timeout_at(deadline, status_rx.changed()).await {
			Ok(Ok(())) => {}
			// Sender dropped — the connection task ended without
			// reporting; treat as failure.
			Ok(Err(_)) => return Err(MoonError::internal("enrollment task ended unexpectedly")),
			Err(_) => return Err(MoonError::internal("timed out waiting for the bridge; check the URL")),
		}
	}
}

/// Report the current remote-bridge connection status (Phase 14.3).
#[tauri::command]
pub async fn companion_remote_status(
	state: tauri::State<'_, crate::state::AppState>,
) -> Result<crate::remote_bridge::RemoteBridgeStatus, MoonError> {
	let guard = state.remote_bridge.lock().await;
	Ok(match guard.as_ref() {
		Some(handle) => handle.status(),
		None => crate::remote_bridge::RemoteBridgeStatus::default(),
	})
}

/// Ask the remote bridge for a fresh phone-pairing payload (Phase
/// 14.5). Requires a live enrolled connection; the panel renders the
/// returned payload as a QR, exactly like local mode's startup QR.
#[tauri::command]
pub async fn companion_remote_pair_code(
	state: tauri::State<'_, crate::state::AppState>,
) -> Result<crate::remote_bridge::PairingQr, MoonError> {
	let guard = state.remote_bridge.lock().await;
	let handle = guard
		.as_ref()
		.ok_or_else(|| MoonError::internal("not connected to a remote bridge"))?;
	handle
		.request_pair_code()
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// Disconnect from the remote bridge and forget the stored credential
/// (Phase 14.3).
#[tauri::command]
pub async fn companion_remote_disconnect(state: tauri::State<'_, crate::state::AppState>) -> Result<(), MoonError> {
	let mut guard = state.remote_bridge.lock().await;
	if let Some(handle) = guard.take() {
		handle.disconnect();
	}
	let _ = crate::remote_bridge::clear_credential();
	Ok(())
}
