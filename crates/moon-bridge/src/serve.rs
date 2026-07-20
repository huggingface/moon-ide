//! The LAN WSS listener (Phase 13.2 + Phase 14.1).
//!
//! Binds one TLS + WebSocket listener, the single deliberate LAN
//! surface the companion connects to (cross-cutting invariant 3:
//! explicit forwards, never auto-expose). Each accepted connection:
//!
//! 1. TLS handshake (self-signed cert from [`crate::tls`]; the phone
//!    pinned its fingerprint at pair time).
//! 2. WebSocket upgrade.
//! 3. One JSON message per frame, tagged `type`. Two client types share
//!    the one listener: **phones** (paired) and **IDEs** (enrolled,
//!    Phase 14). Message shapes:
//!    - **pair** `{"type":"pair","code","label"}` — verify the code
//!      against the in-memory [`PairingSession`], mint + store a
//!      device, reply with the token. One pairing window per process
//!      run (the `serve` command issues a code at startup).
//!    - **call** `{"type":"call","token","workspace","method","params"}`
//!      — authenticate the token against the [`DeviceStore`], then
//!      relay to the workspace process via [`crate::relay::call`] and
//!      reply with the result.
//!
//!    - **enroll** — an IDE presents an enrollment code (Phase 14,
//!      ADR 0031); verify against [`EnrollmentSession`], mint + store
//!      an IDE, reply with the token. Symmetric mirror of `pair`.
//!    - **register** — an enrolled IDE reports its live workspaces so
//!      the bridge's remote-carrier registry stays current (discovery
//!      inverts in remote mode: IDEs dial out, the bridge can't
//!      enumerate a remote filesystem).
//!    - **workspaces** / **call** / **subscribe** — phone → bridge,
//!      authenticated by a device token. `call`/`subscribe` route to
//!      whichever carrier owns the target workspace: local-carrier
//!      over the Unix socket ([`crate::relay::call`], unchanged);
//!      remote-carrier over the enrolled IDE's held-open WS (14.2).
//!
//! Auth is the whole boundary: a valid token (device or IDE) can drive
//! the coder, which runs anything (see `specs/companion.md`). The token
//! check is the gate; there is no per-method allowlist behind it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;

use crate::enrollment::{EnrolledIde, EnrollmentSession, IdeStore};
use crate::pairing::{DeviceStore, PairedDevice, PairingSession};
use crate::tls::TlsIdentity;

/// Inbound message from the phone. Tagged on `type`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMessage {
	/// Present a pairing code to obtain a device token (phone).
	Pair { code: String, label: String },
	/// List the workspaces on this host (the phone's switcher),
	/// authenticated by a device token. Bridge-level, not
	/// workspace-scoped — runs the same `instance.sock` discovery the
	/// `list` subcommand does.
	Workspaces { token: String },
	/// Invoke a relayed method, authenticated by a device token.
	/// `ide` selects the carrier: empty = local-carrier (Unix socket,
	/// today's path); present = remote-carrier (forward to that enrolled
	/// IDE's held-open WS, 14.2).
	Call {
		token: String,
		workspace: String,
		method: String,
		#[serde(default)]
		params: serde_json::Value,
		#[serde(default)]
		ide: String,
	},
	/// Subscribe to a workspace's `coder:event` stream. The bridge
	/// pushes `ServerMessage::Event` frames until the connection drops.
	/// `ide` selects the carrier, same as `Call`.
	Subscribe {
		token: String,
		workspace: String,
		#[serde(default)]
		ide: String,
	},
	/// Present an enrollment code to obtain an IDE token (Phase 14,
	/// ADR 0031). Mirror of `Pair` for the IDE↔bridge relationship.
	/// `ide_id` is the IDE's self-assigned stable id (persisted in its
	/// own keyring) so reconnections rebind to the same registry entry.
	Enroll {
		code: String,
		label: String,
		ide_id: String,
	},
	/// An enrolled IDE reports its live workspaces so the bridge's
	/// remote-carrier registry stays current (Phase 14.1). Sent on
	/// connect and whenever the IDE's workspace set changes.
	Register {
		token: String,
		workspaces: Vec<RemoteWorkspace>,
	},
	/// An enrolled IDE asks for a fresh phone-pairing code (Phase
	/// 14.5). An enrolled IDE is already fully trusted (it is the
	/// thing a paired phone would drive), so letting it mint phone
	/// credentials adds no new capability — it just moves the QR to
	/// the IDE's Companion panel instead of the relay's journal,
	/// available on demand rather than only at `serve` startup.
	PairCode { token: String },
	// --- IDE → bridge forwarding replies (Phase 14.2) ---
	// The IDE runs the forwarded call against its local
	// `BridgeRpcHandler` and sends the result back with the matching
	// `id`. The bridge looks up the pending forward, resolves the
	// original phone's reply, and sends it back.
	/// The result of a forwarded `call` (the IDE's `RpcResponse`).
	ForwardResult { id: u64, ok: serde_json::Value },
	/// The result of a forwarded `call` that errored.
	ForwardError { id: u64, message: String },
	/// One pushed event from a forwarded `subscribe` stream.
	ForwardEvent { id: u64, event: serde_json::Value },
	/// The IDE ended a forwarded `subscribe` stream (the coder
	/// stopped emitting / the workspace process exited). The bridge
	/// removes the pending stream so it stops forwarding events.
	ForwardEnd { id: u64 },
}

