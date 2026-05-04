//! Thin JSON-RPC client on top of [`framing`].
//!
//! One client instance owns an I/O pair (stdin + stdout of a child
//! process), a request-id counter, and two actor tasks:
//!
//! - **Reader task**: pulls framed messages, decodes them, and
//!   dispatches. Responses land in the matching `oneshot` from the
//!   pending map. Server → client notifications go to the
//!   `notifications` broadcast channel the broker subscribes to.
//!   Server → client *requests* get a `null` response so the server
//!   doesn't block — we don't implement any client-side capabilities
//!   yet (see `crate::lsp::server` for why that's OK).
//!
//! - **Writer task**: serialises outbound requests/notifications and
//!   writes them framed. Bounded channel, so backpressure pushes back
//!   on whoever is spamming.
//!
//! The public surface is `request`, `notify`, and `shutdown`. The
//! actor architecture is an implementation detail — callers only see
//! futures.
//!
//! [`framing`]: super::framing

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

use super::framing;

/// One JSON-RPC error as surfaced to the caller. We keep `code` as
/// `i64` matching the spec; known codes (`-32600..`) are wrapped by
/// higher layers.
#[derive(Debug, Clone, thiserror::Error)]
#[error("LSP error {code}: {message}")]
pub struct LspRpcError {
	pub code: i64,
	pub message: String,
}

/// Top-level errors the client surfaces. IO errors include the child
/// process dying; `Rpc` is a well-formed error response from the
/// server; `Shutdown` is emitted when the caller awaits a request
/// after `shutdown` has run or the child has exited.
#[derive(Debug, thiserror::Error)]
pub enum LspClientError {
	#[error("lsp io: {0}")]
	Io(String),
	#[error(transparent)]
	Rpc(#[from] LspRpcError),
	#[error("lsp client shut down")]
	Shutdown,
	#[error("lsp decode: {0}")]
	Decode(String),
}

/// Notification pushed by the server (method + params). Broker
/// subscribes to this to route `publishDiagnostics` and friends.
#[derive(Debug, Clone)]
pub struct ServerNotification {
	pub method: String,
	pub params: Value,
}

/// Pending-responses map. One entry per in-flight request, keyed by
/// the monotonic id we handed to the server; dropped on response,
/// shutdown, or server EOF. Aliased so the signatures below don't
/// have to repeat the five-deep generic.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspRpcError>>>>>;

/// Outbound writer command. Requests are routed to the reader loop's
/// `pending` map by id; the writer doesn't own the response channel.
enum Outbound {
	Request {
		id: i64,
		method: String,
		params: Value,
	},
	Notification {
		method: String,
		params: Value,
	},
	/// Server → client request response (always `null` for stage 1).
	Response {
		id: Value,
		result: Value,
	},
	Shutdown,
}

pub struct LspClient {
	next_id: AtomicI64,
	tx: mpsc::Sender<Outbound>,
	pending: PendingMap,
	// We keep join handles around so the owner can await a clean
	// shutdown. In the common case the broker just drops the client
	// and the tasks exit when their channels close.
	_reader: JoinHandle<()>,
	_writer: JoinHandle<()>,
}

impl LspClient {
	/// Construct a new client over the given I/O halves. `stderr`
	/// logging is the caller's problem — LSP only speaks JSON-RPC
	/// over stdin/stdout, and whatever the child writes to stderr
	/// is diagnostics we'd rather pipe to `tracing` ourselves.
	///
	/// `notifications` is where server → client notifications land.
	/// The broker owns the receiver; one broadcast channel so the
	/// broker can multiplex subscribers cheaply if we ever want to.
	pub fn spawn<R, W>(stdin: W, stdout: R, notifications: broadcast::Sender<ServerNotification>) -> Self
	where
		R: AsyncRead + Unpin + Send + 'static,
		W: AsyncWrite + Unpin + Send + 'static,
	{
		// 256 is more than enough for UI-driven request load. If the
		// broker ever queues a batch bigger than this, something is
		// probably looping.
		let (tx, rx) = mpsc::channel::<Outbound>(256);
		let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

		let writer_task = tokio::spawn(writer_loop(stdin, rx));
		let reader_task = tokio::spawn(reader_loop(stdout, pending.clone(), tx.clone(), notifications));

		Self {
			next_id: AtomicI64::new(1),
			tx,
			pending,
			_reader: reader_task,
			_writer: writer_task,
		}
	}

