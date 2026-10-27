//! Opportunistic gzip of large LLM request bodies.
//!
//! Every turn re-uploads the whole conversation, so request bodies dominate
//! our upstream bytes. No provider we target documents request-side
//! `Content-Encoding`, so support is learned per origin by trying:
//!
//! - RFC 7694: a server that sends `Accept-Encoding` on a response tells us
//!   outright which request codings it takes, and answers an unsupported one
//!   with 415.
//! - Most servers don't implement the RFC; they ignore `Content-Encoding`,
//!   fail to parse the gzip bytes as JSON, and answer 400 / 422. Those
//!   statuses trigger one uncompressed resend; a different outcome marks the
//!   origin unsupported. A 4xx means the request never reached the model, so
//!   the resend can't double-bill.
//!
//! The verdict is cached in memory for the process lifetime, so a
//! non-supporting origin costs at most one extra round trip per run.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{LazyLock, Mutex};

use reqwest::header::{HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH};
use reqwest::StatusCode;

/// Below this, gzip saves too little to be worth a failed probe.
const MIN_BODY_BYTES: usize = 16_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Support {
	Unknown,
	Supported,
	Unsupported,
}

static SUPPORT: LazyLock<Mutex<HashMap<String, Support>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn support_of(origin: &str) -> Support {
	let map = SUPPORT.lock().unwrap_or_else(|e| e.into_inner());
	map.get(origin).copied().unwrap_or(Support::Unknown)
}

fn set_support(origin: &str, support: Support) {
	let mut map = SUPPORT.lock().unwrap_or_else(|e| e.into_inner());
	let previous = map.insert(origin.to_owned(), support);
	if previous != Some(support) {
		tracing::info!(origin, ?support, "learned request gzip support");
	}
}

/// RFC 7694 hint: `Some(true)` when the response's `Accept-Encoding`
/// accepts gzip, `Some(false)` when the header is present but doesn't,
/// `None` when the server said nothing.
fn hinted_gzip(response: &reqwest::Response) -> Option<bool> {
	let raw = response.headers().get(ACCEPT_ENCODING)?.to_str().ok()?;
	Some(raw.split(',').any(|entry| {
		let mut parts = entry.split(';');
		let coding = parts.next().unwrap_or_default().trim();
		let refused = parts.any(|p| {
			let p = p.trim();
			p.strip_prefix("q=")
				.and_then(|q| q.trim().parse::<f32>().ok())
				.is_some_and(|q| q == 0.0)
		});
		!refused && (coding.eq_ignore_ascii_case("gzip") || coding == "*")
	}))
}

/// Statuses that may mean "couldn't read your encoded body". Checked even
/// for origins that already took gzip: a proxy can decode for itself yet
/// forward `Content-Encoding` to one backend that then chokes (the HF
/// router did exactly this for deepinfra while groq worked).
fn may_reject_encoding(status: StatusCode) -> bool {
	status == StatusCode::UNSUPPORTED_MEDIA_TYPE
		|| status == StatusCode::BAD_REQUEST
		|| status == StatusCode::UNPROCESSABLE_ENTITY
}

fn gzip(bytes: &[u8]) -> Option<Vec<u8>> {
	let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 4), flate2::Compression::default());
	encoder.write_all(bytes).ok()?;
	encoder.finish().ok()
}

/// Gzipped copy of `request`, or `None` when it isn't worth compressing
/// (small, already encoded, streaming body, origin known not to support it).
fn compressed(request: &reqwest::Request, origin: &str) -> Option<reqwest::Request> {
	if support_of(origin) == Support::Unsupported || request.headers().contains_key(CONTENT_ENCODING) {
		return None;
	}
	let body = request.body()?.as_bytes()?;
	if body.len() < MIN_BODY_BYTES {
		return None;
	}
	let encoded = gzip(body)?;
	let mut out = request.try_clone()?;
	out
		.headers_mut()
		.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
	out.headers_mut().remove(CONTENT_LENGTH);
	*out.body_mut() = Some(encoded.into());
	Some(out)
}

