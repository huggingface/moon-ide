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
	/// List the workspaces on this host (the phone's switcher),
	/// authenticated by a device token. Bridge-level, not
	/// workspace-scoped — runs the same `instance.sock` discovery the
	/// `list` subcommand does.
	Workspaces { token: String },
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
	/// The workspace list (reply to `workspaces`).
	Workspaces { workspaces: serde_json::Value },
	/// A `call` result (the relayed method's `ok` payload).
	Result { value: serde_json::Value },
	/// Anything went wrong — bad code, bad token, relay failure,
	/// malformed frame. `message` is human-readable.
	Error { message: String },
}

/// Everything a connection handler needs, shared across connections.
struct ServeCtx {
	workspaces_dir: Utf8PathBuf,
	/// Directory of built PWA assets to serve over HTTP. `None` runs
	/// WS-only (e.g. a dev session pointing the PWA's Vite server at
	/// the bridge for the socket).
	web_root: Option<std::path::PathBuf>,
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
	web_root: Option<std::path::PathBuf>,
	devices: DeviceStore,
	pairing: Option<PairingSession>,
) -> anyhow::Result<()> {
	let acceptor = TlsAcceptor::from(tls.server_config);
	let listener = TcpListener::bind(addr).await?;
	tracing::info!(%addr, "moon-bridge listening");

	let ctx = Arc::new(ServeCtx {
		workspaces_dir,
		web_root,
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
	let mut tls_stream = acceptor.accept(stream).await?;

	// Read the first request and branch: static GET (serve the PWA)
	// or WS upgrade (the companion's data channel).
	match crate::http::read_request(&mut tls_stream).await? {
		Some(crate::http::Incoming::Get { path }) => {
			match &ctx.web_root {
				Some(root) => crate::http::serve_static(&mut tls_stream, root, &path).await?,
				None => {
					// WS-only mode: politely refuse static GETs.
					use tokio::io::AsyncWriteExt;
					let body = b"moon-bridge: no web root configured";
					let resp = format!(
						"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
						body.len()
					);
					tls_stream.write_all(resp.as_bytes()).await?;
					tls_stream.write_all(body).await?;
				}
			}
			return Ok(());
		}
		Some(crate::http::Incoming::WebSocket) => {}
		None => return Ok(()),
	}

	// The 101 handshake response was already written by read_request;
	// wrap the raw stream as a server-role WebSocket (no second
	// handshake).
	let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
		tls_stream,
		tokio_tungstenite::tungstenite::protocol::Role::Server,
		None,
	)
	.await;
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
		ClientMessage::Workspaces { token } => handle_workspaces(ctx, &token).await,
		ClientMessage::Call {
			token,
			workspace,
			method,
			params,
		} => handle_call(ctx, &token, &workspace, &method, params).await,
	}
}

/// Check a presented device token. `Ok(())` if it maps to a paired
/// device; an `Error` [`ServerMessage`] otherwise (unknown token or
/// store failure) so callers can early-return it.
fn check_token(ctx: &ServeCtx, token: &str) -> Result<(), ServerMessage> {
	match ctx.devices.device_for_token(token) {
		Ok(Some(_)) => Ok(()),
		Ok(None) => Err(ServerMessage::Error {
			message: "unknown device token; pair this device first".into(),
		}),
		Err(err) => Err(ServerMessage::Error {
			message: format!("token check failed: {err}"),
		}),
	}
}

async fn handle_workspaces(ctx: &ServeCtx, token: &str) -> ServerMessage {
	if let Err(reply) = check_token(ctx, token) {
		return reply;
	}
	let config_dir = match crate::discovery::resolve_config_dir() {
		Ok(dir) => dir,
		Err(err) => {
			return ServerMessage::Error {
				message: format!("could not resolve config dir: {err}"),
			}
		}
	};
	match crate::discovery::discover(&ctx.workspaces_dir, &config_dir).await {
		Ok(found) => {
			let workspaces = serde_json::json!(found
				.iter()
				.map(|w| serde_json::json!({
					"id": w.id,
					"name": w.name,
					"last_active_at": w.last_active_at,
					"live": w.live,
				}))
				.collect::<Vec<_>>());
			ServerMessage::Workspaces { workspaces }
		}
		Err(err) => ServerMessage::Error {
			message: format!("discovery failed: {err}"),
		},
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
	if let Err(reply) = check_token(ctx, token) {
		return reply;
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