/// Outbound reply to the phone.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerMessage {
	/// Pairing succeeded; carries the freshly-minted device token.
	Paired { device_id: String, token: String },
	/// Enrollment succeeded; carries the freshly-minted IDE token
	/// (Phase 14, mirror of `Paired`).
	Enrolled { ide_id: String, token: String },
	/// The workspace list (reply to `workspaces`).
	Workspaces { workspaces: serde_json::Value },
	/// A `call` result (the relayed method's `ok` payload).
	Result { value: serde_json::Value },
	/// One pushed event from a `subscribe` stream (a CoderEventEnvelope).
	Event { event: serde_json::Value },
	/// Anything went wrong — bad code, bad token, relay failure,
	/// malformed frame. `message` is human-readable.
	Error { message: String },
	/// A fresh phone-pairing payload (reply to `PairCode`, Phase
	/// 14.5). `payload` is the compact JSON the QR encodes; `url` /
	/// `code` / `fingerprint` are unpacked for type-in fallback
	/// display.
	PairPayload {
		payload: String,
		url: String,
		code: String,
		fingerprint: String,
	},
	// --- bridge → IDE forwarding (Phase 14.2) ---
	// The bridge sends a phone's call/subscribe to the IDE that owns
	// the target workspace. The IDE runs it against its local
	// `BridgeRpcHandler` and replies with `ForwardResult` /
	// `ForwardEvent` carrying the same `id`.
	/// Forward a phone's `call` to an enrolled IDE. The IDE runs the
	/// method and replies with `ForwardResult` or `ForwardError`.
	ForwardCall {
		id: u64,
		workspace: String,
		method: String,
		params: serde_json::Value,
	},
	/// Forward a phone's `subscribe` to an enrolled IDE. The IDE
	/// pushes `ForwardEvent` frames until the stream ends, then
	/// `ForwardEnd`.
	ForwardSubscribe { id: u64, workspace: String, method: String },
}

/// A workspace an enrolled IDE reports via `Register` (Phase 14). The
/// bridge merges these into its `WorkspaceRegistry` as the
/// remote-carrier half; the phone's switcher sees the union tagged with
/// the owning IDE's id. Deliberately a subset of
/// [`crate::discovery::DiscoveredWorkspace`] — remote IDEs report what
/// they have open, not a filesystem probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteWorkspace {
	/// Workspace slug — the `moon-ide --workspace <id>` argument.
	pub id: String,
	/// Human-readable label (falls back to the slug on the phone).
	pub name: String,
	/// Last-active timestamp (Unix epoch seconds), if the IDE knows it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub last_active_at: Option<i64>,
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
	/// Enrolled-IDE registry (Phase 14). Mirror of `devices` for the
	/// IDE↔bridge relationship.
	ides: IdeStore,
	/// The single active enrollment session for this `serve` run,
	/// behind a mutex so concurrent connections can't both consume it.
	/// `None` once consumed (single-use). Mirror of `pairing`.
	enrollment: Mutex<Option<EnrollmentSession>>,
	/// The `wss://…` URL phones should connect to, baked into
	/// IDE-minted pairing payloads (Phase 14.5). Same value the
	/// startup payload used.
	advertise_url: String,
	/// The TLS cert fingerprint for pairing payloads (TOFU pin).
	fingerprint: String,
	/// Live enrolled IDE connections. Keyed by a bridge-issued
	/// per-connection id — **not** by `ide_id`, because moon-ide is
	/// process-per-workspace (ADR 0014): one host runs one workspace
	/// per process, every process connects with the same `ide_id`,
	/// and keying by it made each `Register` clobber the previous
	/// process's entry (the phone only ever saw one workspace per
	/// host). The value carries the connection's `ide_id` and its
	/// last-reported workspace list; `call`/`subscribe` resolve the
	/// carrier by `(ide_id, workspace)` across all connections.
	live_ides: Mutex<HashMap<u64, IdeConnection>>,
	/// Monotonic counter for per-connection ids (keys of `live_ides`).
	conn_counter: std::sync::atomic::AtomicU64,
	/// Pending forwarded calls (Phase 14.2). Maps the bridge-issued
	/// forward `id` to the phone connection awaiting the reply. When
	/// the IDE replies with `ForwardResult`/`ForwardError`, the bridge
	/// looks up the id, sends the result to the phone, and removes
	/// the entry. An entry that never resolves is reaped by the
	/// per-forward timeout.
	pending_forwards: Mutex<HashMap<u64, PendingForward>>,
	/// Pending forwarded subscriptions (Phase 14.2). Maps the
	/// bridge-issued forward `id` to the phone connection receiving
	/// streamed events. The IDE pushes `ForwardEvent`s with the same
	/// id; `ForwardEnd` removes the entry.
	pending_streams: Mutex<HashMap<u64, PendingForward>>,
	/// Monotonic counter for forward ids. One counter for both
	/// `pending_forwards` and `pending_streams` — the id spaces don't
	/// collide because the registries are separate.
	forward_counter: std::sync::atomic::AtomicU64,
}

/// A phone connection awaiting a forwarded reply (Phase 14.2). Held in
/// `pending_forwards` / `pending_streams` keyed by the forward `id`.
struct PendingForward {
	/// Sink to push the reply/event back to the phone that issued the
	/// original `call`/`subscribe`.
	phone_sink: tokio::sync::mpsc::Sender<ServerMessage>,
	/// The IDE connection this forward was sent to (key of
	/// `live_ides`). Used by the disconnect sweep (when an IDE's WS
	/// drops, all its in-flight forwards are reaped and the phones
	/// are errored).
	conn_id: u64,
}

