//! Remote / relay bridge client — the IDE's outbound connection to a
//! (possibly remote) `moon-bridge` (Phase 14.3, ADR 0031).
//!
//! In local mode (ADR 0024), the IDE *spawns* the bridge on its own
//! host. In remote / relay mode, the bridge runs elsewhere (a relay
//! box on the VPN), and the IDE **dials out** to it over WSS. This
//! module is that outbound client.
//!
//! The connection lifecycle:
//!
//! 1. **Enroll.** The operator runs `moon-bridge enroll-code` on the
//!    relay box and shares the code + bridge URL with the IDE user.
//!    The user opens "Companion: Connect to remote bridge…" and enters
//!    them. The IDE connects, TOFU-pins the bridge cert (same as a
//!    phone), presents the enrollment code, and receives an IDE token.
//!    The token is stored in the IDE's own keyring
//!    (`service=moon-ide, account=remote-bridge`).
//! 2. **Register.** On every connect (including reconnects after a
//!    dropped VPN), the IDE sends `Register { token, workspaces }`
//!    with its live workspace list so the bridge's remote-carrier
//!    registry stays current.
//! 3. **Serve.** The bridge forwards `call`/`subscribe` frames from
//!    phones (`ForwardCall`/`ForwardSubscribe`). The IDE runs them
//!    against its local `BridgeRpcHandler` (the same one the focus
//!    listener dispatches to) and sends the reply back
//!    (`ForwardResult`/`ForwardEvent`).
//! 4. **Reconnect.** A dropped WS heals with exponential backoff.
//!    The stored token means a reconnect, not a re-enroll — the
//!    enrollment code is single-use and long gone.
//!
//! The coder loop never moves off the IDE host (the load-bearing
//! invariant from ADR 0031). The bridge forwards bytes; the IDE owns
//! the loop, the sessions, and the git layer.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
// The WS framing reuses `BridgeRpcHandler` (dispatch + subscribe),
// which returns `serde_json::Value` directly — no need to touch the
// `R`/`S` Unix-socket framing types here (those are for the
// instance.sock hop, not the WSS hop).
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use crate::focus_socket::BridgeRpcHandler;

/// How often the serve loop pings the bridge. Mirrors the bridge's
/// own `PING_INTERVAL` — either side's pings give the other side's
/// idle detector traffic, and keep the WS alive through proxy idle
/// timeouts (nginx `proxy_read_timeout`, ADR 0035).
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Read-silence deadline before the connection is declared dead and
/// the reconnect loop takes over. Three missed ping exchanges.
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(95);

/// Keyring coordinates for the IDE's stored remote-bridge credential.
/// One entry per enrolled bridge; the IDE stores `{ bridge_url,
/// ide_id, token }` so a reconnect doesn't re-enroll.
const KEYRING_SERVICE: &str = "moon-ide";
const KEYRING_ACCOUNT: &str = "remote-bridge";

/// The stored credential for a remote bridge, persisted in the IDE's
/// own keyring after a successful enroll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBridgeCredential {
	/// The `wss://host:port` URL the IDE connects to.
	pub bridge_url: String,
	/// The IDE's self-assigned stable id (persisted so reconnects
	/// rebind to the same bridge registry entry).
	pub ide_id: String,
	/// The long-lived bearer token the bridge minted at enroll time.
	pub token: String,
}

/// Load the stored remote-bridge credential from the keyring, if any.
/// A missing entry (never enrolled) is `None`, not an error.
pub fn load_credential() -> anyhow::Result<Option<RemoteBridgeCredential>> {
	let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
	match entry.get_password() {
		Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
		Err(keyring::Error::NoEntry) => Ok(None),
		Err(err) => Err(err.into()),
	}
}

/// Store the remote-bridge credential in the keyring after a
/// successful enroll. Overwrites any prior entry (one bridge per IDE
/// in v1; revisit if multi-bridge is requested).
pub fn store_credential(cred: &RemoteBridgeCredential) -> anyhow::Result<()> {
	let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
	entry.set_password(&serde_json::to_string(cred)?)?;
	Ok(())
}

/// Clear the stored credential (disconnect / forget bridge).
pub fn clear_credential() -> anyhow::Result<()> {
	let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;
	// `delete_credential` is the v3 API; `set_password("")` is the
	// portable fallback. Either way, a missing entry is not an error.
	let _ = entry.delete_credential();
	Ok(())
}

