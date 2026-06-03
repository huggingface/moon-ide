//! Wire format for the per-workspace `instance.sock`.
//!
//! Two parties speak this protocol:
//!
//! - The IDE process (host-side, owns the listener). Accepts
//!   incoming connections from any sibling launcher process
//!   or any in-container caller that has the socket bind-mounted.
//! - Sibling launcher processes (host-side) that want to focus
//!   an already-open workspace, and the `moon-edit` shim
//!   ([`crates/moon-edit`]) running inside the workspace shell
//!   container, which forwards a `$GIT_EDITOR` invocation to
//!   the host IDE and blocks for the user to finish.
//!
//! See [ADR 0021](../../../specs/decisions/0021-git-editor-forward.md)
//! for the design notes, [`specs/containers.md`](../../../specs/containers.md)
//! § "Editor forwarding" for the user-visible behaviour, and
//! [`src-tauri/src/focus_socket.rs`](../../../src-tauri/src/focus_socket.rs)
//! for the host-side listener.
//!
//! ## Wire format
//!
//! Newline-framed, ASCII-tagged messages:
//!
//! ```text
//! request := "F\n"                     # bring the IDE window to front
//!          | "E\n" <host-path> "\n"    # open <host-path> as a buffer
//!                                      # and block until the user is done
//!
//! reply := "OK\n"                      # only valid in response to "E"
//!        | "CANCEL\n"                  # only valid in response to "E"
//! ```
//!
//! `"F\n"` doesn't carry a reply — the sender writes and disconnects.
//! `"E\n…"` parks until the user either saves+closes the buffer
//! (`"OK\n"`) or closes it without saving (`"CANCEL\n"`). EOF on
//! the socket while the request is parked counts as a cancel from
//! the receiver's point of view; the sender will likely already
//! be gone by then.
//!
//! The framing is line-oriented because the only field we carry —
//! a host path — is allowed to contain anything except `\n` on a
//! POSIX filesystem. We're not trying to be a serialisation format
//! for arbitrary data here; one path, one line, done.
//!
//! ## Why not JSON
//!
//! The protocol is fixed-shape and tiny, and we need the shim
//! that speaks it to fit in `moon-base`. Pulling in `serde_json`
//! for "two tagged variants plus a string" would bloat the
//! binary for no clarity gain; the manual framing is short
//! enough to audit by eye.

use std::io;

/// Inbound message kinds from a connecting client.
///
/// The wire tag is the first byte of the request line; everything
/// up to (and not including) the trailing `\n` is the body, parsed
/// per-variant. Unknown tags are reported as
/// [`ParseError::UnknownTag`] so callers can decide whether to
/// log-and-drop or surface as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
	/// "Bring the IDE window to front." Carries no body.
	Focus,
	/// "Open this host-absolute path as a buffer and block until
	/// the user finishes or cancels." The path is sent verbatim;
	/// the listener is responsible for any normalisation /
	/// validation it cares about.
	Edit { host_path: String },
	/// "Invoke a method and return its result." The body is a
	/// single-line JSON-encoded [`RpcRequest`]. Used by `moon-bridge`
	/// (Phase 13) to reach the workspace process's coder + git
	/// surface from outside the Tauri webview. The listener replies
	/// with a single-line JSON [`RpcResponse`] (sent via
	/// [`encode_rpc_response`]) and closes the connection.
	///
	/// The body is one line because the framing is line-oriented;
	/// JSON never contains a literal newline unless pretty-printed,
	/// and we always send it compact.
	Rpc { json: String },
	/// "Subscribe to an event stream." Like [`Request::Rpc`] but the
	/// reply is **many** lines: the listener writes one compact-JSON
	/// [`RpcResponse`] per event (its `ok` field carrying the event
	/// payload) and keeps the connection open until the client
	/// disconnects or the stream ends. Used by `moon-bridge` to relay
	/// the workspace's `coder:event` stream to the phone. Same `R`-vs-
	/// `S` split rationale as a unary vs server-streaming RPC.
	Subscribe { json: String },
}

/// Reply kinds the IDE listener sends back on the same
/// connection. Only meaningful for [`Request::Edit`]; the
/// [`Request::Focus`] path is fire-and-forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
	/// User saved and finished. The caller (`moon-edit`) exits
	/// zero so `git` proceeds with the edited file.
	Ok,
	/// User closed the tab without finishing. The caller exits
	/// non-zero so `git` aborts the commit / rebase / whatever.
	Cancel,
}