/// A currently-connected enrolled IDE workspace process (Phase 14).
/// Held in `ServeCtx::live_ides` keyed by the per-connection id.
struct IdeConnection {
	/// The enrolled IDE this connection authenticated as. Several
	/// connections share one `ide_id` (one per open workspace
	/// process on that host).
	ide_id: String,
	/// Sink to push messages down this IDE's WS (a forwarded `call`
	/// or `subscribe` from a phone). Cloned from the per-connection
	/// mpsc sender so multiple phones can talk to one IDE.
	sink: tokio::sync::mpsc::Sender<ServerMessage>,
	/// The workspaces this connection last reported via `Register`.
	/// The bridge surfaces these in the phone's `workspaces` reply
	/// (union with local-carrier discovery).
	workspaces: Vec<RemoteWorkspace>,
}

/// Inputs for [`serve`]. Bundled into a struct because the listener
/// needs a fair few co-dependent pieces (TLS identity, the dirs it
/// reads/writes, the startup pairing session + the payload it
/// publishes for the IDE's Companion panel).
pub struct ServeConfig {
	pub addr: SocketAddr,
	pub tls: TlsIdentity,
	pub workspaces_dir: Utf8PathBuf,
	pub bridge_dir: Utf8PathBuf,
	pub web_root: Option<std::path::PathBuf>,
	pub devices: DeviceStore,
	/// The `wss://moon-bridge.local:<port>` URL when mDNS is up.
	pub mdns_url: Option<String>,
	/// LAN IP for mDNS advertising (the host's own address).
	pub advertise_ip: Option<std::net::Ipv4Addr>,
	/// Enrolled-IDE registry (Phase 14). Mirror of `devices`.
	pub ides: IdeStore,
	/// The enrollment session for this `serve` run (Phase 14). `None`
	/// when enrollment is closed (`--no-enrollment`). Mirror of
	/// `pairing`.
	pub enrollment: Option<EnrollmentSession>,
	/// Exit the process when no local workspace is live (ADR 0024,
	/// the auto-spawned local bridge). `false` for a standing relay
	/// deployment (ADR 0035) that serves remote-carrier IDEs and has
	/// no local `instance.sock`s at all.
	pub idle_exit: bool,
	/// The `wss://…` URL phones connect to (the startup payload's
	/// `url`), reused for IDE-minted pairing payloads (Phase 14.5).
	pub advertise_url: String,
}

/// Run the listener until the process is killed.
pub async fn serve(cfg: ServeConfig) -> anyhow::Result<()> {
	let ServeConfig {
		addr,
		tls,
		workspaces_dir,
		bridge_dir,
		web_root,
		devices,
		mdns_url,
		advertise_ip,
		ides,
		enrollment,
		idle_exit,
		advertise_url,
	} = cfg;
	let fingerprint = tls.fingerprint.clone();
	let acceptor = TlsAcceptor::from(tls.server_config);

	// Binding the LAN port *is* the machine-wide owner election
	// (ADR 0024): the first bridge to bind wins; a later one hits
	// `AddrInUse` and exits cleanly, since a live owner already serves
	// the whole machine. This is why every IDE can fire-and-forget a
	// `serve` child on startup without coordinating.
	let listener = match TcpListener::bind(addr).await {
		Ok(l) => l,
		Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
			tracing::info!(%addr, "another bridge already owns this port; exiting");
			return Ok(());
		}
		Err(err) => return Err(err.into()),
	};
	tracing::info!(%addr, "moon-bridge listening");

	// Start mDNS so the phone can use `moon-bridge.local` (best-effort;
	// the IP URL in the payload is the fallback when multicast is
	// blocked). Held for the process lifetime.
	let _mdns = match advertise_ip {
		Some(ip) => crate::mdns::advertise(ip, addr.port())
			.map_err(|err| tracing::warn!(error = %err, "mDNS advertise failed; .local name won't resolve"))
			.ok(),
		None => None,
	};

	let ctx = Arc::new(ServeCtx {
		workspaces_dir,
		web_root,
		devices,
		// Pairing is on-demand (Phase 14.5): no session until the
		// local panel or an enrolled IDE mints one.
		pairing: Mutex::new(None),
		ides,
		enrollment: Mutex::new(enrollment),
		advertise_url,
		fingerprint: fingerprint.clone(),
		live_ides: Mutex::new(HashMap::new()),
		conn_counter: std::sync::atomic::AtomicU64::new(1),
		pending_forwards: Mutex::new(HashMap::new()),
		pending_streams: Mutex::new(HashMap::new()),
		forward_counter: std::sync::atomic::AtomicU64::new(1),
	});

	// Serve the local control socket the IDE's Companion panel uses
	// for status / revoke / shutdown. Liveness is the connect itself,
	// so there's no status file to go stale (replaces the old
	// companion-status.json / companion-revoke.json files).
	spawn_control_listener(bridge_dir.clone(), Arc::clone(&ctx), mdns_url);

	// Idle watcher: when no workspace is live, the bridge has nothing
	// to serve, so it exits (ADR 0024). Discovery is the same signal
	// used for the switcher, so "the last IDE closed" needs no extra
	// IPC. A grace period before the first check avoids a race where
	// the bridge starts microseconds before the IDE that spawned it
	// has bound its own `instance.sock`. A standing relay
	// (`--no-idle-exit`, ADR 0035) skips this: it serves
	// remote-carrier IDEs and never has local workspaces.
	if idle_exit {
		spawn_idle_watcher(ctx.workspaces_dir.clone());
	}

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