	/// Send a request, await the typed response. `R` is the result
	/// type the server returns; params are serialised with serde.
	pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R, LspClientError>
	where
		P: Serialize,
		R: DeserializeOwned,
	{
		let id = self.next_id.fetch_add(1, Ordering::SeqCst);
		let (responder, rx) = oneshot::channel();
		self.pending.lock().await.insert(id, responder);

		let params_value = serde_json::to_value(params).map_err(|e| LspClientError::Decode(e.to_string()))?;
		let send_result = self
			.tx
			.send(Outbound::Request {
				id,
				method: method.to_owned(),
				params: params_value,
			})
			.await;
		if send_result.is_err() {
			// Writer loop is already gone. Drop the pending entry
			// so a later shutdown doesn't see a dangling sender.
			self.pending.lock().await.remove(&id);
			return Err(LspClientError::Shutdown);
		}

		let value = rx.await.map_err(|_| LspClientError::Shutdown)??;
		serde_json::from_value(value).map_err(|e| LspClientError::Decode(e.to_string()))
	}

	/// Fire-and-forget notification.
	pub async fn notify<P>(&self, method: &str, params: P) -> Result<(), LspClientError>
	where
		P: Serialize,
	{
		let params_value = serde_json::to_value(params).map_err(|e| LspClientError::Decode(e.to_string()))?;
		self
			.tx
			.send(Outbound::Notification {
				method: method.to_owned(),
				params: params_value,
			})
			.await
			.map_err(|_| LspClientError::Shutdown)?;
		Ok(())
	}

	/// Tell the writer loop to quit — the reader loop follows
	/// naturally when the child closes stdout. Non-blocking; the
	/// caller doesn't need to await task completion in the normal
	/// drop-the-broker path.
	pub async fn shutdown(&self) {
		let _ = self.tx.send(Outbound::Shutdown).await;
	}
}

async fn writer_loop<W: AsyncWrite + Unpin + Send + 'static>(mut stdin: W, mut rx: mpsc::Receiver<Outbound>) {
	while let Some(out) = rx.recv().await {
		let payload = match out {
			Outbound::Request { id, method, params, .. } => {
				// JSON-RPC request.
				json!({
					"jsonrpc": "2.0",
					"id": id,
					"method": method,
					"params": params,
				})
			}
			Outbound::Notification { method, params } => {
				json!({
					"jsonrpc": "2.0",
					"method": method,
					"params": params,
				})
			}
			Outbound::Response { id, result } => {
				json!({
					"jsonrpc": "2.0",
					"id": id,
					"result": result,
				})
			}
			Outbound::Shutdown => {
				break;
			}
		};
		let bytes = match serde_json::to_vec(&payload) {
			Ok(b) => b,
			Err(e) => {
				tracing::warn!(error = %e, "lsp: failed to encode outbound message");
				continue;
			}
		};
		if let Err(e) = framing::write_message(&mut stdin, &bytes).await {
			tracing::warn!(error = %e, "lsp: stdin write failed, shutting writer");
			break;
		}
	}
}