/// Send `request`, gzipping the body when the origin supports (or might
/// support) it, and falling back to the plain body when it turns out not to.
pub(crate) async fn execute(client: &reqwest::Client, request: reqwest::Request) -> reqwest::Result<reqwest::Response> {
	let origin = request.url().origin().ascii_serialization();
	let Some(encoded) = compressed(&request, &origin) else {
		return client.execute(request).await;
	};
	let response = client.execute(encoded).await?;
	let status = response.status();
	if status.is_success() {
		set_support(&origin, Support::Supported);
		return Ok(response);
	}
	let hint = hinted_gzip(&response);
	let rejected = match hint {
		Some(true) => {
			set_support(&origin, Support::Supported);
			false
		}
		Some(false) => {
			set_support(&origin, Support::Unsupported);
			true
		}
		None => may_reject_encoding(status),
	};
	if !rejected {
		return Ok(response);
	}
	let retry = client.execute(request).await?;
	if hint.is_none() && retry.status() != status {
		set_support(&origin, Support::Unsupported);
	}
	Ok(retry)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Arc;
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	/// Seen by the fake server: whether the request body was gzipped.
	type Log = Arc<Mutex<Vec<bool>>>;

	/// One-request-per-connection HTTP/1.1 server. `reply(gzipped, path)`
	/// picks the status line and extra headers for each request.
	async fn serve(reply: fn(bool, &str) -> (&'static str, &'static str)) -> (String, Log) {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		let log: Log = Arc::default();
		let seen = log.clone();
		tokio::spawn(async move {
			loop {
				let Ok((mut sock, _)) = listener.accept().await else {
					return;
				};
				let mut buf = Vec::new();
				let mut chunk = [0u8; 65536];
				let (head_end, content_length) = loop {
					let n = sock.read(&mut chunk).await.unwrap();
					buf.extend_from_slice(&chunk[..n]);
					if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
						let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
						let len = head
							.lines()
							.find_map(|l| l.strip_prefix("content-length:"))
							.map_or(0, |v| v.trim().parse::<usize>().unwrap());
						break (pos + 4, len);
					}
				};
				while buf.len() < head_end + content_length {
					let n = sock.read(&mut chunk).await.unwrap();
					buf.extend_from_slice(&chunk[..n]);
				}
				let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
				let gzipped = head.contains("content-encoding: gzip");
				seen.lock().unwrap().push(gzipped);
				let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
				let (status, extra) = reply(gzipped, &path);
				let resp = format!("HTTP/1.1 {status}\r\n{extra}content-length: 2\r\nconnection: close\r\n\r\n{{}}");
				sock.write_all(resp.as_bytes()).await.unwrap();
			}
		});
		(base, log)
	}

	fn big_request(client: &reqwest::Client, base: &str) -> reqwest::Request {
		big_request_to(client, base, "/v1/chat")
	}

	fn big_request_to(client: &reqwest::Client, base: &str, path: &str) -> reqwest::Request {
		let body = serde_json::json!({ "pad": "x".repeat(MIN_BODY_BYTES) });
		client.post(format!("{base}{path}")).json(&body).build().unwrap()
	}

	#[tokio::test]
	async fn supporting_origin_keeps_getting_gzip() {
		let (base, log) = serve(|_, _| ("200 OK", "")).await;
		let client = reqwest::Client::new();
		for _ in 0..2 {
			let resp = execute(&client, big_request(&client, &base)).await.unwrap();
			assert_eq!(resp.status(), 200);
		}
		assert_eq!(*log.lock().unwrap(), vec![true, true]);
	}

	#[tokio::test]
	async fn parse_failure_falls_back_and_is_remembered() {
		let (base, log) = serve(|gz, _| if gz { ("400 Bad Request", "") } else { ("200 OK", "") }).await;
		let client = reqwest::Client::new();
		for _ in 0..2 {
			let resp = execute(&client, big_request(&client, &base)).await.unwrap();
			assert_eq!(resp.status(), 200);
		}
		assert_eq!(*log.lock().unwrap(), vec![true, false, false]);
	}

	#[tokio::test]
	async fn genuine_400_is_not_blamed_on_gzip() {
		let (base, log) = serve(|_, _| ("400 Bad Request", "")).await;
		let client = reqwest::Client::new();
		let resp = execute(&client, big_request(&client, &base)).await.unwrap();
		assert_eq!(resp.status(), 400);
		let origin = reqwest::Url::parse(&base).unwrap().origin().ascii_serialization();
		assert_eq!(support_of(&origin), Support::Unknown);
		assert_eq!(*log.lock().unwrap(), vec![true, false]);
	}

	#[tokio::test]
	async fn rfc7694_415_hint_is_honoured() {
		let (base, log) = serve(|gz, _| {
			if gz {
				("415 Unsupported Media Type", "accept-encoding: identity\r\n")
			} else {
				("200 OK", "")
			}
		})
		.await;
		let client = reqwest::Client::new();
		for _ in 0..2 {
			execute(&client, big_request(&client, &base)).await.unwrap();
		}
		assert_eq!(*log.lock().unwrap(), vec![true, false, false]);
	}

	#[tokio::test]
	async fn backend_breaking_on_supported_origin_still_falls_back() {
		let (base, log) = serve(|gz, path| {
			if gz && path == "/broken" {
				("400 Bad Request", "")
			} else {
				("200 OK", "")
			}
		})
		.await;
		let client = reqwest::Client::new();
		execute(&client, big_request_to(&client, &base, "/ok")).await.unwrap();
		let resp = execute(&client, big_request_to(&client, &base, "/broken"))
			.await
			.unwrap();
		assert_eq!(resp.status(), 200);
		execute(&client, big_request_to(&client, &base, "/ok")).await.unwrap();
		assert_eq!(*log.lock().unwrap(), vec![true, true, false, false]);
	}

	#[tokio::test]
	async fn small_bodies_are_sent_plain() {
		let (base, log) = serve(|_, _| ("200 OK", "")).await;
		let client = reqwest::Client::new();
		let req = client
			.post(format!("{base}/v1/chat"))
			.json(&serde_json::json!({}))
			.build()
			.unwrap();
		execute(&client, req).await.unwrap();
		assert_eq!(*log.lock().unwrap(), vec![false]);
	}
}