/// Inbound WS frame from the bridge (tagged `type`). The IDE is a WS
/// *client* here; these are the messages the bridge sends to it.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[allow(dead_code)]
enum BridgeToIde {
	/// Enrollment succeeded; carries the freshly-minted IDE token.
	Enrolled { ide_id: String, token: String },
	/// Acknowledge a `Register` (the IDE's workspace report).
	Result { value: serde_json::Value },
	/// Error — bad enrollment code, relay failure, etc.
	Error { message: String },
	/// Forward a phone's `call` to this IDE. The IDE runs it against
	/// its local `BridgeRpcHandler` and replies with `ForwardResult`
	/// or `ForwardError` carrying the same `id`. `workspace` is
	/// ignored (the IDE runs against its own workspace), but kept on
	/// the wire for symmetry with the phone's `Call` shape.
	ForwardCall {
		id: u64,
		workspace: String,
		method: String,
		#[serde(default)]
		params: serde_json::Value,
	},
	/// Forward a phone's `subscribe` to this IDE. The IDE pushes
	/// `ForwardEvent` frames until the stream ends, then `ForwardEnd`.
	ForwardSubscribe { id: u64, workspace: String, method: String },
	/// A fresh phone-pairing payload (reply to `PairCode`, Phase
	/// 14.5). The IDE renders `payload` as a QR in its Companion
	/// panel; `url` / `code` / `fingerprint` are the type-in fallback.
	PairPayload {
		payload: String,
		url: String,
		code: String,
		fingerprint: String,
	},
}

/// Outbound WS frame from the IDE to the bridge (tagged `type`).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum IdeToBridge {
	/// Present an enrollment code to obtain an IDE token.
	Enroll {
		code: String,
		label: String,
		ide_id: String,
	},
	/// Report the IDE's live workspaces (sent on connect + on changes).
	Register {
		token: String,
		workspaces: Vec<RemoteWorkspace>,
	},
	/// The result of a forwarded `call`.
	ForwardResult { id: u64, ok: serde_json::Value },
	/// The error from a forwarded `call`.
	ForwardError { id: u64, message: String },
	/// One pushed event from a forwarded `subscribe`.
	ForwardEvent { id: u64, event: serde_json::Value },
	/// The IDE ended a forwarded `subscribe` stream.
	ForwardEnd { id: u64 },
	/// Ask the bridge to mint a fresh phone-pairing code (Phase
	/// 14.5). The bridge trusts enrolled IDEs to open pairing
	/// windows; the reply is `PairPayload`.
	PairCode { token: String },
}

/// A phone-pairing payload the bridge minted on this IDE's request
/// (Phase 14.5). Surfaced to the Companion panel, which renders
/// `payload` as a QR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingQr {
	pub payload: String,
	pub url: String,
	pub code: String,
	pub fingerprint: String,
}

/// Commands the UI can send to the live connection task via the
/// handle. One variant today; the channel exists so future
/// UI-initiated requests (re-register, etc.) have a home.
enum HandleCommand {
	/// Request a fresh phone-pairing payload from the bridge. The
	/// oneshot resolves when the bridge replies (or errors).
	PairCode(tokio::sync::oneshot::Sender<Result<PairingQr, String>>),
}

/// A workspace the IDE reports to the bridge via `Register`. Mirror of
/// `moon_bridge::serve::RemoteWorkspace` — kept local to avoid a dep
/// on the bridge binary crate.
///
/// `id` is the workspace **slug** and `name` the catalog's
/// human-readable label ("Hugging Face"), so the phone's switcher
/// shows the same identity the desktop does — not a hardcoded
/// label (an earlier version registered `"moon-ide"` for every
/// workspace).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteWorkspace {
	pub id: String,
	pub name: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub last_active_at: Option<i64>,
	#[serde(default)]
	pub live: bool,
}

/// The handle the IDE holds for its outbound remote-bridge connection.
/// Dropping it cancels the connection task (the `AbortHandle`).
pub struct RemoteBridgeHandle {
	task: tokio::task::AbortHandle,
	/// A channel the UI can poll for connection status.
	status_rx: tokio::sync::watch::Receiver<RemoteBridgeStatus>,
	/// Commands into the live connection task (pair-code requests).
	cmd_tx: tokio::sync::mpsc::Sender<HandleCommand>,
}

