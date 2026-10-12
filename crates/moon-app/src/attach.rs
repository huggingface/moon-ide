//! The channel between a desktop front end and a remote workspace
//! process (ADR 0095): newline-delimited JSON frames over any byte
//! stream — in practice `moon-remote attach` bridged over `ssh`.
//!
//! The server side ([`AttachHub`] + [`serve_connection`]) runs in the
//! long-lived workspace process; the client side ([`AttachClient`])
//! runs in the desktop process, which re-emits events to its webview
//! and forwards the webview's `call`s. Only one desktop is attached
//! at a time: a new attachment detaches the previous one.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moon_protocol::MoonError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::events::{emit, EventSink};
use crate::AppCore;

/// Desktop → workspace process.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
	Call { id: u64, method: String, params: Value },
}

/// Workspace process → desktop.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
	Result {
		id: u64,
		result: Value,
	},
	Error {
		id: u64,
		error: MoonError,
	},
	Event {
		event: String,
		payload: Value,
	},
	/// Another desktop attached; the server closes this connection
	/// right after sending it.
	Detached,
}

/// Connection state the desktop reports to its webview on
/// [`REMOTE_STATUS_EVENT`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RemoteStatus {
	Connecting,
	Connected,
	Disconnected { error: String },
	Detached,
}

pub const REMOTE_STATUS_EVENT: &str = "remote:status";

// Unbounded on purpose: dropping frames would corrupt terminal output
// and lose call replies. A stalled reader ends with the connection.
type FrameTx = mpsc::UnboundedSender<ServerFrame>;

/// The workspace process's end: holds the attached desktop (if any)
/// and is the process's [`EventSink`], so events reach whichever
/// desktop is attached and are dropped while none is.
#[derive(Default)]
pub struct AttachHub {
	current: Mutex<Option<(u64, FrameTx)>>,
	next_id: AtomicU64,
}

impl AttachHub {
	fn attach(&self, tx: FrameTx) -> u64 {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let previous = self.current.lock().expect("attach hub poisoned").replace((id, tx));
		if let Some((_, old)) = previous {
			let _ = old.send(ServerFrame::Detached);
		}
		id
	}

	fn detach(&self, id: u64) {
		let mut current = self.current.lock().expect("attach hub poisoned");
		if matches!(current.as_ref(), Some((current_id, _)) if *current_id == id) {
			*current = None;
		}
	}
}

impl EventSink for AttachHub {
	fn emit(&self, event: &str, payload: Value) {
		let current = self.current.lock().expect("attach hub poisoned");
		if let Some((_, tx)) = current.as_ref() {
			let _ = tx.send(ServerFrame::Event {
				event: event.to_owned(),
				payload,
			});
		}
	}
}

/// Serve one attached desktop until it disconnects or is detached by
/// a newer attachment.
pub async fn serve_connection<R, W>(app: Arc<AppCore>, hub: Arc<AttachHub>, reader: R, writer: W)
where
	R: AsyncRead + Unpin + Send + 'static,
	W: AsyncWrite + Unpin + Send + 'static,
{
	let (tx, rx) = mpsc::unbounded_channel();
	let id = hub.attach(tx.clone());
	let writer_task = tokio::spawn(write_server_frames(writer, rx));

	let mut lines = BufReader::new(reader).lines();
	loop {
		tokio::select! {
			line = lines.next_line() => {
				let Ok(Some(line)) = line else {
					break;
				};
				let frame = match serde_json::from_str::<ClientFrame>(&line) {
					Ok(frame) => frame,
					Err(err) => {
						tracing::warn!(error = %err, "attach: ignoring malformed frame");
						continue;
					}
				};
				let ClientFrame::Call { id: call_id, method, params } = frame;
				let app = app.clone();
				let tx = tx.clone();
				tokio::spawn(async move {
					let reply = match app.dispatch(&method, params).await {
						Ok(result) => ServerFrame::Result { id: call_id, result },
						Err(error) => ServerFrame::Error { id: call_id, error },
					};
					let _ = tx.send(reply);
				});
			}
			// The writer ends after forwarding `Detached` (or on a
			// write error): stop reading too.
			_ = tx.closed() => break,
		}
	}
	hub.detach(id);
	drop(tx);
	let _ = writer_task.await;
}