/// Encoding errors. Effectively only one shape — a path that
/// contains a newline — but kept as an enum so we can grow it
/// without breaking callers.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
	/// `Request::Edit { host_path }` carries a path with an
	/// embedded `\n`. Real-world POSIX filesystems can hold
	/// these; we refuse to encode them rather than re-frame the
	/// protocol around it. Callers should surface this as a
	/// shim-side error.
	#[error("host path contains a newline; cannot encode")]
	NewlineInPath,
	/// `Request::Rpc { json }` body contained a literal newline.
	/// We always send compact JSON, so this only happens if a
	/// caller hand-built a pretty-printed body; refuse rather than
	/// silently break the line framing.
	#[error("rpc body contains a newline; cannot encode")]
	NewlineInBody,
}

/// Decoding errors. Bytes from the wire are not trusted; every
/// failure path returns one of these.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
	/// Empty input or no trailing newline before EOF.
	#[error("request line was empty or unterminated")]
	Truncated,
	/// First byte didn't match any known tag.
	#[error("unknown request tag {tag:?}")]
	UnknownTag { tag: char },
	/// `E\n…` body wasn't valid UTF-8.
	#[error("edit body was not valid utf-8: {0}")]
	BadUtf8(#[from] std::string::FromUtf8Error),
	/// An `R` reply line was present and complete but wasn't valid
	/// JSON for the expected shape. Distinct from [`Truncated`] so
	/// the reader doesn't spin waiting for more bytes that won't fix
	/// a malformed-but-complete line.
	#[error("rpc response body was not valid json")]
	BadRpcJson,
}

/// Maximum bytes the listener is willing to read for one request.
/// Path strings on POSIX cap at `PATH_MAX` (4096 bytes on Linux);
/// we add headroom for the tag, the leading newline, and any
/// path-name escape the user has managed to invent.
pub const MAX_REQUEST_BYTES: usize = 8192;

/// Encode `req` to a [`Vec<u8>`] ready to write to the socket.
/// Always ends in `\n`.
pub fn encode_request(req: &Request) -> Result<Vec<u8>, EncodeError> {
	let mut out = Vec::new();
	match req {
		Request::Focus => out.extend_from_slice(b"F\n"),
		Request::Edit { host_path } => {
			if host_path.as_bytes().contains(&b'\n') {
				return Err(EncodeError::NewlineInPath);
			}
			out.push(b'E');
			out.push(b'\n');
			out.extend_from_slice(host_path.as_bytes());
			out.push(b'\n');
		}
		Request::Rpc { json } => {
			if json.as_bytes().contains(&b'\n') {
				return Err(EncodeError::NewlineInBody);
			}
			out.push(b'R');
			out.push(b'\n');
			out.extend_from_slice(json.as_bytes());
			out.push(b'\n');
		}
		Request::Subscribe { json } => {
			if json.as_bytes().contains(&b'\n') {
				return Err(EncodeError::NewlineInBody);
			}
			out.push(b'S');
			out.push(b'\n');
			out.extend_from_slice(json.as_bytes());
			out.push(b'\n');
		}
	}
	Ok(out)
}

/// Encode a reply. Currently `OK\n` / `CANCEL\n`.
pub fn encode_reply(reply: Reply) -> Vec<u8> {
	match reply {
		Reply::Ok => b"OK\n".to_vec(),
		Reply::Cancel => b"CANCEL\n".to_vec(),
	}
}

/// Parse one request from `buf`. Returns the parsed request **and**
/// the number of bytes consumed, so callers using a buffered
/// reader can advance correctly. `buf` is expected to contain at
/// least the first framing line; an `E` request also requires its
/// path line to be present.
///
/// Returns [`ParseError::Truncated`] if there isn't enough data
/// yet — the caller should read more bytes and retry, up to
/// [`MAX_REQUEST_BYTES`].
pub fn parse_request(buf: &[u8]) -> Result<(Request, usize), ParseError> {
	let first_nl = buf.iter().position(|&b| b == b'\n').ok_or(ParseError::Truncated)?;
	let tag_line = &buf[..first_nl];
	let after_tag = first_nl + 1;
	// We accept a tag line with optional trailing whitespace (CRLF
	// from a non-POSIX client, stray spaces from a hand-typed
	// `nc` test). Empty tag line is treated as truncated.
	let tag_byte = tag_line
		.iter()
		.find(|b| !b.is_ascii_whitespace())
		.ok_or(ParseError::Truncated)?;
	match *tag_byte {
		b'F' => Ok((Request::Focus, after_tag)),
		b'E' => {
			let body_start = after_tag;
			let body_nl = buf[body_start..]
				.iter()
				.position(|&b| b == b'\n')
				.ok_or(ParseError::Truncated)?;
			let body = &buf[body_start..body_start + body_nl];
			let host_path = String::from_utf8(body.to_vec())?;
			Ok((Request::Edit { host_path }, body_start + body_nl + 1))
		}
		b'R' | b'S' => {
			let body_start = after_tag;
			let body_nl = buf[body_start..]
				.iter()
				.position(|&b| b == b'\n')
				.ok_or(ParseError::Truncated)?;
			let body = &buf[body_start..body_start + body_nl];
			let json = String::from_utf8(body.to_vec())?;
			let req = if *tag_byte == b'S' {
				Request::Subscribe { json }
			} else {
				Request::Rpc { json }
			};
			Ok((req, body_start + body_nl + 1))
		}
		other => Err(ParseError::UnknownTag { tag: other as char }),
	}
}