/// Snapshot the current companion state from live device state.
fn current_status(ctx: &ServeCtx, mdns_url: Option<&str>) -> crate::status::CompanionStatus {
	let devices = ctx
		.devices
		.list()
		.unwrap_or_default()
		.into_iter()
		.map(|d| crate::status::DeviceEntry {
			id: d.id,
			label: d.label,
			paired_at_ms: d.paired_at_ms,
		})
		.collect();
	let ides = ctx
		.ides
		.list()
		.unwrap_or_default()
		.into_iter()
		.map(|i| crate::status::IdeEntry {
			id: i.id,
			label: i.label,
			enrolled_at_ms: i.enrolled_at_ms,
		})
		.collect();
	crate::status::CompanionStatus {
		running: true,
		url: ctx.advertise_url.clone(),
		mdns_url: mdns_url.map(str::to_owned),
		fingerprint: ctx.fingerprint.clone(),
		devices,
		ides,
		build_id: crate::status::self_build_id(),
	}
}

/// Bind the local control socket and serve `status` / `revoke` /
/// `shutdown` from the IDE's Companion panel. Replaces the old
/// status-file + revoke-file channel: a successful connect *is* the
/// liveness signal, so nothing goes stale.
fn spawn_control_listener(bridge_dir: Utf8PathBuf, ctx: Arc<ServeCtx>, mdns_url: Option<String>) {
	tokio::spawn(async move {
		let path = crate::status::control_sock_path(&bridge_dir);
		// A stale socket file (previous crash) blocks bind; unlink and
		// rebind. Safe because we already won the port election, so
		// we're the one true bridge.
		let _ = std::fs::remove_file(&path);
		let listener = match tokio::net::UnixListener::bind(&path) {
			Ok(l) => l,
			Err(err) => {
				tracing::warn!(error = %err, "could not bind companion control socket; panel will show not-running");
				return;
			}
		};

		loop {
			let Ok((mut stream, _)) = listener.accept().await else {
				continue;
			};
			let ctx = Arc::clone(&ctx);
			let mdns_url = mdns_url.clone();
			tokio::spawn(async move {
				use tokio::io::{AsyncReadExt, AsyncWriteExt};
				let mut buf = Vec::with_capacity(256);
				let mut tmp = [0u8; 1024];
				// Read one framed request line.
				let req = loop {
					let Ok(n) = stream.read(&mut tmp).await else {
						return;
					};
					if n == 0 {
						return;
					}
					buf.extend_from_slice(&tmp[..n]);
					if let Some(req) = crate::status::parse_request(&buf) {
						break req;
					}
					if buf.len() > 64 * 1024 {
						return;
					}
				};

				let resp = match req {
					crate::status::ControlRequest::Status => {
						crate::status::ControlResponse::Status(current_status(&ctx, mdns_url.as_deref()))
					}
					crate::status::ControlRequest::PairCode => {
						// Local-panel mint (Phase 14.5): same semantics
						// as the enrolled-IDE path — one live session,
						// fresh code replaces the old.
						let session = PairingSession::issue();
						let payload = crate::pairing::PairingPayload::new(&ctx.advertise_url, &ctx.fingerprint, session.code());
						*ctx.pairing.lock().await = Some(session);
						tracing::info!("pairing window opened from the local panel");
						crate::status::ControlResponse::PairCode {
							payload: payload.to_link(),
							url: payload.url,
							code: payload.code,
							fingerprint: payload.fingerprint,
						}
					}
					crate::status::ControlRequest::Revoke { device_id } => match ctx.devices.revoke(&device_id) {
						Ok(revoked) => {
							if revoked {
								tracing::info!(id = %device_id, "revoked companion device");
							}
							crate::status::ControlResponse::Revoked { revoked }
						}
						Err(err) => crate::status::ControlResponse::Error {
							message: format!("revoke failed: {err}"),
						},
					},
					crate::status::ControlRequest::RevokeIde { ide_id } => match ctx.ides.revoke(&ide_id) {
						Ok(revoked) => {
							if revoked {
								tracing::info!(id = %ide_id, "revoked enrolled IDE");
								// Also drop any live connections (one per
								// workspace process), so a revoked IDE
								// can't keep forwarding until it
								// reconnects.
								ctx.live_ides.lock().await.retain(|_, c| c.ide_id != ide_id);
							}
							crate::status::ControlResponse::Revoked { revoked }
						}
						Err(err) => crate::status::ControlResponse::Error {
							message: format!("revoke-ide failed: {err}"),
						},
					},
					crate::status::ControlRequest::Shutdown => {
						let _ = stream
							.write_all(&crate::status::encode_response(&crate::status::ControlResponse::Ok))
							.await;
						let _ = stream.flush().await;
						tracing::info!("companion bridge shutting down on control request");
						std::process::exit(0);
					}
				};
				let _ = stream.write_all(&crate::status::encode_response(&resp)).await;
				let _ = stream.flush().await;
			});
		}
	});
}