/// Connection status surfaced to the UI.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteBridgeStatus {
	pub connected: bool,
	pub bridge_url: String,
	pub ide_id: String,
	pub error: Option<String>,
}

/// Spawn the outbound remote-bridge client. Connects to `bridge_url`,
/// presents `code` + `ide_id` to enroll, stores the resulting token,
/// then enters the serve loop (Register + answer ForwardCall /
/// ForwardSubscribe). Reconnects with exponential backoff on drop.
///
/// The `rpc` is the same `BridgeRpcHandler` the focus listener
/// dispatches to — reused unchanged so forwarded calls hit the exact
/// same coder + workspace surface a local bridge would.
pub fn spawn(
	bridge_url: String,
	code: String,
	ide_id: String,
	label: String,
	// The full catalog of workspaces on this host, with the
	// currently-open one marked `live: true`. The bridge unions
	// these into the phone's switcher so stopped workspaces show
	// up as launchable too — without this, the phone would only
	// ever see the one workspace the IDE process owns.
	workspaces: Vec<RemoteWorkspace>,
	rpc: Arc<dyn BridgeRpcHandler>,
) -> RemoteBridgeHandle {
	let (status_tx, status_rx) = tokio::sync::watch::channel(RemoteBridgeStatus {
		connected: false,
		bridge_url: bridge_url.clone(),
		ide_id: ide_id.clone(),
		error: None,
	});
	let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<HandleCommand>(8);
	let task = tauri::async_runtime::spawn(async move {
		run_connection_loop(bridge_url, code, ide_id, label, workspaces, rpc, status_tx, cmd_rx).await;
	});
	RemoteBridgeHandle {
		task: task.inner().abort_handle(),
		status_rx,
		cmd_tx,
	}
}

/// The connection loop: connect → enroll (first time) or reconnect
/// (subsequent) → register → serve. Reconnects with backoff on any
/// failure.
#[allow(clippy::too_many_arguments)]
async fn run_connection_loop(
	bridge_url: String,
	code: String,
	ide_id: String,
	label: String,
	workspaces: Vec<RemoteWorkspace>,
	rpc: Arc<dyn BridgeRpcHandler>,
	status_tx: tokio::sync::watch::Sender<RemoteBridgeStatus>,
	mut cmd_rx: tokio::sync::mpsc::Receiver<HandleCommand>,
) {
	// If we have a stored credential, use it (reconnect). If not,
	// this is a fresh enroll — use the code once, then store the
	// resulting token.
	let mut backoff = std::time::Duration::from_secs(1);
	let mut first_attempt = true;
	let _ = code; // code is consumed on the first connect only

	loop {
		let cred = match load_credential() {
			Ok(Some(c)) => Some(c),
			Ok(None) if first_attempt => None, // fresh enroll
			Ok(None) => {
				// No stored credential and we've already tried —
				// the enroll failed. Don't loop forever.
				let _ = status_tx.send(RemoteBridgeStatus {
					connected: false,
					bridge_url: bridge_url.clone(),
					ide_id: ide_id.clone(),
					error: Some("enrollment did not complete".into()),
				});
				return;
			}
			Err(err) => {
				tracing::warn!(error = %err, "could not read stored remote-bridge credential");
				None
			}
		};

		let result = match cred {
			Some(c) => connect_and_serve(&c, &ide_id, &workspaces, &rpc, &status_tx, &mut cmd_rx).await,
			None => {
				// Fresh enroll: connect, present the code, store the
				// token, then fall through to the serve loop.
				match connect_and_enroll(&bridge_url, &code, &ide_id, &label, &status_tx).await {
					Ok(c) => {
						let _ = store_credential(&c);
						connect_and_serve(&c, &ide_id, &workspaces, &rpc, &status_tx, &mut cmd_rx).await
					}
					Err(err) => Err(err),
				}
			}
		};

		first_attempt = false;
		match result {
			Ok(()) => {
				// The bridge closed the WS cleanly — a bridge restart
				// (redeploy, reboot) looks exactly like this. Treat it
				// as a disconnect and reconnect with backoff; the only
				// deliberate stop is `disconnect()` aborting the task.
				tracing::info!("remote-bridge connection closed; reconnecting");
				let _ = status_tx.send(RemoteBridgeStatus {
					connected: false,
					bridge_url: bridge_url.clone(),
					ide_id: ide_id.clone(),
					error: None,
				});
				// A clean close means we did connect — start the
				// backoff ladder over.
				backoff = std::time::Duration::from_secs(1);
				tokio::time::sleep(backoff).await;
			}
			Err(err) => {
				tracing::warn!(error = %err, "remote-bridge connection lost; reconnecting");
				let _ = status_tx.send(RemoteBridgeStatus {
					connected: false,
					bridge_url: bridge_url.clone(),
					ide_id: ide_id.clone(),
					error: Some(err.to_string()),
				});
				tokio::time::sleep(backoff).await;
				backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
			}
		}
	}
}