/// Parse one reply from `buf`. Mirror of [`parse_request`].
pub fn parse_reply(buf: &[u8]) -> Result<(Reply, usize), ParseError> {
	let nl = buf.iter().position(|&b| b == b'\n').ok_or(ParseError::Truncated)?;
	// `String::from_utf8` is the route that gives us the
	// `FromUtf8Error` shape our `ParseError::BadUtf8` variant
	// wraps; we deliberately copy the line bytes here so the
	// error's `into_bytes()` makes sense if anyone reads it.
	let line = String::from_utf8(buf[..nl].to_vec())?;
	let trimmed = line.trim();
	match trimmed {
		"OK" => Ok((Reply::Ok, nl + 1)),
		"CANCEL" => Ok((Reply::Cancel, nl + 1)),
		other => Err(ParseError::UnknownTag {
			tag: other.chars().next().unwrap_or('?'),
		}),
	}
}

/// Convenience: classify a parse error as "caller should read
/// more bytes" vs "this is a hard fail". Useful in the async
/// listener loop where [`ParseError::Truncated`] is the signal
/// to await more data, not log an error.
pub fn is_truncated(err: &ParseError) -> bool {
	matches!(err, ParseError::Truncated)
}

/// One method invocation carried in a [`Request::Rpc`] body.
///
/// Deliberately minimal — `method` is a stable string the
/// listener matches on, `params` is opaque JSON the handler
/// destructures. This is the wire shape `moon-bridge` (and, later,
/// the companion PWA via the bridge) uses to reach the workspace
/// process's coder + git surface. The set of supported `method`
/// strings is whatever the listener wires up (Phase 13), not a
/// fixed enum here — keeping it a string means adding a method is a
/// handler change, not a protocol-crate change.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RpcRequest {
	pub method: String,
	#[serde(default)]
	pub params: serde_json::Value,
}

/// The listener's reply to a [`RpcRequest`]. Exactly one of `ok` /
/// `error` is set. `ok` carries the method's result JSON; `error`
/// carries a human-readable message (the workspace process's
/// `MoonError` / `CoderError` display string).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RpcResponse {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub ok: Option<serde_json::Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
}

impl RpcResponse {
	/// Build a success response.
	pub fn ok(value: serde_json::Value) -> Self {
		Self {
			ok: Some(value),
			error: None,
		}
	}

	/// Build an error response.
	pub fn error(message: impl Into<String>) -> Self {
		Self {
			ok: None,
			error: Some(message.into()),
		}
	}
}