/// Grace period before the first idle check, so the bridge doesn't
/// exit in the gap between starting and the IDE that spawned it
/// binding its own `instance.sock`. The IDE binds that socket very
/// early (pre-Tauri, ADR 0014) and only spawns the bridge after
/// setup, so a few seconds is ample — kept short so the bridge stops
/// promptly after the last IDE closes (it would otherwise linger and
/// hold its binary against the next in-IDE rebuild, ADR 0005).
const IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the idle watcher re-checks for live workspaces. Short so
/// "closed the IDE → bridge gone" feels immediate; the cost is a
/// couple of cheap socket probes per tick.
const IDLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Exit the process once no workspace is live (ADR 0024). Discovery
/// is the same signal the switcher uses, so "the last IDE closed"
/// needs no extra IPC. Runs forever; the only way out is
/// `std::process::exit` when the live count hits zero.
fn spawn_idle_watcher(workspaces_dir: Utf8PathBuf) {
	tokio::spawn(async move {
		tokio::time::sleep(IDLE_GRACE).await;
		// `config_dir` only decorates names; discovery's liveness
		// doesn't depend on it, so a config-dir hiccup falls back to
		// the workspaces dir as its own config root (harmless).
		let config_dir = crate::discovery::resolve_config_dir().unwrap_or_else(|_| workspaces_dir.clone());
		loop {
			tokio::time::sleep(IDLE_INTERVAL).await;
			// Only exit on a *successful* discovery reporting zero live
			// workspaces. A transient discovery error keeps the bridge
			// up rather than tearing it down on a filesystem blip.
			match crate::discovery::discover(&workspaces_dir, &config_dir).await {
				Ok(ws) if ws.iter().all(|w| !w.live) => {
					tracing::info!("no live workspaces; bridge exiting");
					std::process::exit(0);
				}
				Ok(_) => {}
				Err(err) => tracing::debug!(error = %err, "idle check discovery failed; staying up"),
			}
		}
	});
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
	let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
		tls_stream,
		tokio_tungstenite::tungstenite::protocol::Role::Server,
		None,
	)
	.await;
	tracing::debug!(%peer, "ws connection established");

	// Split the socket so a subscription's pushed events and the
	// request/reply path can both write. A single writer task owns the
	// sink; everything else sends `ServerMessage`s down an mpsc.
	let (mut sink, mut source) = ws.split();
	let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<ServerMessage>(256);

	let writer = tokio::spawn(async move {
		while let Some(msg) = out_rx.recv().await {
			let json = serde_json::to_string(&msg).unwrap_or_else(|_| r#"{"type":"error","message":"encode failed"}"#.into());
			if sink.send(Message::Text(json.into())).await.is_err() {
				break;
			}
		}
	});

	// Per-connection id — the `live_ides` key if this connection
	// `Register`s as an IDE workspace process. A phone never
	// registers, so its cleanup below is a no-op.
	let conn_id = ctx.conn_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

	while let Some(frame) = source.next().await {
		let msg = frame?;
		let text = match msg {
			Message::Text(t) => t.to_string(),
			Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
			Message::Close(_) => break,
			_ => continue,
		};
		handle_message(&ctx, &text, &out_tx, conn_id).await;
	}

	drop(out_tx);
	let _ = writer.await;

	// Connection cleanup: if this connection had registered as an IDE
	// workspace process, remove it from the live table so the phone's
	// switcher no longer lists its workspaces, and sweep all
	// in-flight forwards for it so phones awaiting replies get an
	// error instead of hanging. The IDE reconnects on restart (its
	// stored token means a reconnect, not a re-enroll).
	if let Some(conn) = ctx.live_ides.lock().await.remove(&conn_id) {
		let ide_id = conn.ide_id;
		// Sweep pending forwards: error every phone whose forwarded
		// call was in-flight to this connection.
		let mut errored = 0;
		{
			let mut pending = ctx.pending_forwards.lock().await;
			let stale: Vec<u64> = pending
				.iter()
				.filter(|(_, pf)| pf.conn_id == conn_id)
				.map(|(id, _)| *id)
				.collect();
			for id in stale {
				if let Some(entry) = pending.remove(&id) {
					let _ = entry
						.phone_sink
						.send(ServerMessage::Error {
							message: format!("IDE `{ide_id}` disconnected mid-call"),
						})
						.await;
					errored += 1;
				}
			}
		}
		// Sweep pending streams: same, for forwarded subscriptions.
		{
			let mut pending = ctx.pending_streams.lock().await;
			let stale: Vec<u64> = pending
				.iter()
				.filter(|(_, pf)| pf.conn_id == conn_id)
				.map(|(id, _)| *id)
				.collect();
			for id in stale {
				if let Some(entry) = pending.remove(&id) {
					let _ = entry
						.phone_sink
						.send(ServerMessage::Error {
							message: format!("IDE `{ide_id}` disconnected mid-stream"),
						})
						.await;
					errored += 1;
				}
			}
		}
		tracing::info!(%ide_id, errored, "IDE workspace disconnected; removed from live table, errored pending forwards");
	}
	Ok(())
}

