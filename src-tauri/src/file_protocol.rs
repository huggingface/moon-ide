//! `moonfile://` — serves workspace file bytes to a remote window's
//! webview (ADR 0095). The asset protocol reads this desktop's disk;
//! for a remote workspace the bytes live on the server, so this scheme
//! pages them through `fs_read_range` over the attach channel. Range
//! requests are honoured so video can seek.

use serde::Deserialize;
use tauri::http::{header, Request, Response, StatusCode};
use tauri::{AppHandle, Manager};

use crate::state::{AppState, Backend};

pub const SCHEME: &str = "moonfile";

/// Whole-file responses stop here; video and other big files are
/// expected to come in with `Range`.
const WHOLE_FILE_MAX_BYTES: u64 = 200_000_000;

#[derive(Deserialize)]
struct FileRange {
	data: String,
	offset: u64,
	total: u64,
	mime: Option<String>,
}

pub async fn handle(app: AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
	match serve(&app, &request).await {
		Ok(response) => response,
		Err((status, message)) => Response::builder()
			.status(status)
			.body(message.into_bytes())
			.unwrap_or_default(),
	}
}

type Failure = (StatusCode, String);

async fn serve(app: &AppHandle, request: &Request<Vec<u8>>) -> Result<Response<Vec<u8>>, Failure> {
	let path = percent_decode(request.uri().path().trim_start_matches('/'))
		.ok_or_else(|| (StatusCode::BAD_REQUEST, "bad path encoding".to_owned()))?;
	let path = format!("/{}", path.trim_start_matches('/'));
	let range = request
		.headers()
		.get(header::RANGE)
		.and_then(|v| v.to_str().ok())
		.and_then(parse_range);

	let state = app.state::<AppState>();
	let Backend::Remote(remote) = &state.backend else {
		return Err((
			StatusCode::NOT_FOUND,
			"moonfile:// serves remote workspaces only".to_owned(),
		));
	};
	let read = |offset: u64, length: u64| {
		let path = path.clone();
		async move {
			let value = remote
				.client
				.call(
					"fs_read_range",
					serde_json::json!({ "path": path, "offset": offset, "length": length }),
				)
				.await
				.map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
			let range: FileRange = serde_json::from_value(value).map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
			let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, range.data)
				.map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
			Ok::<_, Failure>((bytes, range.offset, range.total, range.mime))
		}
	};

	if let Some((start, end)) = range {
		let length = end.map_or(u64::MAX, |end| end.saturating_sub(start) + 1);
		let (bytes, offset, total, mime) = read(start, length).await?;
		if offset >= total && total > 0 {
			return Err((StatusCode::RANGE_NOT_SATISFIABLE, "range past end of file".to_owned()));
		}
		let last = offset + (bytes.len() as u64).saturating_sub(1);
		return Ok(
			Response::builder()
				.status(StatusCode::PARTIAL_CONTENT)
				.header(
					header::CONTENT_TYPE,
					mime.unwrap_or_else(|| "application/octet-stream".to_owned()),
				)
				.header(header::ACCEPT_RANGES, "bytes")
				.header(header::CONTENT_RANGE, format!("bytes {offset}-{last}/{total}"))
				.body(bytes)
				.unwrap_or_default(),
		);
	}

	let (mut body, _, total, mime) = read(0, u64::MAX).await?;
	if total > WHOLE_FILE_MAX_BYTES {
		return Err((
			StatusCode::PAYLOAD_TOO_LARGE,
			format!("{:.0} MB is too large to preview remotely", total as f64 / 1_000_000.0),
		));
	}
	while (body.len() as u64) < total {
		let (bytes, ..) = read(body.len() as u64, u64::MAX).await?;
		if bytes.is_empty() {
			break;
		}
		body.extend_from_slice(&bytes);
	}
	Ok(
		Response::builder()
			.status(StatusCode::OK)
			.header(
				header::CONTENT_TYPE,
				mime.unwrap_or_else(|| "application/octet-stream".to_owned()),
			)
			.header(header::ACCEPT_RANGES, "bytes")
			.body(body)
			.unwrap_or_default(),
	)
}

/// `bytes=start-` or `bytes=start-end`; suffix and multi-ranges fall
/// back to a whole-file response.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
	let spec = value.strip_prefix("bytes=")?;
	if spec.contains(',') {
		return None;
	}
	let (start, end) = spec.split_once('-')?;
	let start = start.parse().ok()?;
	let end = if end.is_empty() { None } else { Some(end.parse().ok()?) };
	Some((start, end))
}

fn percent_decode(input: &str) -> Option<String> {
	let bytes = input.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] != b'%' {
			out.push(bytes[i]);
			i += 1;
			continue;
		}
		let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
		out.push(u8::from_str_radix(hex, 16).ok()?);
		i += 3;
	}
	String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_byte_ranges() {
		assert_eq!(parse_range("bytes=0-"), Some((0, None)));
		assert_eq!(parse_range("bytes=100-199"), Some((100, Some(199))));
		assert_eq!(parse_range("bytes=-500"), None);
		assert_eq!(parse_range("bytes=0-1,5-6"), None);
		assert_eq!(parse_range("items=0-1"), None);
	}

	#[test]
	fn decodes_percent_escapes() {
		assert_eq!(
			percent_decode("home%2Fme%2Fa%20b.png").as_deref(),
			Some("home/me/a b.png")
		);
		assert_eq!(percent_decode("caf%C3%A9").as_deref(), Some("café"));
		assert_eq!(percent_decode("bad%2"), None);
	}
}
