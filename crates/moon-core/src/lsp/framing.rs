//! LSP JSON-RPC framing.
//!
//! LSP wraps every JSON-RPC message in a tiny header block:
//!
//! ```text
//! Content-Length: <N>\r\n
//! [Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n]
//! \r\n
//! <N bytes of UTF-8 JSON>
//! ```
//!
//! We parse both sides (the `Content-Type` header is optional and we
//! ignore its payload). No other LSP framing lives here — higher layers
//! deal with ids, methods, and message routing.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Read one framed LSP message from `reader`. Returns the payload
/// bytes (without the header block). EOF before the header is fully
/// read is reported as `UnexpectedEof`.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<Vec<u8>> {
	let mut content_length: Option<usize> = None;
	let mut header_line = String::new();

	loop {
		header_line.clear();
		let n = reader.read_line(&mut header_line).await?;
		if n == 0 {
			return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof during header"));
		}
		// `read_line` leaves `\n` in place; strip it along with any
		// stray `\r` so both real servers (CRLF) and test fixtures
		// (LF-only) parse.
		let trimmed = header_line.trim_end_matches(['\r', '\n']);
		if trimmed.is_empty() {
			// End of header block. Must have seen a Content-Length.
			break;
		}
		if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
			let value: usize = rest
				.trim()
				.parse()
				.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad Content-Length: {e}")))?;
			content_length = Some(value);
		}
		// Anything else (Content-Type, vendor extensions) is
		// intentionally ignored.
	}

	let len = content_length.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
	let mut buf = vec![0u8; len];
	tokio::io::AsyncReadExt::read_exact(reader, &mut buf).await?;
	Ok(buf)
}

/// Write one framed LSP message. `payload` is the JSON body.
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
	let header = format!("Content-Length: {}\r\n\r\n", payload.len());
	writer.write_all(header.as_bytes()).await?;
	writer.write_all(payload).await?;
	writer.flush().await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::BufReader;

	#[tokio::test]
	async fn roundtrip_single_message() {
		let mut buf: Vec<u8> = Vec::new();
		write_message(&mut buf, br#"{"jsonrpc":"2.0","method":"ping"}"#)
			.await
			.unwrap();
		let mut reader = BufReader::new(&buf[..]);
		let msg = read_message(&mut reader).await.unwrap();
		assert_eq!(msg, br#"{"jsonrpc":"2.0","method":"ping"}"#);
	}

	#[tokio::test]
	async fn reads_back_to_back_messages() {
		let mut buf: Vec<u8> = Vec::new();
		write_message(&mut buf, b"{}").await.unwrap();
		write_message(&mut buf, br#"{"x":1}"#).await.unwrap();
		let mut reader = BufReader::new(&buf[..]);
		assert_eq!(read_message(&mut reader).await.unwrap(), b"{}");
		assert_eq!(read_message(&mut reader).await.unwrap(), br#"{"x":1}"#);
	}

	#[tokio::test]
	async fn ignores_content_type_header() {
		let input = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}";
		let mut reader = BufReader::new(&input[..]);
		assert_eq!(read_message(&mut reader).await.unwrap(), b"{}");
	}

	#[tokio::test]
	async fn tolerates_lf_only_line_endings() {
		// Some test fixtures (and buggy servers) ship LF without
		// the CR. The spec says CRLF; we accept both because the
		// cost is one char-class check and the upside is not
		// chasing a ghost bug in a test harness later.
		let input = b"Content-Length: 2\n\n{}";
		let mut reader = BufReader::new(&input[..]);
		assert_eq!(read_message(&mut reader).await.unwrap(), b"{}");
	}

	#[tokio::test]
	async fn eof_before_header_is_unexpected() {
		let input: &[u8] = b"";
		let mut reader = BufReader::new(input);
		let err = read_message(&mut reader).await.unwrap_err();
		assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
	}
}
