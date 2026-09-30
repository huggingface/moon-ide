//! Host-loopback preview proxies for the IDE's browser tabs
//! (ADR 0088).
//!
//! Each `(side, host, port)` target gets one HTTP/1 proxy listening on
//! `127.0.0.1:<ephemeral>`. Upstream connections are dialed either
//! directly (host targets) or with `docker exec -i <container> ncat
//! <host> <port>` (container targets) — the dial happens *inside* the
//! container, so a dev server bound to the container's own loopback is
//! reachable and compose service names resolve. No sidecar, no
//! published port, nothing survives the IDE process.
//!
//! The proxy is HTTP-aware for one reason: HTML responses get the page
//! bridge (`preview_bridge.js`) injected, which is what lets an agent
//! read and drive the page through the IDE's iframe. Frame-blocking
//! and CSP headers are dropped on the way so the page renders in the
//! frame and the inline bridge runs. Everything else — WebSocket
//! upgrades (HMR), SSE, binary assets — passes through untouched.
//!
//! Distinct from [`crate::port_forward`]: those forwards are
//! user-declared, persisted, and stable on the host; these are
//! ephemeral, per-process, and only exist because a browser tab
//! asked for a URL.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// In-page bridge injected into every proxied HTML document.
const BRIDGE_JS: &str = include_str!("preview_bridge.js");

/// Which side of the container boundary a preview URL means.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreviewTarget {
	Host,
	/// Dial from inside this container.
	Container(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Dial {
	target: PreviewTarget,
	host: String,
	port: u16,
}

struct Proxy {
	local_port: u16,
	accept_loop: JoinHandle<()>,
}

/// Registry of live proxies. One per IDE process; proxies are reused
/// across tabs so a target keeps a stable origin (cookies,
/// localStorage) for the lifetime of the process.
#[derive(Default)]
pub struct PreviewTunnels {
	proxies: Mutex<HashMap<Dial, Proxy>>,
}

impl PreviewTunnels {
	pub fn new() -> Self {
		Self::default()
	}

	/// Host-side loopback port whose proxy reaches `host:port` as
	/// seen from `target`. Creates the listener on first use.
	pub async fn ensure(&self, target: PreviewTarget, host: &str, port: u16) -> io::Result<u16> {
		let dial = Dial {
			target,
			host: host.to_owned(),
			port,
		};
		let mut proxies = self.proxies.lock().await;
		if let Some(existing) = proxies.get(&dial) {
			if !existing.accept_loop.is_finished() {
				return Ok(existing.local_port);
			}
		}
		let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
		let local_port = listener.local_addr()?.port();
		let accept_loop = tokio::spawn(accept_loop(listener, Arc::new(dial.clone())));
		proxies.insert(
			dial,
			Proxy {
				local_port,
				accept_loop,
			},
		);
		Ok(local_port)
	}
}

impl Drop for PreviewTunnels {
	fn drop(&mut self) {
		for proxy in self.proxies.get_mut().values() {
			proxy.accept_loop.abort();
		}
	}
}

async fn accept_loop(listener: TcpListener, dial: Arc<Dial>) {
	loop {
		let stream = match listener.accept().await {
			Ok((stream, _)) => stream,
			Err(err) => {
				tracing::warn!(%err, "preview proxy: accept failed, closing listener");
				return;
			}
		};
		let dial = dial.clone();
		tokio::spawn(async move {
			// One upstream connection per downstream connection,
			// reused across its keep-alive requests: a container dial
			// is a `docker exec`, far too slow to pay per request.
			let upstream: Arc<Mutex<Option<SendRequest<Incoming>>>> = Arc::new(Mutex::new(None));
			let service = hyper::service::service_fn(move |req| {
				let dial = dial.clone();
				let upstream = upstream.clone();
				async move { Ok::<_, hyper::Error>(proxy_request(req, &dial, &upstream).await) }
			});
			let served = hyper::server::conn::http1::Builder::new()
				.serve_connection(TokioIo::new(stream), service)
				.with_upgrades()
				.await;
			if let Err(err) = served {
				tracing::debug!(%err, "preview proxy: downstream connection ended with error");
			}
		});
	}
}

type ProxyBody = BoxBody<Bytes, hyper::Error>;

async fn proxy_request(
	req: Request<Incoming>,
	dial: &Dial,
	upstream: &Mutex<Option<SendRequest<Incoming>>>,
) -> Response<ProxyBody> {
	let result = if req.headers().contains_key(header::UPGRADE) {
		proxy_upgrade(req, dial).await
	} else {
		proxy_plain(req, dial, upstream).await
	};
	result.unwrap_or_else(|err| {
		let body = format!(
			"moon-ide preview proxy: could not reach {}:{} ({}): {err}",
			dial.host,
			dial.port,
			match &dial.target {
				PreviewTarget::Host => "host".to_owned(),
				PreviewTarget::Container(name) => format!("inside {name}"),
			}
		);
		let mut resp = Response::new(full(Bytes::from(body)));
		*resp.status_mut() = StatusCode::BAD_GATEWAY;
		resp
	})
}

async fn proxy_plain(
	mut req: Request<Incoming>,
	dial: &Dial,
	upstream: &Mutex<Option<SendRequest<Incoming>>>,
) -> io::Result<Response<ProxyBody>> {
	// Identity bodies so HTML can be rewritten without a decoder;
	// it's loopback, bandwidth is free.
	req.headers_mut().remove(header::ACCEPT_ENCODING);
	let mut slot = upstream.lock().await;
	let reusable = match slot.as_mut() {
		Some(sender) => sender.ready().await.is_ok(),
		None => false,
	};
	if !reusable {
		let io = connect(dial).await?;
		let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
			.await
			.map_err(io::Error::other)?;
		tokio::spawn(async move {
			if let Err(err) = conn.await {
				tracing::debug!(%err, "preview proxy: upstream connection ended with error");
			}
		});
		*slot = Some(sender);
	}
	let Some(sender) = slot.as_mut() else {
		return Err(io::Error::other("upstream sender missing"));
	};
	let resp = sender.send_request(req).await.map_err(io::Error::other)?;
	drop(slot);
	rewrite_response(resp).await
}

/// WebSocket (or any `Upgrade:`) request: fresh upstream connection,
/// forward the handshake, then splice the two upgraded streams.
async fn proxy_upgrade(mut req: Request<Incoming>, dial: &Dial) -> io::Result<Response<ProxyBody>> {
	let downstream_upgrade = hyper::upgrade::on(&mut req);
	let io = connect(dial).await?;
	let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
		.await
		.map_err(io::Error::other)?;
	tokio::spawn(async move {
		if let Err(err) = conn.with_upgrades().await {
			tracing::debug!(%err, "preview proxy: upgrade connection ended with error");
		}
	});
	let mut resp = sender.send_request(req).await.map_err(io::Error::other)?;
	if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
		return rewrite_response(resp).await;
	}
	let upstream_upgrade = hyper::upgrade::on(&mut resp);
	tokio::spawn(async move {
		let (Ok(down), Ok(up)) = tokio::join!(downstream_upgrade, upstream_upgrade) else {
			return;
		};
		let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(down), &mut TokioIo::new(up)).await;
	});
	let (parts, _) = resp.into_parts();
	Ok(Response::from_parts(parts, empty()))
}