async fn handle_message(ctx: &Arc<ServeCtx>, text: &str, out: &tokio::sync::mpsc::Sender<ServerMessage>, conn_id: u64) {
	let parsed: ClientMessage = match serde_json::from_str(text) {
		Ok(m) => m,
		Err(err) => {
			let _ = out
				.send(ServerMessage::Error {
					message: format!("malformed message: {err}"),
				})
				.await;
			return;
		}
	};

	match parsed {
		ClientMessage::Pair { code, label } => {
			let _ = out.send(handle_pair(ctx, &code, &label).await).await;
		}
		ClientMessage::Workspaces { token } => {
			let _ = out.send(handle_workspaces(ctx, &token).await).await;
		}
		ClientMessage::Call {
			token,
			workspace,
			method,
			params,
			ide,
		} => {
			handle_call(Arc::clone(ctx), &token, &workspace, &method, params, &ide, out).await;
		}
		ClientMessage::Subscribe { token, workspace, ide } => {
			handle_subscribe(ctx, &token, &workspace, &ide, out).await;
		}
		ClientMessage::Enroll { code, label, ide_id } => {
			// Enrollment only mints the token; the connection joins
			// the live table when it `Register`s its workspaces.
			let _ = out.send(handle_enroll(ctx, &code, &label, &ide_id).await).await;
		}
		ClientMessage::Register { token, workspaces } => {
			let _ = out
				.send(handle_register(ctx, &token, workspaces, out.clone(), conn_id).await)
				.await;
		}
		ClientMessage::PairCode { token } => {
			let _ = out.send(handle_pair_code(ctx, &token).await).await;
		}
		// IDE → bridge forwarding replies (14.2). These come from an
		// enrolled IDE responding to a `ForwardCall`/`ForwardSubscribe`
		// the bridge sent it. The bridge looks up the pending forward
		// by `id`, forwards the result/event to the phone, and removes
		// the entry (for calls) or keeps it (for streams, until
		// `ForwardEnd`).
		ClientMessage::ForwardResult { id, ok } => {
			let mut pending = ctx.pending_forwards.lock().await;
			if let Some(entry) = pending.remove(&id) {
				let _ = entry.phone_sink.send(ServerMessage::Result { value: ok }).await;
			}
		}
		ClientMessage::ForwardError { id, message } => {
			let mut pending = ctx.pending_forwards.lock().await;
			if let Some(entry) = pending.remove(&id) {
				let _ = entry.phone_sink.send(ServerMessage::Error { message }).await;
			}
		}
		ClientMessage::ForwardEvent { id, event } => {
			let pending = ctx.pending_streams.lock().await;
			if let Some(entry) = pending.get(&id) {
				let _ = entry.phone_sink.try_send(ServerMessage::Event { event });
			}
		}
		ClientMessage::ForwardEnd { id } => {
			let mut pending = ctx.pending_streams.lock().await;
			pending.remove(&id);
		}
	}
}

/// Start streaming `coder_events` from `workspace` to the phone.
/// Spawns a task that relays each event as a `ServerMessage::Event`
/// down the writer channel until the workspace ends the stream or the
/// channel closes (phone disconnected).
async fn handle_subscribe(
	ctx: &Arc<ServeCtx>,
	token: &str,
	workspace: &str,
	ide: &str,
	out: &tokio::sync::mpsc::Sender<ServerMessage>,
) {
	if let Err(reply) = check_token(ctx, token) {
		let _ = out.send(reply).await;
		return;
	}

	// Carrier selection (ADR 0031), same as `handle_call`.
	if ide.is_empty() {
		// Local-carrier: subscribe over the Unix socket (unchanged).
		let socket = crate::discovery::socket_path(&ctx.workspaces_dir, workspace);
		let out = out.clone();
		tokio::spawn(async move {
			let forward = |event: serde_json::Value| -> bool { out.try_send(ServerMessage::Event { event }).is_ok() };
			if let Err(err) = crate::relay::subscribe(&socket, "coder_events", forward).await {
				let _ = out
					.send(ServerMessage::Error {
						message: format!("event stream ended: {err}"),
					})
					.await;
			}
		});
		return;
	}

	// Remote-carrier: forward the subscribe to the IDE connection
	// that owns `(ide, workspace)`.
	let id = ctx.forward_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let live = ctx.live_ides.lock().await;
	let Some((conn_id, conn)) = find_ide_conn(&live, ide, workspace) else {
		let _ = out
			.send(ServerMessage::Error {
				message: format!("IDE `{ide}` has no connected workspace `{workspace}`"),
			})
			.await;
		return;
	};
	// Register the pending stream *before* sending, so the first
	// `ForwardEvent` can't arrive before the entry exists.
	{
		let mut pending = ctx.pending_streams.lock().await;
		pending.insert(
			id,
			PendingForward {
				phone_sink: out.clone(),
				conn_id,
			},
		);
	}
	if conn
		.sink
		.send(ServerMessage::ForwardSubscribe {
			id,
			workspace: workspace.to_owned(),
			method: "coder_events".to_owned(),
		})
		.await
		.is_err()
	{
		ctx.pending_streams.lock().await.remove(&id);
		let _ = out
			.send(ServerMessage::Error {
				message: format!("IDE `{ide}` connection lost mid-forward"),
			})
			.await;
		return;
	}
	// Spawn a per-stream timeout task: if the IDE never sends even one
	// event (and no `ForwardEnd`), reap the entry after `FORWARD_TIMEOUT`
	// so the phone doesn't hold a dead subscription indefinitely. Note
	// this only reaps *stale* streams — an active stream that's just
	// quiet (the agent is idle) will have already received at least one
	// event and the entry will have been touched. A quiet-but-alive
	// stream is the normal case; the timeout only catches the
	// "IDE accepted the subscribe but never responded at all" case.
	let ctx_for_timeout = Arc::clone(ctx);
	tokio::spawn(async move {
		tokio::time::sleep(FORWARD_TIMEOUT).await;
		let mut pending = ctx_for_timeout.pending_streams.lock().await;
		if let Some(entry) = pending.remove(&id) {
			let _ = entry
				.phone_sink
				.send(ServerMessage::Error {
					message: "forwarded stream timed out".into(),
				})
				.await;
		}
	});
	// Events arrive asynchronously via `ForwardEvent` — see the dispatch
	// in `handle_message`. `ForwardEnd` removes the entry.
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
	// Local-carrier workspaces (instance.sock discovery, unchanged).
	let mut entries: Vec<serde_json::Value> = match crate::discovery::discover(&ctx.workspaces_dir, &config_dir).await {
		Ok(found) => found
			.iter()
			.map(|w| {
				serde_json::json!({
					"id": w.id,
					"name": w.name,
					"last_active_at": w.last_active_at,
					"live": w.live,
					// Empty `ide` = local-carrier (this machine). The
					// phone's switcher groups by it (14.4).
					"ide": "",
				})
			})
			.collect(),
		Err(err) => {
			return ServerMessage::Error {
				message: format!("discovery failed: {err}"),
			}
		}
	};
	// Remote-carrier workspaces (enrolled IDE connections in the live
	// table, 14.1). Each is tagged with its owning IDE's id so the
	// phone can group by host.
	let live = ctx.live_ides.lock().await;
	for conn in live.values() {
		for w in &conn.workspaces {
			entries.push(serde_json::json!({
				"id": w.id,
				"name": w.name,
				"last_active_at": w.last_active_at,
				"live": true,
				"ide": conn.ide_id,
			}));
		}
	}
	ServerMessage::Workspaces {
		workspaces: serde_json::Value::Array(entries),
	}
}