async fn write_server_frames<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::UnboundedReceiver<ServerFrame>) {
	while let Some(frame) = rx.recv().await {
		let detached = matches!(frame, ServerFrame::Detached);
		let Ok(mut line) = serde_json::to_string(&frame) else {
			continue;
		};
		line.push('\n');
		if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
			break;
		}
		if detached {
			break;
		}
	}
	rx.close();
	let _ = writer.shutdown().await;
}

/// A byte stream to a remote workspace process plus whatever keeps it
/// alive (the `ssh` child), dropped when the connection ends.
pub struct Connection {
	pub reader: Box<dyn AsyncRead + Unpin + Send>,
	pub writer: Box<dyn AsyncWrite + Unpin + Send>,
	pub guard: Option<Box<dyn Send>>,
}

pub type Connect = Box<dyn Fn() -> Pin<Box<dyn Future<Output = std::io::Result<Connection>> + Send>> + Send + Sync>;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, MoonError>>>>>;

/// The desktop's end: keeps a connection open (reconnecting with
/// backoff), forwards calls, and re-emits events and
/// [`RemoteStatus`] changes to `events`.
pub struct AttachClient {
	outbound: Mutex<Option<mpsc::UnboundedSender<String>>>,
	pending: Pending,
	next_id: AtomicU64,
	reattach: Notify,
}

const MAX_BACKOFF: Duration = Duration::from_secs(10);

impl AttachClient {
	/// Start the connection loop. Must run on the tokio runtime.
	pub fn spawn(connect: Connect, events: Arc<dyn EventSink>) -> Arc<Self> {
		let client = Arc::new(Self {
			outbound: Mutex::new(None),
			pending: Arc::default(),
			next_id: AtomicU64::new(1),
			reattach: Notify::new(),
		});
		tokio::spawn(Arc::clone(&client).run(connect, events));
		client
	}

