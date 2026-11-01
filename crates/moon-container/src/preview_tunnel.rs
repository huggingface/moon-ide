//! Host-loopback preview proxies for the IDE's browser tabs
//! (ADR 0088).
//!
//! Each `(sides, host, port)` target gets one HTTP/1 proxy listening on
//! `127.0.0.1:<ephemeral>`. Upstream connections are dialed either
//! directly (host side) or with `docker exec -i <container> ncat
//! <host> <port>` (container side) — the dial happens *inside* the
//! container, so a dev server bound to the container's own loopback is
//! reachable and compose service names resolve. Sides are tried in
//! order until one connects, so a `localhost` URL works whichever side
//! the server actually runs on. No sidecar, no published port, nothing
//! survives the IDE process.
//!
//! The proxy is HTTP-aware for one reason: HTML responses get the page
//! bridge (`preview_bridge.js`) injected, which is what lets an agent
//! read and drive the page through the IDE's iframe. Frame-blocking
//! and CSP headers are dropped on the way so the page renders in the
//! frame and the inline bridge runs. Everything else — WebSocket
//! upgrades (HMR), SSE, binary assets — passes through untouched.
//!
//! `https` targets are dialed the same way with TLS on top, so the
//! iframe still loads plain `http://127.0.0.1` and gets the bridge
//! (ADR 0104). Loopback and in-container servers usually carry dev
//! certificates, so those aren't verified; public hosts are, against
//! the webpki roots.
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
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// In-page bridge injected into every proxied HTML document.
const BRIDGE_JS: &str = include_str!("preview_bridge.js");