async fn handle_pair(ctx: &ServeCtx, code: &str, label: &str) -> ServerMessage {
	let mut guard = ctx.pairing.lock().await;
	let Some(session) = guard.as_mut() else {
		tracing::warn!("pair attempt while pairing is closed");
		return ServerMessage::Error {
			message: "pairing is closed; ask the desktop to start a new pairing".into(),
		};
	};
	if let Err(err) = session.verify_and_consume(code) {
		tracing::warn!(error = %err, "pair attempt rejected");
		return ServerMessage::Error {
			message: err.to_string(),
		};
	}
	// Consumed — drop the session so a replay can't re-pair.
	*guard = None;
	drop(guard);

	let device = PairedDevice::mint(label);
	match ctx.devices.add(device) {
		Ok(stored) => {
			tracing::info!(device_id = %stored.id, label = %stored.label, "device paired");
			ServerMessage::Paired {
				device_id: stored.id,
				token: stored.token,
			}
		}
		Err(err) => ServerMessage::Error {
			message: format!("could not store device: {err}"),
		},
	}
}

/// Verify an enrollment code and mint an IDE token (Phase 14.1). Mirror
/// of [`handle_pair`] for the IDE↔bridge relationship. The `ide_id` is
/// the IDE's self-assigned stable id (persisted in its own keyring) so
/// reconnections rebind to the same registry entry — a phone has no
/// stable identity to offer, but an IDE does.
async fn handle_enroll(ctx: &ServeCtx, code: &str, label: &str, ide_id: &str) -> ServerMessage {
	let mut guard = ctx.enrollment.lock().await;
	let Some(session) = guard.as_mut() else {
		tracing::warn!(%ide_id, "enroll attempt while enrollment is closed");
		return ServerMessage::Error {
			message: "enrollment is closed; ask the operator to issue a new enrollment code".into(),
		};
	};
	if let Err(err) = session.verify_and_consume(code) {
		tracing::warn!(%ide_id, error = %err, "enroll attempt rejected");
		return ServerMessage::Error {
			message: err.to_string(),
		};
	}
	// Consumed — drop the session so a replay can't re-enroll.
	*guard = None;
	drop(guard);

	let ide = EnrolledIde::mint(ide_id, label);
	match ctx.ides.add(ide) {
		Ok(stored) => {
			tracing::info!(ide_id = %stored.id, label = %stored.label, "IDE enrolled");
			ServerMessage::Enrolled {
				ide_id: stored.id,
				token: stored.token,
			}
		}
		Err(err) => ServerMessage::Error {
			message: format!("could not store IDE: {err}"),
		},
	}
}

/// An enrolled IDE workspace process reports its live workspaces
/// (Phase 14.1). Verifies the IDE token, then upserts this
/// connection's entry in the live table with the reported workspace
/// list + sink (so 14.2's forwarding can reach it). Keyed by the
/// per-connection id — several workspace processes on one host share
/// an `ide_id` and each holds its own connection (ADR 0014).
async fn handle_register(
	ctx: &ServeCtx,
	token: &str,
	workspaces: Vec<RemoteWorkspace>,
	sink: tokio::sync::mpsc::Sender<ServerMessage>,
	conn_id: u64,
) -> ServerMessage {
	let ide = match check_ide_token(ctx, token) {
		Ok(ide) => ide,
		Err(reply) => return reply,
	};
	let mut live = ctx.live_ides.lock().await;
	live.insert(
		conn_id,
		IdeConnection {
			ide_id: ide.id.clone(),
			sink,
			workspaces,
		},
	);
	tracing::info!(ide_id = %ide.id, conn_id, "IDE registered workspaces");
	ServerMessage::Result {
		value: serde_json::json!({ "registered": true }),
	}
}

/// Mint a fresh phone-pairing code on request from an enrolled IDE
/// (Phase 14.5). Replaces the active pairing session (there is one
/// live code at a time, same as the startup window), and returns the
/// payload for the IDE's Companion panel to render as a QR. The code
/// keeps the usual TTL + single-use semantics; only *when* a window
/// opens changes.
async fn handle_pair_code(ctx: &ServeCtx, token: &str) -> ServerMessage {
	if let Err(reply) = check_ide_token(ctx, token) {
		return reply;
	}
	let session = PairingSession::issue();
	let payload = crate::pairing::PairingPayload::new(&ctx.advertise_url, &ctx.fingerprint, session.code());
	*ctx.pairing.lock().await = Some(session);
	tracing::info!("pairing window opened by enrolled IDE");
	ServerMessage::PairPayload {
		payload: payload.to_link(),
		url: payload.url,
		code: payload.code,
		fingerprint: payload.fingerprint,
	}
}