	pub async fn call(&self, method: &str, params: Value) -> Result<Value, MoonError> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let frame = ClientFrame::Call {
			id,
			method: method.to_owned(),
			params,
		};
		let line = serde_json::to_string(&frame).map_err(|e| MoonError::internal(e.to_string()))? + "\n";
		let (reply_tx, reply_rx) = oneshot::channel();
		self.pending.lock().expect("pending poisoned").insert(id, reply_tx);
		let sent = self
			.outbound
			.lock()
			.expect("outbound poisoned")
			.as_ref()
			.is_some_and(|tx| tx.send(line).is_ok());
		if !sent {
			self.pending.lock().expect("pending poisoned").remove(&id);
			return Err(not_connected());
		}
		reply_rx.await.unwrap_or_else(|_| Err(not_connected()))
	}

	/// Take the workspace back after another desktop detached this one.
	pub fn reattach(&self) {
		self.reattach.notify_one();
	}

	async fn run(self: Arc<Self>, connect: Connect, events: Arc<dyn EventSink>) {
		let mut backoff = Duration::from_secs(1);
		loop {
			emit(events.as_ref(), REMOTE_STATUS_EVENT, &RemoteStatus::Connecting);
			let connection = match connect().await {
				Ok(connection) => connection,
				Err(err) => {
					self.disconnected(&events, err.to_string(), &mut backoff).await;
					continue;
				}
			};
			let started = tokio::time::Instant::now();
			let ended = self.session(connection, &events).await;
			self.fail_pending();
			if started.elapsed() > MAX_BACKOFF {
				backoff = Duration::from_secs(1);
			}
			match ended {
				SessionEnd::Detached => {
					emit(events.as_ref(), REMOTE_STATUS_EVENT, &RemoteStatus::Detached);
					self.reattach.notified().await;
					backoff = Duration::from_secs(1);
				}
				SessionEnd::Closed => {
					self
						.disconnected(&events, "connection closed".to_owned(), &mut backoff)
						.await;
				}
			}
		}
	}

	async fn disconnected(&self, events: &Arc<dyn EventSink>, error: String, backoff: &mut Duration) {
		emit(
			events.as_ref(),
			REMOTE_STATUS_EVENT,
			&RemoteStatus::Disconnected { error },
		);
		tokio::time::sleep(*backoff).await;
		*backoff = (*backoff * 2).min(MAX_BACKOFF);
	}

	async fn session(&self, connection: Connection, events: &Arc<dyn EventSink>) -> SessionEnd {
		let Connection {
			reader,
			mut writer,
			guard,
		} = connection;
		let (tx, mut rx) = mpsc::unbounded_channel::<String>();
		*self.outbound.lock().expect("outbound poisoned") = Some(tx);
		// Only after `outbound` is set, so a caller reacting to
		// `Connected` can call straight away.
		emit(events.as_ref(), REMOTE_STATUS_EVENT, &RemoteStatus::Connected);
		let writer_task = tokio::spawn(async move {
			while let Some(line) = rx.recv().await {
				if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
					break;
				}
			}
		});

		let mut end = SessionEnd::Closed;
		let mut lines = BufReader::new(reader).lines();
		while let Ok(Some(line)) = lines.next_line().await {
			let frame = match serde_json::from_str::<ServerFrame>(&line) {
				Ok(frame) => frame,
				Err(err) => {
					tracing::warn!(error = %err, "attach: ignoring malformed frame");
					continue;
				}
			};
			match frame {
				ServerFrame::Result { id, result } => self.resolve(id, Ok(result)),
				ServerFrame::Error { id, error } => self.resolve(id, Err(error)),
				ServerFrame::Event { event, payload } => events.emit(&event, payload),
				ServerFrame::Detached => {
					end = SessionEnd::Detached;
					break;
				}
			}
		}
		*self.outbound.lock().expect("outbound poisoned") = None;
		writer_task.abort();
		drop(guard);
		end
	}

	fn resolve(&self, id: u64, reply: Result<Value, MoonError>) {
		let waiter = self.pending.lock().expect("pending poisoned").remove(&id);
		if let Some(waiter) = waiter {
			let _ = waiter.send(reply);
		}
	}

	fn fail_pending(&self) {
		let waiters: Vec<_> = self.pending.lock().expect("pending poisoned").drain().collect();
		for (_, waiter) in waiters {
			let _ = waiter.send(Err(not_connected()));
		}
	}
}

enum SessionEnd {
	Closed,
	Detached,
}