async fn rewrite_response(resp: Response<Incoming>) -> io::Result<Response<ProxyBody>> {
	let (mut parts, body) = resp.into_parts();
	// The whole point is to render inside the IDE's frame.
	parts.headers.remove(header::X_FRAME_OPTIONS);
	let is_html = parts
		.headers
		.get(header::CONTENT_TYPE)
		.and_then(|v| v.to_str().ok())
		.is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"));
	let encoded = parts
		.headers
		.get(header::CONTENT_ENCODING)
		.is_some_and(|v| v.as_bytes() != b"identity");
	if !is_html || encoded {
		parts.headers.remove(header::CONTENT_SECURITY_POLICY);
		return Ok(Response::from_parts(parts, body.boxed()));
	}
	let bytes = body.collect().await.map_err(io::Error::other)?.to_bytes();
	let injected = inject_bridge(&bytes);
	// An inline script needs `unsafe-inline`; dropping the policy is
	// simpler than rewriting it, and this is a dev preview.
	parts.headers.remove(header::CONTENT_SECURITY_POLICY);
	parts.headers.remove("content-security-policy-report-only");
	parts.headers.remove(header::TRANSFER_ENCODING);
	parts
		.headers
		.insert(header::CONTENT_LENGTH, HeaderValue::from(injected.len()));
	Ok(Response::from_parts(parts, full(Bytes::from(injected))))
}

/// Insert the bridge `<script>` right after `<head …>` (else after
/// `<html …>`, else at the very start) so it runs before page code.
fn inject_bridge(html: &[u8]) -> Vec<u8> {
	let script = format!("<script>{BRIDGE_JS}</script>");
	let lower = html.to_ascii_lowercase();
	let at = find_tag_end(&lower, b"<head")
		.or_else(|| find_tag_end(&lower, b"<html"))
		.unwrap_or(0);
	let mut out = Vec::with_capacity(html.len() + script.len());
	out.extend_from_slice(&html[..at]);
	out.extend_from_slice(script.as_bytes());
	out.extend_from_slice(&html[at..]);
	out
}

/// Byte offset just past the `>` closing the first `tag` opening
/// (matching `<head>` / `<head lang=…>`, not `<header>`).
fn find_tag_end(lower: &[u8], tag: &[u8]) -> Option<usize> {
	let mut from = 0;
	while let Some(rel) = lower[from..].windows(tag.len()).position(|w| w == tag) {
		let start = from + rel;
		let after = start + tag.len();
		match lower.get(after) {
			Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/') => {
				return lower[after..].iter().position(|&b| b == b'>').map(|p| after + p + 1);
			}
			_ => from = after,
		}
	}
	None
}

fn full(bytes: Bytes) -> ProxyBody {
	Full::new(bytes).map_err(|never| match never {}).boxed()
}

fn empty() -> ProxyBody {
	Empty::new().map_err(|never| match never {}).boxed()
}

trait UpstreamIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> UpstreamIo for T {}

