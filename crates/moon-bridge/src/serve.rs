//! The LAN WSS listener (Phase 13.2).
//!
//! Binds one TLS + WebSocket listener, the single deliberate LAN
//! surface the companion connects to (cross-cutting invariant 3:
//! explicit forwards, never auto-expose). Each accepted connection:
//!
//! 1. TLS handshake (self-signed cert from [`crate::tls`]; the phone
//!    pinned its fingerprint at pair time).
//! 2. WebSocket upgrade.
//! 3. One JSON message per frame. Two message shapes:
//!    - **pair** `{"type":"pair","code","label"}` — verify the code
//!      against the in-memory [`PairingSession`], mint + store a
//!      device, reply with the token. One pairing window per process
//!      run (the `serve` command issues a code at startup).
//!    - **call** `{"type":"call","token","workspace","method","params"}`
//!      — authenticate the token against the [`DeviceStore`], then
//!      relay to the workspace process via [`crate::relay::call`] and
//!      reply with the result.
//!
//! Auth is the whole boundary: a valid device token can call any
//! relayed method, which can drive the coder, which runs anything
//! (see `specs/companion.md`). The token check is the gate; there is
//! no per-method allowlist behind it.

use std::net::SocketAddr;
use std::sync::Arc;

use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;

use crate::pairing::{DeviceStore, PairedDevice, PairingSession};
use crate::tls::TlsIdentity;

/// Inbound message from the phone. Tagged on `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMessage {
	/// Present a pairing code to obtain a device token.
	Pair { code: String, label: String },
	/// Invoke a relayed method, authenticated by a device token.
	Call {
		token: String,
		workspace: String,
		method: String,
		#[serde(default)]
		params: serde_json::Value,
	},
}

/// Outbound reply to the phone.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerMessage {
	/// Pairing succeeded; carries the freshly-minted device token.
	Paired { device_id: String, token: String },
	/// A `call` result (the relayed method's `ok` payload).
	Result { value: serde_json::Value },
	/// Anything went wrong — bad code, bad token, relay failure,
	/// malformed frame. `message` is human-readable.
	Error { message: String },
}

/// Everything a connection handler needs, shared across connections.
struct ServeCtx {
	workspaces_dir: Utf8PathBuf,
	devices: DeviceStore,
	/// The single active pairing session for this `serve` run, behind
	/// a mutex so concurrent connections can't both consume it. `None`
	/// once consumed (single-use) — further `pair` attempts then fail
	/// cleanly until the operator issues a new code (restart `serve`).
	pairing: Mutex<Option<PairingSession>>,
}

/// Run the listener until the process is killed. `addr` is typically
/// `0.0.0.0:53180`. `pairing` is the code issued at startup (shown
/// to the user via the QR); pass `None` to run with pairing closed
/// (only already-paired devices can connect).
pub async fn serve(
	addr: SocketAddr,
	tls: TlsIdentity,
	workspaces_dir: Utf8PathBuf,
	devices: DeviceStore,
	pairing: Option<PairingSession>,
) -> anyhow::Result<()> {
	let acceptor = TlsAcceptor::from(tls.server_config);
	let listener = TcpListener::bind(addr).await?;
	tracing::info!(%addr, "moon-bridge listening");

	let ctx = Arc::new(ServeCtx {
		workspaces_dir,
		devices,
		pairing: Mutex::new(pairing),
	});

	loop {
		let (stream, peer) = match listener.accept().await {
			Ok(pair) => pair,
			Err(err) => {
				tracing::warn!(error = %err, "accept failed");
				continue;
			}
		};
		let acceptor = acceptor.clone();
		let ctx = Arc::clone(&ctx);
		tokio::spawn(async move {
			if let Err(err) = handle_conn(acceptor, stream, peer, ctx).await {
				tracing::debug!(error = %err, %peer, "connection ended");
			}
		});
	}
}

async fn handle_conn(
	acceptor: TlsAcceptor,
	stream: tokio::net::TcpStream,
	peer: SocketAddr,
	ctx: Arc<ServeCtx>,
) -> anyhow::Result<()> {
	let tls_stream = acceptor.accept(stream).await?;
	let mut ws = tokio_tungstenite::accept_async(tls_stream).await?;
	tracing::debug!(%peer, "ws connection established");

	while let Some(frame) = ws.next().await {
		let msg = frame?;
		let text = match msg {
			Message::Text(t) => t.to_string(),
			Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
			Message::Close(_) => break,
			// Ping/pong/frame: tungstenite handles ping/pong itself;
			// ignore anything else.
			_ => continue,
		};

		let reply = handle_message(&ctx, &text).await;
		let json = serde_json::to_string(&reply).unwrap_or_else(|_| r#"{"type":"error","message":"encode failed"}"#.into());
		ws.send(Message::Text(json.into())).await?;
	}
	Ok(())
}

async fn handle_message(ctx: &ServeCtx, text: &str) -> ServerMessage {
	let parsed: ClientMessage = match serde_json::from_str(text) {
		Ok(m) => m,
		Err(err) => {
			return ServerMessage::Error {
				message: format!("malformed message: {err}"),
			}
		}
	};

	match parsed {
		ClientMessage::Pair { code, label } => handle_pair(ctx, &code, &label).await,
		ClientMessage::Call {
			token,
			workspace,
			method,
			params,
		} => handle_call(ctx, &token, &workspace, &method, params).await,
	}
}

async fn handle_pair(ctx: &ServeCtx, code: &str, label: &str) -> ServerMessage {
	let mut guard = ctx.pairing.lock().await;
	let Some(session) = guard.as_mut() else {
		return ServerMessage::Error {
			message: "pairing is closed; ask the desktop to start a new pairing".into(),
		};
	};
	if let Err(err) = session.verify_and_consume(code) {
		return ServerMessage::Error {
			message: err.to_string(),
		};
	}
	// Consumed — drop the session so a replay can't re-pair.
	*guard = None;
	drop(guard);

	let device = PairedDevice::mint(label);
	match ctx.devices.add(device) {
		Ok(stored) => ServerMessage::Paired {
			device_id: stored.id,
			token: stored.token,
		},
		Err(err) => ServerMessage::Error {
			message: format!("could not store device: {err}"),
		},
	}
}

async fn handle_call(
	ctx: &ServeCtx,
	token: &str,
	workspace: &str,
	method: &str,
	params: serde_json::Value,
) -> ServerMessage {
	match ctx.devices.device_for_token(token) {
		Ok(Some(_device)) => {}
		Ok(None) => {
			return ServerMessage::Error {
				message: "unknown device token; pair this device first".into(),
			}
		}
		Err(err) => {
			return ServerMessage::Error {
				message: format!("token check failed: {err}"),
			}
		}
	}

	let socket = crate::discovery::socket_path(&ctx.workspaces_dir, workspace);
	match crate::relay::call(&socket, method, params).await {
		Ok(resp) => {
			if let Some(error) = resp.error {
				ServerMessage::Error { message: error }
			} else {
				ServerMessage::Result {
					value: resp.ok.unwrap_or(serde_json::Value::Null),
				}
			}
		}
		Err(err) => ServerMessage::Error {
			message: err.to_string(),
		},
	}
}