fn not_connected() -> MoonError {
	MoonError::HostUnavailable("remote workspace not connected".to_owned())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	/// Records every event and wakes waiters on each one.
	#[derive(Default)]
	struct Recorder {
		seen: Mutex<Vec<(String, Value)>>,
		changed: Notify,
	}

	impl EventSink for Recorder {
		fn emit(&self, event: &str, payload: Value) {
			self.seen.lock().unwrap().push((event.to_owned(), payload));
			self.changed.notify_waiters();
		}
	}

	impl Recorder {
		async fn wait_for(&self, event: &str, payload: Value) {
			self.wait_for_count(event, payload, 1).await;
		}

		async fn wait_for_count(&self, event: &str, payload: Value, count: usize) {
			let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
			loop {
				let changed = self.changed.notified();
				if self.count(event, &payload) >= count {
					return;
				}
				assert!(
					tokio::time::timeout_at(deadline, changed).await.is_ok(),
					"timed out waiting for {event} {payload}; saw {:?}",
					self.seen.lock().unwrap()
				);
			}
		}

		fn count(&self, event: &str, payload: &Value) -> usize {
			self
				.seen
				.lock()
				.unwrap()
				.iter()
				.filter(|(e, p)| e == event && p == payload)
				.count()
		}
	}

	/// A client whose every (re)connect attaches a fresh in-memory
	/// stream to the shared server.
	fn client(app: Arc<AppCore>, hub: Arc<AttachHub>, events: Arc<Recorder>) -> Arc<AttachClient> {
		let connect: Connect = Box::new(move || {
			let app = app.clone();
			let hub = hub.clone();
			Box::pin(async move {
				let (client_end, server_end) = tokio::io::duplex(64 * 1024);
				let (server_read, server_write) = tokio::io::split(server_end);
				tokio::spawn(serve_connection(app, hub, server_read, server_write));
				let (reader, writer) = tokio::io::split(client_end);
				Ok(Connection {
					reader: Box::new(reader),
					writer: Box::new(writer),
					guard: None,
				})
			})
		});
		AttachClient::spawn(connect, events)
	}

	fn status(status: RemoteStatus) -> Value {
		serde_json::to_value(status).unwrap()
	}

	#[tokio::test]
	async fn calls_round_trip_and_events_reach_the_attached_desktop() {
		let hub = Arc::new(AttachHub::default());
		let (app, _data) = crate::tests::app_with_events(hub.clone());
		let app = Arc::new(app);
		let events = Arc::new(Recorder::default());
		let desktop = client(app, hub.clone(), events.clone());
		events
			.wait_for(REMOTE_STATUS_EVENT, status(RemoteStatus::Connected))
			.await;

		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("a.txt");
		let path = path.to_str().unwrap();
		desktop
			.call("fs_write_file_host", json!({ "path": path, "text": "over the wire" }))
			.await
			.unwrap();
		let read = desktop
			.call("fs_read_file_host", json!({ "path": path }))
			.await
			.unwrap();
		assert_eq!(read["text"], "over the wire");
		let err = desktop.call("nope", Value::Null).await.unwrap_err();
		assert!(err.to_string().contains("unknown method `nope`"), "{err}");

		hub.emit("terminal:output", json!({ "stream_id": "t1" }));
		events.wait_for("terminal:output", json!({ "stream_id": "t1" })).await;
	}

	#[tokio::test]
	async fn a_second_desktop_takes_over_and_the_first_can_take_it_back() {
		let hub = Arc::new(AttachHub::default());
		let (app, _data) = crate::tests::app_with_events(hub.clone());
		let app = Arc::new(app);
		let first_events = Arc::new(Recorder::default());
		let first = client(app.clone(), hub.clone(), first_events.clone());
		first_events
			.wait_for(REMOTE_STATUS_EVENT, status(RemoteStatus::Connected))
			.await;

		let second_events = Arc::new(Recorder::default());
		let second = client(app, hub.clone(), second_events.clone());
		second_events
			.wait_for(REMOTE_STATUS_EVENT, status(RemoteStatus::Connected))
			.await;
		first_events
			.wait_for(REMOTE_STATUS_EVENT, status(RemoteStatus::Detached))
			.await;

		// Events go to the attached desktop only, and the detached one
		// can't call.
		hub.emit("ping", json!(1));
		second_events.wait_for("ping", json!(1)).await;
		assert_eq!(first_events.count("ping", &json!(1)), 0);
		assert!(first.call("workspace_list", Value::Null).await.is_err());

		first.reattach();
		second_events
			.wait_for(REMOTE_STATUS_EVENT, status(RemoteStatus::Detached))
			.await;
		first_events
			.wait_for_count(REMOTE_STATUS_EVENT, status(RemoteStatus::Connected), 2)
			.await;
		assert!(second.call("workspace_list", Value::Null).await.is_err());
		first.call("workspace_list", Value::Null).await.unwrap();
	}
}