async fn connect(dial: &Dial) -> io::Result<Box<dyn UpstreamIo>> {
	match &dial.target {
		PreviewTarget::Host => Ok(Box::new(TcpStream::connect((dial.host.as_str(), dial.port)).await?)),
		PreviewTarget::Container(container) => {
			let mut child = Command::new("docker")
				.args(["exec", "-i", container, "ncat", &dial.host, &dial.port.to_string()])
				.stdin(Stdio::piped())
				.stdout(Stdio::piped())
				.stderr(Stdio::null())
				.kill_on_drop(true)
				.spawn()?;
			let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
				return Err(io::Error::other("docker exec has no stdio"));
			};
			Ok(Box::new(ChildIo {
				stdin,
				stdout,
				_child: child,
			}))
		}
	}
}

/// A dial process's stdio as one duplex stream. Dropping it kills the
/// process (`kill_on_drop`).
struct ChildIo {
	stdin: ChildStdin,
	stdout: ChildStdout,
	_child: Child,
}

impl AsyncRead for ChildIo {
	fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.stdout).poll_read(cx, buf)
	}
}

impl AsyncWrite for ChildIo {
	fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
		Pin::new(&mut self.stdin).poll_write(cx, buf)
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.stdin).poll_flush(cx)
	}

	fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Pin::new(&mut self.stdin).poll_shutdown(cx)
	}
}

#[cfg(test)]
mod tests {
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	use super::*;

	#[test]
	fn bridge_goes_after_head_not_header() {
		let out = String::from_utf8(inject_bridge(b"<html><header></header><HEAD lang=en><title>x</title>")).unwrap();
		let script_at = out.find("<script>").unwrap();
		assert_eq!(&out[..script_at], "<html><header></header><HEAD lang=en>");
		let bare = String::from_utf8(inject_bridge(b"hello")).unwrap();
		assert!(bare.starts_with("<script>") && bare.ends_with("</script>hello"));
	}

	#[tokio::test]
	async fn upgrades_are_spliced() {
		let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
		let upstream_port = upstream.local_addr().unwrap().port();
		tokio::spawn(async move {
			let (mut sock, _) = upstream.accept().await.unwrap();
			let mut buf = vec![0u8; 4096];
			let n = sock.read(&mut buf).await.unwrap();
			assert!(String::from_utf8_lossy(&buf[..n])
				.to_ascii_lowercase()
				.contains("upgrade: websocket"));
			sock
				.write_all(b"HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\n")
				.await
				.unwrap();
			let mut frame = [0u8; 4];
			sock.read_exact(&mut frame).await.unwrap();
			sock.write_all(&frame).await.unwrap();
		});

		let tunnels = PreviewTunnels::new();
		let local = tunnels
			.ensure(PreviewTarget::Host, "127.0.0.1", upstream_port)
			.await
			.unwrap();
		let mut client = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
		client
			.write_all(b"GET /hmr HTTP/1.1\r\nhost: x\r\nconnection: upgrade\r\nupgrade: websocket\r\n\r\n")
			.await
			.unwrap();
		let mut head = Vec::new();
		while !head.ends_with(b"\r\n\r\n") {
			let mut byte = [0u8; 1];
			client.read_exact(&mut byte).await.unwrap();
			head.push(byte[0]);
		}
		assert!(head.starts_with(b"HTTP/1.1 101"));
		client.write_all(b"ping").await.unwrap();
		let mut echoed = [0u8; 4];
		client.read_exact(&mut echoed).await.unwrap();
		assert_eq!(&echoed, b"ping");
	}

	#[tokio::test]
	async fn proxies_and_injects_html() {
		let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
		let upstream_port = upstream.local_addr().unwrap().port();
		tokio::spawn(async move {
			let (mut sock, _) = upstream.accept().await.unwrap();
			let mut buf = vec![0u8; 4096];
			let n = sock.read(&mut buf).await.unwrap();
			let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
			assert!(!request.contains("accept-encoding"));
			let body = "<html><head></head><body>hi</body></html>";
			let reply = format!(
				"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nx-frame-options: DENY\r\ncontent-length: {}\r\n\r\n{body}",
				body.len()
			);
			sock.write_all(reply.as_bytes()).await.unwrap();
		});

		let tunnels = PreviewTunnels::new();
		let local = tunnels
			.ensure(PreviewTarget::Host, "127.0.0.1", upstream_port)
			.await
			.unwrap();
		assert_eq!(
			tunnels
				.ensure(PreviewTarget::Host, "127.0.0.1", upstream_port)
				.await
				.unwrap(),
			local
		);

		let mut client = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
		client
			.write_all(b"GET / HTTP/1.1\r\nhost: x\r\naccept-encoding: gzip\r\nconnection: close\r\n\r\n")
			.await
			.unwrap();
		let mut response = String::new();
		client.read_to_string(&mut response).await.unwrap();
		let lower = response.to_ascii_lowercase();
		assert!(lower.starts_with("http/1.1 200"));
		assert!(!lower.contains("x-frame-options"));
		assert!(response.contains("<head><script>"));
		assert!(response.ends_with("<body>hi</body></html>"));
	}
}