/// Resolve the live IDE connection that owns `(ide_id, workspace)`.
/// Several connections can share an `ide_id` (one per workspace
/// process on that host, ADR 0014); the workspace slug picks the
/// right one. No fallback to an ide-only match — the phone builds
/// the pair from the `workspaces` reply, so a miss means the
/// process is gone and the caller should error, not misroute.
fn find_ide_conn<'a>(
	live: &'a HashMap<u64, IdeConnection>,
	ide_id: &str,
	workspace: &str,
) -> Option<(u64, &'a IdeConnection)> {
	live
		.iter()
		.find(|(_, c)| c.ide_id == ide_id && c.workspaces.iter().any(|w| w.id == workspace))
		.map(|(id, c)| (*id, c))
}

/// Check a presented IDE token. `Ok(ide)` if it maps to an enrolled
/// IDE; an `Error` [`ServerMessage`] otherwise. Mirror of
/// [`check_token`] for IDEs.
fn check_ide_token(ctx: &ServeCtx, token: &str) -> Result<EnrolledIde, ServerMessage> {
	match ctx.ides.ide_for_token(token) {
		Ok(Some(ide)) => Ok(ide),
		Ok(None) => Err(ServerMessage::Error {
			message: "unknown IDE token; enroll this IDE first".into(),
		}),
		Err(err) => Err(ServerMessage::Error {
			message: format!("IDE token check failed: {err}"),
		}),
	}
}

async fn handle_call(
	ctx: Arc<ServeCtx>,
	token: &str,
	workspace: &str,
	method: &str,
	params: serde_json::Value,
	ide: &str,
	out: &tokio::sync::mpsc::Sender<ServerMessage>,
) {
	if let Err(reply) = check_token(&ctx, token) {
		let _ = out.send(reply).await;
		return;
	}

	// Carrier selection (ADR 0031): empty `ide` = local-carrier
	// (Unix socket, today's path); present `ide` = remote-carrier
	// (forward to that enrolled IDE's held-open WS, 14.2).
	if ide.is_empty() {
		let socket = crate::discovery::socket_path(&ctx.workspaces_dir, workspace);
		let resp = match crate::relay::call(&socket, method, params).await {
			Ok(resp) => resp,
			Err(err) => {
				let _ = out
					.send(ServerMessage::Error {
						message: err.to_string(),
					})
					.await;
				return;
			}
		};
		let reply = if let Some(error) = resp.error {
			ServerMessage::Error { message: error }
		} else {
			ServerMessage::Result {
				value: resp.ok.unwrap_or(serde_json::Value::Null),
			}
		};
		let _ = out.send(reply).await;
		return;
	}

	// Remote-carrier: forward to the IDE connection that owns
	// `(ide, workspace)`. Allocate a forward id, register the
	// phone's sink so the `ForwardResult` reply can find it, and
	// send the `ForwardCall` to that connection's WS.
	let id = ctx.forward_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	let live = ctx.live_ides.lock().await;
	let Some((conn_id, conn)) = find_ide_conn(&live, ide, workspace) else {
		let _ = out
			.send(ServerMessage::Error {
				message: format!("IDE `{ide}` has no connected workspace `{workspace}`"),
			})
			.await;
		return;
	};
	// Register the pending forward *before* sending, so the reply
	// can't arrive before the entry exists.
	{
		let mut pending = ctx.pending_forwards.lock().await;
		pending.insert(
			id,
			PendingForward {
				phone_sink: out.clone(),
				conn_id,
			},
		);
	}
	// Send the ForwardCall to the IDE. If the send fails (IDE gone),
	// clean up the pending entry and error the phone immediately.
	if conn
		.sink
		.send(ServerMessage::ForwardCall {
			id,
			workspace: workspace.to_owned(),
			method: method.to_owned(),
			params,
		})
		.await
		.is_err()
	{
		ctx.pending_forwards.lock().await.remove(&id);
		let _ = out
			.send(ServerMessage::Error {
				message: format!("IDE `{ide}` connection lost mid-forward"),
			})
			.await;
		return;
	}
	// Spawn a per-forward timeout task: if the IDE doesn't reply
	// within `FORWARD_TIMEOUT`, reap the entry and error the phone so
	// its FIFO reply queue doesn't block forever on a hung call.
	let ctx_for_timeout = Arc::clone(&ctx);
	tokio::spawn(async move {
		tokio::time::sleep(FORWARD_TIMEOUT).await;
		let mut pending = ctx_for_timeout.pending_forwards.lock().await;
		if let Some(entry) = pending.remove(&id) {
			// Only error if we actually removed it — the IDE may
			// have replied in the race window between the timeout
			// firing and the lock being acquired, in which case
			// `remove` returns `None` and the phone already has its
			// reply.
			let _ = entry
				.phone_sink
				.send(ServerMessage::Error {
					message: "forwarded call timed out".into(),
				})
				.await;
		}
	});
}

/// How long the bridge waits for a forwarded call's reply (or a
/// forwarded subscribe's first event) before reaping the entry and
/// erroring the phone. The IDE's method handlers are quick (status /
/// session list), but the coder lock can be briefly contended
/// mid-turn, so we're generous — mirrors `relay::RESPONSE_TIMEOUT`.
const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
