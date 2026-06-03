//! Relay client — invoke a workspace process's RPC methods over its
//! `instance.sock` (Phase 13.1).
//!
//! The workspace process serves a small method surface on its
//! per-workspace socket via the `R` (RPC) request kind added to
//! [`moon_protocol::focus_socket`]. This module is the calling half:
//! connect, send one `RpcRequest`, read one `RpcResponse`, close.
//!
//! It speaks the *same* JSON-RPC shape the eventual WSS listener
//! (13.2) will carry to the phone — the bridge is a transport adapter
//! in front of this, not a second protocol. Keeping the call here (a
//! plain Unix-socket round-trip, no TLS, no auth) lets 13.1 be
//! exercised end to end before any network code exists.

use std::time::Duration;

use camino::Utf8Path;
use moon_protocol::focus_socket::{
	encode_request, is_truncated, parse_rpc_response, Request, RpcRequest, RpcResponse, MAX_REQUEST_BYTES,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// How long we wait to connect to a workspace socket before giving
/// up. Matches the focus-socket prober's 250 ms — the owner is local
/// and either answers immediately or isn't there.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long we wait for the response after sending the request. The
/// method handlers are quick reads (status / session list), but the
/// coder lock can be briefly contended mid-turn, so we're generous.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors invoking a relayed method.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
	#[error("could not connect to workspace socket {path}: {source}")]
	Connect {
		path: String,
		#[source]
		source: std::io::Error,
	},
	#[error("connection to workspace socket timed out")]
	ConnectTimeout,
	#[error("i/o error talking to workspace process: {0}")]
	Io(#[from] std::io::Error),
	#[error("timed out waiting for the workspace process to respond")]
	ResponseTimeout,
	#[error("workspace process closed the connection before replying")]
	Closed,
	#[error("workspace reply exceeded {MAX_REQUEST_BYTES} bytes")]
	TooLarge,
	#[error("could not parse the workspace reply")]
	BadReply,
}

/// Invoke `method` with `params` against the workspace whose socket
/// is at `socket_path`. Returns the raw [`RpcResponse`] — the caller
/// decides how to render `ok` vs `error`.
pub async fn call(socket_path: &Utf8Path, method: &str, params: serde_json::Value) -> Result<RpcResponse, RelayError> {
	let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(socket_path.as_std_path())).await {
		Ok(Ok(stream)) => stream,
		Ok(Err(source)) => {
			return Err(RelayError::Connect {
				path: socket_path.to_string(),
				source,
			})
		}
		Err(_) => return Err(RelayError::ConnectTimeout),
	};

	let rpc = RpcRequest {
		method: method.to_owned(),
		params,
	};
	// `to_string` on this fixed shape can't fail; the framing layer
	// rejects an embedded newline, which compact JSON never has.
	let json = serde_json::to_string(&rpc).map_err(|_| RelayError::BadReply)?;
	let bytes = encode_request(&Request::Rpc { json }).map_err(std::io::Error::from)?;
	stream.write_all(&bytes).await?;
	stream.flush().await?;

	read_response(&mut stream).await
}

/// Open a streaming subscription against `socket_path` for `method`.
/// Sends one `Subscribe` request, then invokes `on_event` for each
/// event line the workspace pushes until the connection closes or
/// `on_event` returns `false` (the caller wants to stop — e.g. the
/// phone disconnected). Used to relay `coder_events` to the phone.
pub async fn subscribe<F>(socket_path: &Utf8Path, method: &str, mut on_event: F) -> Result<(), RelayError>
where
	F: FnMut(serde_json::Value) -> bool,
{
	let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(socket_path.as_std_path())).await {
		Ok(Ok(stream)) => stream,
		Ok(Err(source)) => {
			return Err(RelayError::Connect {
				path: socket_path.to_string(),
				source,
			})
		}
		Err(_) => return Err(RelayError::ConnectTimeout),
	};

	let rpc = RpcRequest {
		method: method.to_owned(),
		params: serde_json::Value::Null,
	};
	let json = serde_json::to_string(&rpc).map_err(|_| RelayError::BadReply)?;
	let bytes = encode_request(&Request::Subscribe { json }).map_err(std::io::Error::from)?;
	stream.write_all(&bytes).await?;
	stream.flush().await?;

	// Each event is one framed RpcResponse line. Accumulate bytes and
	// drain every complete line as it arrives — no per-event timeout,
	// since a quiet stream is normal (the agent is just idle).
	let mut buf = Vec::with_capacity(1024);
	let mut tmp = [0u8; 4096];
	loop {
		let n = stream.read(&mut tmp).await?;
		if n == 0 {
			return Ok(()); // workspace ended the stream
		}
		buf.extend_from_slice(&tmp[..n]);
		// Drain whole lines.
		while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
			let line: Vec<u8> = buf.drain(..=nl).collect();
			let Ok((resp, _)) = parse_rpc_response(&line) else {
				continue; // skip a malformed line rather than tear down
			};
			if let Some(event) = resp.ok {
				if !on_event(event) {
					return Ok(());
				}
			}
		}
	}
}