async fn reader_loop<R: AsyncRead + Unpin + Send + 'static>(
	stdout: R,
	pending: PendingMap,
	tx: mpsc::Sender<Outbound>,
	notifications: broadcast::Sender<ServerNotification>,
) {
	let mut reader = BufReader::new(stdout);
	loop {
		let bytes = match framing::read_message(&mut reader).await {
			Ok(b) => b,
			Err(e) => {
				if e.kind() != std::io::ErrorKind::UnexpectedEof {
					tracing::warn!(error = %e, "lsp: stdout read failed");
				}
				// Fail every pending request so callers wake up.
				let mut guard = pending.lock().await;
				for (_, sender) in guard.drain() {
					let _ = sender.send(Err(LspRpcError {
						code: -32099,
						message: "lsp client shut down".to_owned(),
					}));
				}
				return;
			}
		};
		let value: Value = match serde_json::from_slice(&bytes) {
			Ok(v) => v,
			Err(e) => {
				tracing::warn!(error = %e, "lsp: bad JSON from server");
				continue;
			}
		};

		// Three shapes: response (has `id` + `result`/`error`,
		// no `method`), server->client request (has `id` +
		// `method`), notification (has `method`, no `id`).
		let has_id = value.get("id").is_some();
		let has_method = value.get("method").is_some();

		if has_method && has_id {
			// Server → client request. Stage 1: acknowledge with
			// `null`. That's enough for `workspace/configuration`
			// (server treats null as "no config, use defaults"),
			// `window/workDoneProgress/create` (server fires
			// progress but we ignore it), and
			// `client/registerCapability` (we already exposed
			// what we support in `initialize`).
			let id = value.get("id").cloned().unwrap_or(Value::Null);
			let method = value
				.get("method")
				.and_then(Value::as_str)
				.unwrap_or("<unknown>")
				.to_owned();
			tracing::trace!(method = %method, "lsp: server->client request, responding null");
			if tx
				.send(Outbound::Response {
					id,
					result: Value::Null,
				})
				.await
				.is_err()
			{
				return;
			}
			continue;
		}
		if has_method {
			let method = value.get("method").and_then(Value::as_str).unwrap_or("").to_owned();
			let params = value.get("params").cloned().unwrap_or(Value::Null);
			let _ = notifications.send(ServerNotification { method, params });
			continue;
		}
		if has_id {
			let id = match value.get("id").and_then(Value::as_i64) {
				Some(i) => i,
				None => {
					tracing::warn!("lsp: response without numeric id, dropping");
					continue;
				}
			};
			let responder = pending.lock().await.remove(&id);
			let Some(responder) = responder else {
				tracing::warn!(id, "lsp: response for unknown request id");
				continue;
			};
			if let Some(err) = value.get("error") {
				let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
				let message = err
					.get("message")
					.and_then(Value::as_str)
					.unwrap_or("<no message>")
					.to_owned();
				let _ = responder.send(Err(LspRpcError { code, message }));
				continue;
			}
			let result = value.get("result").cloned().unwrap_or(Value::Null);
			let _ = responder.send(Ok(result));
			continue;
		}

		tracing::warn!("lsp: message with neither method nor id, dropping");
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::duplex;

	/// End-to-end smoke test that wires the client to a scripted
	/// "server" spoken over in-memory pipes, confirming the
	/// request/response roundtrip plus a notification dispatch.
	#[tokio::test]
	async fn round_trips_request_and_notification() {
		let (client_rx, mut server_tx) = duplex(4096);
		let (mut server_rx, client_tx) = duplex(4096);

		let (notif_tx, mut notif_rx) = broadcast::channel(4);
		let client = LspClient::spawn(client_tx, client_rx, notif_tx);

		// Server: read one request, reply with a known result, then
		// send a notification.
		let server = tokio::spawn(async move {
			let mut reader = BufReader::new(&mut server_rx);
			let msg = framing::read_message(&mut reader).await.unwrap();
			let req: Value = serde_json::from_slice(&msg).unwrap();
			assert_eq!(req["method"], "ping");
			let id = req["id"].clone();
			let response = serde_json::to_vec(&json!({
				"jsonrpc": "2.0",
				"id": id,
				"result": { "ok": true },
			}))
			.unwrap();
			framing::write_message(&mut server_tx, &response).await.unwrap();
			let notification = serde_json::to_vec(&json!({
				"jsonrpc": "2.0",
				"method": "server/hello",
				"params": { "msg": "hi" },
			}))
			.unwrap();
			framing::write_message(&mut server_tx, &notification).await.unwrap();
		});

		#[derive(serde::Deserialize, Debug)]
		struct PingResult {
			ok: bool,
		}
		let res: PingResult = client.request("ping", json!({})).await.unwrap();
		assert!(res.ok);

		let notif = notif_rx.recv().await.unwrap();
		assert_eq!(notif.method, "server/hello");
		assert_eq!(notif.params["msg"], "hi");

		server.await.unwrap();
	}
}