/// Connect to the bridge, present the enrollment code, and receive
/// the IDE token. Returns the credential to store + use for reconnects.
async fn connect_and_enroll(
	bridge_url: &str,
	code: &str,
	ide_id: &str,
	label: &str,
	status_tx: &tokio::sync::watch::Sender<RemoteBridgeStatus>,
) -> anyhow::Result<RemoteBridgeCredential> {
	let ws = connect_wss(bridge_url).await?;
	let (mut sink, mut source) = ws.split();

	// Send the enroll message.
	let enroll = IdeToBridge::Enroll {
		code: code.to_owned(),
		label: label.to_owned(),
		ide_id: ide_id.to_owned(),
	};
	let json = serde_json::to_string(&enroll)?;
	sink.send(Message::Text(json.into())).await?;

	// Wait for the Enrolled reply.
	while let Some(frame) = source.next().await {
		let msg = frame?;
		let text = match msg {
			Message::Text(t) => t.to_string(),
			Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
			_ => continue,
		};
		let parsed: BridgeToIde = serde_json::from_str(&text)?;
		match parsed {
			BridgeToIde::Enrolled { ide_id: ok_id, token } => {
				let _ = status_tx.send(RemoteBridgeStatus {
					connected: true,
					bridge_url: bridge_url.to_owned(),
					ide_id: ok_id.clone(),
					error: None,
				});
				return Ok(RemoteBridgeCredential {
					bridge_url: bridge_url.to_owned(),
					ide_id: ok_id,
					token,
				});
			}
			BridgeToIde::Error { message } => {
				return Err(anyhow::anyhow!("enrollment failed: {message}"));
			}
			_ => continue,
		}
	}
	Err(anyhow::anyhow!(
		"bridge closed the connection before replying to enroll"
	))
}