async fn read_response(stream: &mut UnixStream) -> Result<RpcResponse, RelayError> {
	let mut buf = Vec::with_capacity(256);
	loop {
		if buf.len() > MAX_REQUEST_BYTES {
			return Err(RelayError::TooLarge);
		}
		let read = tokio::time::timeout(RESPONSE_TIMEOUT, stream.read_buf(&mut buf)).await;
		match read {
			Ok(Ok(0)) => {
				// EOF. If we already have a full line we'd have
				// returned below; reaching here means the process
				// closed before sending a complete reply.
				return Err(RelayError::Closed);
			}
			Ok(Ok(_)) => match parse_rpc_response(&buf) {
				Ok((resp, _consumed)) => return Ok(resp),
				Err(err) if is_truncated(&err) => continue,
				Err(_) => return Err(RelayError::BadReply),
			},
			Ok(Err(err)) => return Err(RelayError::Io(err)),
			Err(_) => return Err(RelayError::ResponseTimeout),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use moon_protocol::focus_socket::{encode_rpc_response, parse_request, Request};

	/// Stand up a fake workspace listener that answers one `R`
	/// request, and prove the client round-trips a result through it.
	#[tokio::test]
	async fn call_round_trips_against_a_fake_listener() {
		let dir = std::env::temp_dir().join(format!("moon-bridge-relay-{}", uuid::Uuid::new_v4().simple()));
		std::fs::create_dir_all(&dir).unwrap();
		let sock = Utf8Path::from_path(&dir).unwrap().join("instance.sock");
		let listener = tokio::net::UnixListener::bind(sock.as_std_path()).unwrap();

		let server = tokio::spawn(async move {
			let (mut stream, _) = listener.accept().await.unwrap();
			let mut buf = Vec::new();
			// Read until we can parse a request.
			let req = loop {
				let mut chunk = [0u8; 256];
				let n = stream.read(&mut chunk).await.unwrap();
				buf.extend_from_slice(&chunk[..n]);
				match parse_request(&buf) {
					Ok((req, _)) => break req,
					Err(e) if is_truncated(&e) => continue,
					Err(e) => panic!("bad request: {e}"),
				}
			};
			let Request::Rpc { json } = req else {
				panic!("expected an Rpc request");
			};
			let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
			assert_eq!(parsed.method, "coder_status");
			let resp = RpcResponse::ok(serde_json::json!({ "signed_in": false }));
			stream.write_all(&encode_rpc_response(&resp)).await.unwrap();
			stream.flush().await.unwrap();
		});

		let resp = call(&sock, "coder_status", serde_json::json!({})).await.unwrap();
		assert_eq!(resp.ok, Some(serde_json::json!({ "signed_in": false })));
		assert!(resp.error.is_none());
		server.await.unwrap();
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[tokio::test]
	async fn call_errors_when_socket_absent() {
		let missing = Utf8Path::new("/nonexistent/moon-bridge/instance.sock");
		let err = call(missing, "coder_status", serde_json::json!({})).await.unwrap_err();
		assert!(matches!(err, RelayError::Connect { .. } | RelayError::ConnectTimeout));
	}
}