/// Upper bound on one in-container dial (exec + TCP connect).
const CONTAINER_DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Which side of the container boundary a preview URL is dialed from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreviewTarget {
	Host,
	/// Dial from inside this container.
	Container(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Dial {
	/// Tried in order; the first that connects serves the connection.
	sides: Vec<PreviewTarget>,
	host: String,
	port: u16,
	/// Upstream speaks `https`.
	tls: bool,
}

impl Dial {
	/// `scheme://host[:port]` the upstream knows itself as.
	fn origin(&self) -> String {
		let (scheme, default_port) = if self.tls { ("https", 443) } else { ("http", 80) };
		let host = if self.host.contains(':') {
			format!("[{}]", self.host)
		} else {
			self.host.clone()
		};
		if self.port == default_port {
			format!("{scheme}://{host}")
		} else {
			format!("{scheme}://{host}:{}", self.port)
		}
	}
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
	/// seen from the first of `sides` that accepts the connection,
	/// over TLS when `tls`. Creates the listener on first use.
	pub async fn ensure(&self, sides: Vec<PreviewTarget>, host: &str, port: u16, tls: bool) -> io::Result<u16> {
		if sides.is_empty() {
			return Err(io::Error::other("preview proxy needs at least one side to dial from"));
		}
		let dial = Dial {
			sides,
			host: host.to_owned(),
			port,
			tls,
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
			"moon-ide preview proxy: could not reach {}:{} - {err}\n\nIs the server running and listening on that port?",
			dial.host, dial.port,
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
	address_upstream(&mut req, dial);
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
	rewrite_response(resp, dial).await
}

/// A TLS upstream is usually a real site (or a dev server with a
/// host check), so it gets its own name in `Host` and `Origin`, not
/// the proxy's `127.0.0.1:<port>`.
fn address_upstream<B>(req: &mut Request<B>, dial: &Dial) {
	if !dial.tls {
		return;
	}
	let origin = dial.origin();
	let authority = origin.trim_start_matches("https://").to_owned();
	if let Ok(value) = HeaderValue::from_str(&authority) {
		req.headers_mut().insert(header::HOST, value);
	}
	if req.headers().contains_key(header::ORIGIN) {
		if let Ok(value) = HeaderValue::from_str(&origin) {
			req.headers_mut().insert(header::ORIGIN, value);
		}
	}
}

/// WebSocket (or any `Upgrade:`) request: fresh upstream connection,
/// forward the handshake, then splice the two upgraded streams.
async fn proxy_upgrade(mut req: Request<Incoming>, dial: &Dial) -> io::Result<Response<ProxyBody>> {
	let downstream_upgrade = hyper::upgrade::on(&mut req);
	address_upstream(&mut req, dial);
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
		return rewrite_response(resp, dial).await;
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

async fn rewrite_response(resp: Response<Incoming>, dial: &Dial) -> io::Result<Response<ProxyBody>> {
	let (mut parts, body) = resp.into_parts();
	// The whole point is to render inside the IDE's frame.
	parts.headers.remove(header::X_FRAME_OPTIONS);
	rewrite_location(&mut parts.headers, dial);
	if dial.tls {
		strip_secure_cookies(&mut parts.headers);
	}
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

/// A redirect to the upstream's own absolute URL would leave the proxy
/// (and the bridge); make it path-relative.
fn rewrite_location(headers: &mut hyper::HeaderMap, dial: &Dial) {
	let Some(location) = headers.get(header::LOCATION).and_then(|v| v.to_str().ok()) else {
		return;
	};
	let origin = dial.origin();
	let Some(rest) = location
		.get(..origin.len())
		.filter(|prefix| prefix.eq_ignore_ascii_case(&origin))
		.map(|_| &location[origin.len()..])
	else {
		return;
	};
	if !(rest.is_empty() || rest.starts_with(['/', '?', '#'])) {
		return;
	}
	let relative = if rest.starts_with('/') {
		rest.to_owned()
	} else {
		format!("/{rest}")
	};
	if let Ok(value) = HeaderValue::from_str(&relative) {
		headers.insert(header::LOCATION, value);
	}
}

/// The frame is plain `http`, where a `Secure` cookie wouldn't stick.
fn strip_secure_cookies(headers: &mut hyper::HeaderMap) {
	let cookies: Vec<HeaderValue> = headers
		.get_all(header::SET_COOKIE)
		.iter()
		.filter_map(|value| {
			let text = value.to_str().ok()?;
			let kept: Vec<&str> = text
				.split(';')
				.filter(|attr| !attr.trim().eq_ignore_ascii_case("secure"))
				.collect();
			HeaderValue::from_str(&kept.join(";")).ok()
		})
		.collect();
	if cookies.is_empty() {
		return;
	}
	headers.remove(header::SET_COOKIE);
	for cookie in cookies {
		headers.append(header::SET_COOKIE, cookie);
	}
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

/// First side of `dial` that connects; the error lists why each failed.
async fn connect(dial: &Dial) -> io::Result<Box<dyn UpstreamIo>> {
	let mut failures = Vec::with_capacity(dial.sides.len());
	for side in &dial.sides {
		let attempt = match side {
			PreviewTarget::Host => TcpStream::connect((dial.host.as_str(), dial.port))
				.await
				.map(|stream| Box::new(stream) as Box<dyn UpstreamIo>),
			PreviewTarget::Container(container) => connect_in_container(container, &dial.host, dial.port)
				.await
				.map(|io| Box::new(io) as Box<dyn UpstreamIo>),
		};
		let attempt = match attempt {
			Ok(io) if dial.tls => {
				let verify = matches!(side, PreviewTarget::Host) && !is_local_host(&dial.host);
				tls_wrap(io, &dial.host, verify).await
			}
			other => other,
		};
		match attempt {
			Ok(io) => return Ok(io),
			Err(err) => failures.push(match side {
				PreviewTarget::Host => format!("on the host: {err}"),
				PreviewTarget::Container(name) => format!("inside {name}: {err}"),
			}),
		}
	}
	Err(io::Error::other(failures.join("; ")))
}

fn is_local_host(host: &str) -> bool {
	host.eq_ignore_ascii_case("localhost")
		|| host.to_ascii_lowercase().ends_with(".localhost")
		|| host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn tls_wrap(io: Box<dyn UpstreamIo>, host: &str, verify: bool) -> io::Result<Box<dyn UpstreamIo>> {
	let name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(io::Error::other)?;
	let connector = tokio_rustls::TlsConnector::from(tls_config(verify)?);
	let stream = connector
		.connect(name, io)
		.await
		.map_err(|err| io::Error::other(format!("TLS handshake failed: {err}")))?;
	Ok(Box::new(stream))
}

fn tls_config(verify: bool) -> io::Result<Arc<rustls::ClientConfig>> {
	let provider = Arc::new(rustls::crypto::ring::default_provider());
	let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
		.with_safe_default_protocol_versions()
		.map_err(io::Error::other)?;
	let config = if verify {
		let roots = rustls::RootCertStore {
			roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
		};
		builder.with_root_certificates(roots).with_no_client_auth()
	} else {
		builder
			.dangerous()
			.with_custom_certificate_verifier(Arc::new(AcceptDevCert(provider)))
			.with_no_client_auth()
	};
	Ok(Arc::new(config))
}

/// Accepts any certificate (still checking the handshake signatures):
/// dev servers on loopback or in the container run self-signed or
/// locally-minted certs no root store knows.
#[derive(Debug)]
struct AcceptDevCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptDevCert {
	fn verify_server_cert(
		&self,
		_end_entity: &rustls::pki_types::CertificateDer<'_>,
		_intermediates: &[rustls::pki_types::CertificateDer<'_>],
		_server_name: &rustls::pki_types::ServerName<'_>,
		_ocsp_response: &[u8],
		_now: rustls::pki_types::UnixTime,
	) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
		Ok(rustls::client::danger::ServerCertVerified::assertion())
	}

	fn verify_tls12_signature(
		&self,
		message: &[u8],
		cert: &rustls::pki_types::CertificateDer<'_>,
		dss: &rustls::DigitallySignedStruct,
	) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
	}

	fn verify_tls13_signature(
		&self,
		message: &[u8],
		cert: &rustls::pki_types::CertificateDer<'_>,
		dss: &rustls::DigitallySignedStruct,
	) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
		rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
	}

	fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
		self.0.signature_verification_algorithms.supported_schemes()
	}
}

/// `docker exec` an `ncat -v` dial and wait for it to report the TCP
/// connection, so a refused dial fails here (with ncat's reason)
/// instead of surfacing later as an opaque closed upstream.
async fn connect_in_container(container: &str, host: &str, port: u16) -> io::Result<ChildIo> {
	let mut child = Command::new("docker")
		.args(["exec", "-i", container, "ncat", "-v", host, &port.to_string()])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true)
		.spawn()?;
	let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
		return Err(io::Error::other("docker exec has no stdio"));
	};
	let mut lines = BufReader::new(stderr).lines();
	let mut reasons = Vec::new();
	let wait = async {
		while let Some(line) = lines.next_line().await? {
			if line.contains("Connected to") {
				return Ok(true);
			}
			reasons.push(line);
		}
		Ok::<_, io::Error>(false)
	};
	match tokio::time::timeout(CONTAINER_DIAL_TIMEOUT, wait).await {
		Ok(Ok(true)) => {}
		Ok(Ok(false)) => return Err(io::Error::other(describe_dial_failure(&reasons))),
		Ok(Err(err)) => return Err(err),
		Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out connecting")),
	}
	// Keep draining so ncat never blocks on a full stderr pipe; ends
	// when the process does.
	tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
	Ok(ChildIo {
		stdin,
		stdout,
		_child: child,
	})
}

/// Condense a failed dial's stderr (ncat or docker) into one reason.
fn describe_dial_failure(lines: &[String]) -> String {
	let all = lines.join("\n");
	if all.contains("No such container") || all.contains("is not running") {
		return "workspace container is not running".to_owned();
	}
	if all.contains("executable file not found") {
		return "`ncat` is not installed in the container".to_owned();
	}
	let reasons: Vec<&str> = lines
		.iter()
		.map(|line| line.trim())
		.filter(|line| {
			!line.is_empty()
				&& !line.starts_with("Ncat: Version")
				&& !line.contains("Trying next address")
				&& !line.contains("failed: Connection refused")
		})
		.map(|line| line.strip_prefix("Ncat: ").unwrap_or(line))
		.collect();
	if reasons.is_empty() {
		return "dial exited without connecting".to_owned();
	}
	reasons.join("; ")
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

	#[test]
	fn tls_dials_address_and_redirect_as_the_upstream() {
		let dial = Dial {
			sides: vec![PreviewTarget::Host],
			host: "example.com".into(),
			port: 443,
			tls: true,
		};
		assert_eq!(dial.origin(), "https://example.com");
		let mut req = Request::builder()
			.uri("/x")
			.header(header::HOST, "127.0.0.1:4000")
			.header(header::ORIGIN, "http://127.0.0.1:4000")
			.body(())
			.unwrap();
		address_upstream(&mut req, &dial);
		assert_eq!(req.headers()[header::HOST], "example.com");
		assert_eq!(req.headers()[header::ORIGIN], "https://example.com");

		let mut headers = hyper::HeaderMap::new();
		headers.insert(
			header::LOCATION,
			HeaderValue::from_static("https://EXAMPLE.com/login?next=1"),
		);
		headers.append(
			header::SET_COOKIE,
			HeaderValue::from_static("a=1; Path=/; Secure; HttpOnly"),
		);
		rewrite_location(&mut headers, &dial);
		strip_secure_cookies(&mut headers);
		assert_eq!(headers[header::LOCATION], "/login?next=1");
		assert_eq!(headers[header::SET_COOKIE], "a=1; Path=/; HttpOnly");

		// Other origins (and look-alike hosts) are left alone.
		headers.insert(header::LOCATION, HeaderValue::from_static("https://example.com.evil/x"));
		rewrite_location(&mut headers, &dial);
		assert_eq!(headers[header::LOCATION], "https://example.com.evil/x");
	}

	#[test]
	fn dial_failures_are_condensed() {
		let refused = [
			"Ncat: Version 7.93 ( https://nmap.org/ncat )",
			"Ncat: Connection to ::1 failed: Connection refused.",
			"Ncat: Trying next address...",
			"Ncat: Connection refused.",
		]
		.map(String::from);
		assert_eq!(describe_dial_failure(&refused), "Connection refused.");
		let gone = ["Error response from daemon: No such container: moon-ws-x-dev-1".to_owned()];
		assert_eq!(describe_dial_failure(&gone), "workspace container is not running");
		assert_eq!(describe_dial_failure(&[]), "dial exited without connecting");
	}

	#[tokio::test]
	async fn later_sides_are_tried_and_failures_reported() {
		let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
		let upstream_port = upstream.local_addr().unwrap().port();
		drop(upstream);
		let dial = Dial {
			sides: vec![PreviewTarget::Host, PreviewTarget::Host],
			host: "127.0.0.1".to_owned(),
			port: upstream_port,
			tls: false,
		};
		let Err(err) = connect(&dial).await else {
			panic!("nothing listens on the port");
		};
		assert_eq!(err.to_string().matches("on the host:").count(), 2);
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
			.ensure(vec![PreviewTarget::Host], "127.0.0.1", upstream_port, false)
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
			.ensure(vec![PreviewTarget::Host], "127.0.0.1", upstream_port, false)
			.await
			.unwrap();
		assert_eq!(
			tunnels
				.ensure(vec![PreviewTarget::Host], "127.0.0.1", upstream_port, false)
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