/// Connect to the bridge with a stored credential, send `Register`,
/// then enter the serve loop (answer `ForwardCall`/`ForwardSubscribe`).
async fn connect_and_serve(
	cred: &RemoteBridgeCredential,
	ide_id: &str,
	workspaces: &[RemoteWorkspace],
	rpc: &Arc<dyn BridgeRpcHandler>,
	status_tx: &tokio::sync::watch::Sender<RemoteBridgeStatus>,
	cmd_rx: &mut tokio::sync::mpsc::Receiver<HandleCommand>,
) -> anyhow::Result<()> {
	let ws = connect_wss(&cred.bridge_url).await?;
	let (mut sink, mut source) = ws.split();

	// Send Register with this host's full workspace catalog — open
	// and stopped — so the phone's switcher shows launchable
	// workspaces even when no process is listening on them. Each
	// open workspace process on the same host sends its own
	// `Register`; the bridge unions them into the phone's switcher,
	// deduping by `(ide_id, workspace)` keeping the newest. The
	// `live` flag on each entry tells the bridge which ones are
	// currently running.
	let register = IdeToBridge::Register {
		token: cred.token.clone(),
		workspaces: workspaces.to_vec(),
	};
	let json = serde_json::to_string(&register)?;
	sink.send(Message::Text(json.into())).await?;

	let _ = status_tx.send(RemoteBridgeStatus {
		connected: true,
		bridge_url: cred.bridge_url.clone(),
		ide_id: ide_id.to_owned(),
		error: None,
	});

	// Serve loop: read forwarded calls/subscribes from the bridge,
	// dispatch them against the local BridgeRpcHandler, and send the
	// replies back. The sink is wrapped in a mutex so the call-replier
	// and the event-forwarder tasks can both write.
	let sink = Arc::new(Mutex::new(sink));

	// Pair-code requests in flight (FIFO — the bridge answers frames
	// in order on this connection). Resolved by `PairPayload`;
	// drained with an error when the connection drops.
	let mut pending_pair: std::collections::VecDeque<tokio::sync::oneshot::Sender<Result<PairingQr, String>>> =
		std::collections::VecDeque::new();

	// Stop polling the command channel once every sender is gone,
	// or `recv() -> None` would spin the select loop.
	let mut cmd_open = true;

	// Keepalive: ping the bridge every `PING_INTERVAL` (it
	// auto-pongs, and its own pings arrive here as traffic), and
	// treat total read silence past `READ_IDLE_TIMEOUT` as a dead
	// connection — a suspended laptop / dropped NAT entry otherwise
	// leaves this task believing it's connected while the bridge
	// has long stopped hearing from us. Bailing returns into the
	// caller's reconnect-with-backoff loop.
	let mut ping = tokio::time::interval(PING_INTERVAL);
	ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let mut last_traffic = tokio::time::Instant::now();

	loop {
		let text = tokio::select! {
			frame = source.next() => {
				let Some(frame) = frame else { break };
				last_traffic = tokio::time::Instant::now();
				match frame {
					Ok(Message::Text(t)) => t.to_string(),
					Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
					Ok(Message::Close(_)) => break,
					Ok(_) => continue,
					Err(err) => {
						// Fail pending pair requests before surfacing
						// the error so the UI doesn't wait out its
						// timeout.
						for tx in pending_pair.drain(..) {
							let _ = tx.send(Err("bridge connection lost".into()));
						}
						return Err(err.into());
					}
				}
			}
			_ = ping.tick() => {
				if last_traffic.elapsed() > READ_IDLE_TIMEOUT {
					for tx in pending_pair.drain(..) {
						let _ = tx.send(Err("bridge connection lost".into()));
					}
					return Err(anyhow::anyhow!("bridge connection idle past deadline; reconnecting"));
				}
				if sink.lock().await.send(Message::Ping(Vec::new().into())).await.is_err() {
					break;
				}
				continue;
			}
			cmd = cmd_rx.recv(), if cmd_open => {
				match cmd {
					Some(HandleCommand::PairCode(reply_tx)) => {
						let req = IdeToBridge::PairCode { token: cred.token.clone() };
						let json = serde_json::to_string(&req)?;
						if sink.lock().await.send(Message::Text(json.into())).await.is_err() {
							let _ = reply_tx.send(Err("bridge connection lost".into()));
							break;
						}
						pending_pair.push_back(reply_tx);
					}
					// Handle dropped — keep serving; the connection
					// outlives UI interest in commands.
					None => cmd_open = false,
				}
				continue;
			}
		};
		let parsed: BridgeToIde = match serde_json::from_str(&text) {
			Ok(m) => m,
			Err(err) => {
				tracing::warn!(error = %err, "remote-bridge: malformed message");
				continue;
			}
		};
		match parsed {
			BridgeToIde::ForwardCall {
				id,
				workspace: _,
				method,
				params,
			} => {
				let rpc = Arc::clone(rpc);
				let sink = Arc::clone(&sink);
				tauri::async_runtime::spawn(async move {
					let reply = match rpc.dispatch(&method, params).await {
						Ok(value) => IdeToBridge::ForwardResult { id, ok: value },
						Err(message) => IdeToBridge::ForwardError { id, message },
					};
					let json = serde_json::to_string(&reply)
						.unwrap_or_else(|_| r#"{"type":"forwarderror","id":0,"message":"encode failed"}"#.into());
					let _ = sink.lock().await.send(Message::Text(json.into())).await;
				});
			}
			BridgeToIde::ForwardSubscribe {
				id,
				workspace: _,
				method,
			} => {
				let rpc = Arc::clone(rpc);
				let sink = Arc::clone(&sink);
				tauri::async_runtime::spawn(async move {
					let mut rx = match rpc.subscribe(&method, serde_json::Value::Null).await {
						Ok(rx) => rx,
						Err(message) => {
							let reply = IdeToBridge::ForwardError { id, message };
							let json = serde_json::to_string(&reply).unwrap_or_default();
							let _ = sink.lock().await.send(Message::Text(json.into())).await;
							return;
						}
					};
					while let Some(event) = rx.recv().await {
						let frame = IdeToBridge::ForwardEvent { id, event };
						let json = match serde_json::to_string(&frame) {
							Ok(j) => j,
							Err(_) => continue,
						};
						if sink.lock().await.send(Message::Text(json.into())).await.is_err() {
							break; // bridge gone
						}
					}
					// Stream ended — tell the bridge.
					let end = IdeToBridge::ForwardEnd { id };
					let json = serde_json::to_string(&end).unwrap_or_default();
					let _ = sink.lock().await.send(Message::Text(json.into())).await;
				});
			}
			BridgeToIde::Result { .. } => {
				// Register ack — harmless, no action needed.
			}
			BridgeToIde::PairPayload {
				payload,
				url,
				code,
				fingerprint,
			} => match pending_pair.pop_front() {
				Some(tx) => {
					let _ = tx.send(Ok(PairingQr {
						payload,
						url,
						code,
						fingerprint,
					}));
				}
				None => tracing::warn!("remote-bridge: unsolicited pair payload"),
			},
			BridgeToIde::Error { message } => {
				// The bridge's error frames carry no correlation id.
				// If a pair-code request is in flight, the error is
				// its reply (FIFO on this connection); otherwise it's
				// a register/forward complaint — log it.
				match pending_pair.pop_front() {
					Some(tx) => {
						let _ = tx.send(Err(message));
					}
					None => tracing::warn!(%message, "remote-bridge error"),
				}
			}
			BridgeToIde::Enrolled { .. } => {
				// Shouldn't happen in the serve loop (enroll is
				// handled in `connect_and_enroll`), but ignore.
			}
		}
	}
	// Connection ended — fail anything still waiting for a reply.
	for tx in pending_pair.drain(..) {
		let _ = tx.send(Err("bridge connection closed".into()));
	}
	Ok(())
}

/// Connect to a `wss://` URL with a TOFU TLS config (no cert
/// verification — the IDE pins the fingerprint itself, same as a
/// phone). The first connection accepts the cert; subsequent
/// reconnects could verify it, but v1 keeps it simple (the keyring
/// token is the real boundary).
async fn connect_wss(
	url: &str,
) -> anyhow::Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
	let (ws, _response) = tokio_tungstenite::connect_async(url).await?;
	Ok(ws)
}