/// Encode an [`RpcResponse`] as a single-line framed reply ready to
/// write to the socket. Always compact JSON + trailing `\n`.
pub fn encode_rpc_response(resp: &RpcResponse) -> Vec<u8> {
	// Serialisation of this fixed-shape struct can't fail; the
	// fallback keeps the function infallible for callers in the
	// hot socket path.
	let mut line =
		serde_json::to_string(resp).unwrap_or_else(|_| r#"{"error":"failed to encode rpc response"}"#.to_owned());
	line.push('\n');
	line.into_bytes()
}

/// Parse a single-line [`RpcResponse`] from `buf`. Mirror of
/// [`parse_reply`] for the RPC path; returns the response and the
/// bytes consumed so a buffered reader can advance.
pub fn parse_rpc_response(buf: &[u8]) -> Result<(RpcResponse, usize), ParseError> {
	let nl = buf.iter().position(|&b| b == b'\n').ok_or(ParseError::Truncated)?;
	let line = std::str::from_utf8(&buf[..nl]).map_err(|_| ParseError::BadRpcJson)?;
	let resp: RpcResponse = serde_json::from_str(line).map_err(|_| ParseError::BadRpcJson)?;
	Ok((resp, nl + 1))
}

/// Convert a [`ParseError`] into a generic [`io::Error`] —
/// callers that bubble socket errors up as `io::Error` (the
/// shim's `main`, mostly) want one shape, not two.
impl From<ParseError> for io::Error {
	fn from(err: ParseError) -> Self {
		io::Error::new(io::ErrorKind::InvalidData, err)
	}
}

impl From<EncodeError> for io::Error {
	fn from(err: EncodeError) -> Self {
		io::Error::new(io::ErrorKind::InvalidInput, err)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn encode_focus_is_two_bytes() {
		assert_eq!(encode_request(&Request::Focus).unwrap(), b"F\n");
	}

	#[test]
	fn encode_edit_frames_path_with_trailing_newline() {
		let bytes = encode_request(&Request::Edit {
			host_path: "/home/me/code/moon-ide/.git/COMMIT_EDITMSG".to_string(),
		})
		.unwrap();
		assert_eq!(bytes, b"E\n/home/me/code/moon-ide/.git/COMMIT_EDITMSG\n");
	}

	#[test]
	fn encode_edit_refuses_newline_in_path() {
		let err = encode_request(&Request::Edit {
			host_path: "/tmp/with\nnewline".to_string(),
		})
		.unwrap_err();
		assert!(matches!(err, EncodeError::NewlineInPath));
	}

	#[test]
	fn parse_focus_consumes_two_bytes() {
		let (req, n) = parse_request(b"F\nleftover").unwrap();
		assert_eq!(req, Request::Focus);
		assert_eq!(n, 2);
	}

	#[test]
	fn parse_edit_round_trips() {
		let path = "/home/me/code/x/COMMIT_EDITMSG";
		let bytes = encode_request(&Request::Edit {
			host_path: path.to_string(),
		})
		.unwrap();
		let (req, n) = parse_request(&bytes).unwrap();
		assert_eq!(
			req,
			Request::Edit {
				host_path: path.to_string()
			}
		);
		assert_eq!(n, bytes.len());
	}

	#[test]
	fn parse_truncated_says_so() {
		assert!(is_truncated(&parse_request(b"E\n/no/trailing").unwrap_err()));
		assert!(is_truncated(&parse_request(b"").unwrap_err()));
	}

	#[test]
	fn parse_unknown_tag_errors() {
		let err = parse_request(b"X\n").unwrap_err();
		assert!(matches!(err, ParseError::UnknownTag { tag: 'X' }));
	}

	#[test]
	fn reply_round_trips() {
		assert_eq!(parse_reply(&encode_reply(Reply::Ok)).unwrap().0, Reply::Ok);
		assert_eq!(parse_reply(&encode_reply(Reply::Cancel)).unwrap().0, Reply::Cancel);
	}

	#[test]
	fn parse_rpc_request_round_trips() {
		let json = r#"{"method":"coder_status","params":{}}"#.to_string();
		let bytes = encode_request(&Request::Rpc { json: json.clone() }).unwrap();
		let (req, n) = parse_request(&bytes).unwrap();
		assert_eq!(req, Request::Rpc { json });
		assert_eq!(n, bytes.len());
	}

	#[test]
	fn parse_subscribe_request_round_trips() {
		let json = r#"{"method":"coder_subscribe","params":{}}"#.to_string();
		let bytes = encode_request(&Request::Subscribe { json: json.clone() }).unwrap();
		let (req, n) = parse_request(&bytes).unwrap();
		assert_eq!(req, Request::Subscribe { json });
		assert_eq!(n, bytes.len());
	}

	#[test]
	fn encode_rpc_refuses_newline_in_body() {
		let err = encode_request(&Request::Rpc {
			json: "{\n}".to_string(),
		})
		.unwrap_err();
		assert!(matches!(err, EncodeError::NewlineInPath | EncodeError::NewlineInBody));
		assert!(matches!(
			encode_request(&Request::Rpc {
				json: "{\n}".to_string()
			})
			.unwrap_err(),
			EncodeError::NewlineInBody
		));
	}

	#[test]
	fn rpc_response_round_trips_ok_and_error() {
		let ok = RpcResponse::ok(serde_json::json!({ "signed_in": true }));
		let (parsed, n) = parse_rpc_response(&encode_rpc_response(&ok)).unwrap();
		assert_eq!(parsed, ok);
		assert_eq!(n, encode_rpc_response(&ok).len());

		let err = RpcResponse::error("not signed in");
		let (parsed, _) = parse_rpc_response(&encode_rpc_response(&err)).unwrap();
		assert_eq!(parsed, err);
		assert_eq!(parsed.error.as_deref(), Some("not signed in"));
		assert!(parsed.ok.is_none());
	}

	#[test]
	fn rpc_response_malformed_is_hard_fail_not_truncated() {
		let err = parse_rpc_response(b"not json at all\n").unwrap_err();
		assert!(matches!(err, ParseError::BadRpcJson));
		assert!(!is_truncated(&err));
	}
}
