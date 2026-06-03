//! Minimal HTTP/1.1 front for the bridge's single TLS listener
//! (Phase 13.4). The same port serves two things:
//!
//! - The companion PWA's static assets (so a phone just opens
//!   `https://<bridge>/` and installs the app).
//! - The WebSocket endpoint the PWA then talks to (`serve.rs`).
//!
//! We read the request head ourselves and branch: a request carrying
//! `Upgrade: websocket` is completed as a WS handshake (compute the
//! accept key, write 101, hand the raw stream to tungstenite in
//! server role); anything else is a static GET served from the
//! `--web-root` directory.
//!
//! This is deliberately a tiny hand-rolled parser, not a web
//! framework: the surface is "GET a file or upgrade to WS", and
//! pulling in hyper/axum for that would be a lot of dependency for a
//! LAN tool serving one small SPA.

use std::path::{Component, Path, PathBuf};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;

/// Cap on the request head we'll buffer. A GET line + headers from a
/// browser is well under this; anything larger is malformed or
/// hostile and gets dropped.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// The parsed first request on a connection.
pub enum Incoming {
	/// A WebSocket upgrade. The 101 response has already been written;
	/// the caller wraps `stream` with `WebSocketStream::from_raw_socket`.
	WebSocket,
	/// A plain GET for `path` (already URL-decoded, leading slash
	/// kept). The response has not been written yet.
	Get { path: String },
}

/// Read and classify the first request on `stream`. For a WS upgrade,
/// writes the 101 handshake response before returning. For a GET,
/// returns the path for the caller to serve. Returns `None` if the
/// request is malformed or unsupported (the caller should close).
pub async fn read_request<S>(stream: &mut S) -> std::io::Result<Option<Incoming>>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	let mut buf = Vec::with_capacity(1024);
	let mut tmp = [0u8; 1024];
	loop {
		if buf.len() > MAX_HEAD_BYTES {
			return Ok(None);
		}
		let n = stream.read(&mut tmp).await?;
		if n == 0 {
			return Ok(None);
		}
		buf.extend_from_slice(&tmp[..n]);
		if find_head_end(&buf).is_some() {
			break;
		}
	}

	let head = match std::str::from_utf8(&buf) {
		Ok(s) => s,
		Err(_) => return Ok(None),
	};
	let mut lines = head.split("\r\n");
	let Some(request_line) = lines.next() else {
		return Ok(None);
	};
	let mut parts = request_line.split_whitespace();
	let method = parts.next().unwrap_or_default();
	let target = parts.next().unwrap_or_default();
	if method != "GET" {
		return Ok(None);
	}

	// Collect the headers we care about.
	let mut upgrade_ws = false;
	let mut ws_key: Option<String> = None;
	for line in lines {
		if line.is_empty() {
			break;
		}
		let Some((name, value)) = line.split_once(':') else {
			continue;
		};
		let name = name.trim().to_ascii_lowercase();
		let value = value.trim();
		match name.as_str() {
			"upgrade" if value.eq_ignore_ascii_case("websocket") => upgrade_ws = true,
			"sec-websocket-key" => ws_key = Some(value.to_owned()),
			_ => {}
		}
	}

	if upgrade_ws {
		let Some(key) = ws_key else {
			return Ok(None);
		};
		let accept = derive_accept_key(key.as_bytes());
		let response = format!(
			"HTTP/1.1 101 Switching Protocols\r\n\
			 Upgrade: websocket\r\n\
			 Connection: Upgrade\r\n\
			 Sec-WebSocket-Accept: {accept}\r\n\r\n"
		);
		stream.write_all(response.as_bytes()).await?;
		stream.flush().await?;
		return Ok(Some(Incoming::WebSocket));
	}

	// Strip the query string; we serve static files by path only.
	let path = target.split(['?', '#']).next().unwrap_or("/").to_owned();
	Ok(Some(Incoming::Get { path }))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
	buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Serve a static file from `web_root` for `request_path`. Falls back
/// to `index.html` for unknown paths (SPA routing). Writes the full
/// HTTP response to `stream`.
pub async fn serve_static<S>(stream: &mut S, web_root: &Path, request_path: &str) -> std::io::Result<()>
where
	S: AsyncWrite + Unpin,
{
	let rel = request_path.trim_start_matches('/');
	let rel = if rel.is_empty() { "index.html" } else { rel };

	let resolved = match safe_join(web_root, rel) {
		Some(p) if p.is_file() => p,
		// SPA fallback: unknown route -> index.html.
		_ => web_root.join("index.html"),
	};

	match tokio::fs::read(&resolved).await {
		Ok(bytes) => {
			let ctype = content_type(&resolved);
			let header = format!(
				"HTTP/1.1 200 OK\r\n\
				 Content-Type: {ctype}\r\n\
				 Content-Length: {}\r\n\
				 Cache-Control: no-cache\r\n\r\n",
				bytes.len()
			);
			stream.write_all(header.as_bytes()).await?;
			stream.write_all(&bytes).await?;
		}
		Err(_) => {
			let body = b"not found";
			let header = format!(
				"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
				body.len()
			);
			stream.write_all(header.as_bytes()).await?;
			stream.write_all(body).await?;
		}
	}
	stream.flush().await
}

/// Join `rel` onto `root`, refusing any path that escapes `root` via
/// `..` or an absolute component. Returns `None` on traversal.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
	let mut out = root.to_path_buf();
	for comp in Path::new(rel).components() {
		match comp {
			Component::Normal(c) => out.push(c),
			// Reject anything that could climb out of the web root.
			Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
			Component::CurDir => {}
		}
	}
	Some(out)
}

fn content_type(path: &Path) -> &'static str {
	match path.extension().and_then(|e| e.to_str()) {
		Some("html") => "text/html; charset=utf-8",
		Some("js") => "text/javascript; charset=utf-8",
		Some("css") => "text/css; charset=utf-8",
		Some("json") => "application/json; charset=utf-8",
		Some("webmanifest") => "application/manifest+json; charset=utf-8",
		Some("svg") => "image/svg+xml",
		Some("png") => "image/png",
		Some("ico") => "image/x-icon",
		Some("woff2") => "font/woff2",
		_ => "application/octet-stream",
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn safe_join_blocks_traversal() {
		let root = Path::new("/srv/web");
		assert!(safe_join(root, "../etc/passwd").is_none());
		assert!(safe_join(root, "/etc/passwd").is_none());
		assert_eq!(
			safe_join(root, "assets/app.js"),
			Some(PathBuf::from("/srv/web/assets/app.js"))
		);
		assert_eq!(
			safe_join(root, "./index.html"),
			Some(PathBuf::from("/srv/web/index.html"))
		);
	}

	#[test]
	fn content_type_maps_common_extensions() {
		assert_eq!(content_type(Path::new("x/index.html")), "text/html; charset=utf-8");
		assert_eq!(content_type(Path::new("x/app.js")), "text/javascript; charset=utf-8");
		assert_eq!(
			content_type(Path::new("x/manifest.webmanifest")),
			"application/manifest+json; charset=utf-8"
		);
	}
}