impl RemoteBridgeHandle {
	/// Get a snapshot of the current connection status.
	pub fn status(&self) -> RemoteBridgeStatus {
		self.status_rx.borrow().clone()
	}

	/// Subscribe to status changes. `companion_enroll` awaits this to
	/// report the enrollment outcome instead of returning before the
	/// handshake even starts.
	pub fn status_receiver(&self) -> tokio::sync::watch::Receiver<RemoteBridgeStatus> {
		self.status_rx.clone()
	}

	/// Disconnect and stop the connection task.
	pub fn disconnect(&self) {
		self.task.abort();
	}

	/// Ask the bridge for a fresh phone-pairing payload (Phase 14.5).
	/// Errors if the connection is down or the bridge doesn't reply
	/// within the timeout.
	pub async fn request_pair_code(&self) -> anyhow::Result<PairingQr> {
		let (tx, rx) = tokio::sync::oneshot::channel();
		self
			.cmd_tx
			.send(HandleCommand::PairCode(tx))
			.await
			.map_err(|_| anyhow::anyhow!("remote-bridge connection task is not running"))?;
		let reply = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
			.await
			.map_err(|_| anyhow::anyhow!("bridge did not reply to the pairing request in time"))?
			.map_err(|_| anyhow::anyhow!("remote-bridge connection dropped mid-request"))?;
		reply.map_err(|message| anyhow::anyhow!(message))
	}
}

/// Generate a stable `ide_id` for this IDE install. It's the hostname
/// (or a fallback random id if the hostname is unavailable), stored
/// in the keyring alongside the token so reconnects rebind to the
/// same registry entry.
pub fn generate_ide_id() -> String {
	hostname::get()
		.ok()
		.and_then(|h| h.into_string().ok())
		.filter(|s| !s.is_empty())
		.unwrap_or_else(|| format!("ide-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]))
}
