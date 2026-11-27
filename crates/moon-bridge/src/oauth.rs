//! Hugging Face sign-in for headless IDEs through an OAuth client
//! metadata document (CIMD), so the phone signs a remote IDE in with
//! one redirect instead of a device code
//! ([ADR 0130](../../../specs/decisions/0130-hf-sign-in-through-the-bridge.md)).
//!
//! The bridge's public origin is the OAuth client: it serves the
//! client's metadata and is its redirect target. The IDE holds the
//! PKCE verifier and runs the exchange; the bridge only parks the
//! authorization code under its `state` until that IDE collects it.
//! A code is useless without the verifier, so parking it here
//! doesn't make the relay any less blind.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CLIENT_METADATA_PATH: &str = "/oauth/client.json";
pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const COLLECT_PATH: &str = "/oauth/code";

/// How long a parked code waits for its IDE. HF's codes expire
/// sooner; this only bounds memory.
const CODE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PARKED: usize = 64;
/// The IDE's `state` is 43 base64url chars; anything far longer is
/// not ours.
const MAX_STATE_LEN: usize = 128;
const MAX_VALUE_LEN: usize = 2048;

/// A response for the HTTP front to write.
pub struct Reply {
	pub status: &'static str,
	pub content_type: &'static str,
	pub cache_control: &'static str,
	pub body: Vec<u8>,
}

#[derive(Default)]
pub struct ParkedCodes {
	codes: Mutex<HashMap<String, (Instant, Outcome)>>,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
	Code(String),
	Error(String),
}

impl ParkedCodes {
	fn park(&self, state: String, outcome: Outcome) {
		let mut codes = self.codes.lock().expect("oauth codes lock");
		codes.retain(|_, (at, _)| at.elapsed() < CODE_TTL);
		if codes.len() >= MAX_PARKED {
			return;
		}
		codes.insert(state, (Instant::now(), outcome));
	}

	fn take(&self, state: &str) -> Option<Outcome> {
		let mut codes = self.codes.lock().expect("oauth codes lock");
		let (at, outcome) = codes.remove(state)?;
		(at.elapsed() < CODE_TTL).then_some(outcome)
	}
}

/// The bridge's public `https://` origin, from the `wss://` URL it
/// advertises.
pub fn public_origin(advertise_url: &str) -> String {
	let rest = advertise_url
		.strip_prefix("wss://")
		.map(|rest| format!("https://{rest}"))
		.or_else(|| advertise_url.strip_prefix("ws://").map(|rest| format!("http://{rest}")))
		.unwrap_or_else(|| advertise_url.to_owned());
	rest.trim_end_matches('/').to_owned()
}

/// Answers the three OAuth paths; `None` for anything else.
pub fn handle(path: &str, query: &str, origin: &str, parked: &ParkedCodes) -> Option<Reply> {
	match path {
		CLIENT_METADATA_PATH => Some(client_metadata(origin)),
		CALLBACK_PATH => Some(callback(query, parked)),
		COLLECT_PATH => Some(collect(query, parked)),
		_ => None,
	}
}

fn client_metadata(origin: &str) -> Reply {
	let body = serde_json::json!({
		"client_id": format!("{origin}{CLIENT_METADATA_PATH}"),
		"client_name": "moon-ide",
		"client_uri": "https://github.com/huggingface/moon-ide",
		"logo_uri": format!("{origin}/icon-512.png"),
		"redirect_uris": [format!("{origin}{CALLBACK_PATH}")],
		"grant_types": ["authorization_code", "refresh_token"],
		"response_types": ["code"],
		"token_endpoint_auth_method": "none",
	});
	Reply {
		status: "200 OK",
		content_type: "application/json",
		cache_control: "public, max-age=3600",
		body: body.to_string().into_bytes(),
	}
}

fn callback(query: &str, parked: &ParkedCodes) -> Reply {
	let params = parse_query(query);
	let state = params.get("state").filter(|s| valid(s, MAX_STATE_LEN));
	let outcome = match (params.get("code"), params.get("error")) {
		(Some(code), _) if valid(code, MAX_VALUE_LEN) => Some(Outcome::Code(code.clone())),
		(_, Some(error)) if valid(error, MAX_VALUE_LEN) => Some(Outcome::Error(
			params
				.get("error_description")
				.filter(|d| d.len() <= MAX_VALUE_LEN)
				.cloned()
				.unwrap_or_else(|| error.clone()),
		)),
		_ => None,
	};
	let (Some(state), Some(outcome)) = (state, outcome) else {
		return page("400 Bad Request", "This sign-in link is incomplete.");
	};
	let message = match &outcome {
		Outcome::Code(_) => "Signed in to Hugging Face. You can close this tab and go back to moon-ide.",
		Outcome::Error(_) => "Hugging Face did not sign you in. Go back to moon-ide to try again.",
	};
	parked.park(state.clone(), outcome);
	page("200 OK", message)
}

fn collect(query: &str, parked: &ParkedCodes) -> Reply {
	let params = parse_query(query);
	let outcome = params.get("state").and_then(|state| parked.take(state));
	let Some(outcome) = outcome else {
		return Reply {
			status: "404 Not Found",
			content_type: "application/json",
			cache_control: "no-store",
			body: b"{}".to_vec(),
		};
	};
	Reply {
		status: "200 OK",
		content_type: "application/json",
		cache_control: "no-store",
		body: serde_json::to_vec(&outcome).unwrap_or_default(),
	}
}

fn page(status: &'static str, message: &str) -> Reply {
	let body = format!(
		"<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
		 content=\"width=device-width, initial-scale=1\"><title>moon-ide</title>\
		 <link rel=\"icon\" href=\"/icon-192.png\"></head>\
		 <body style=\"font-family: system-ui, sans-serif; max-width: 28rem; margin: 4rem auto; \
		 padding: 0 1rem; text-align: center\"><img src=\"/icon-192.png\" alt=\"\" width=\"72\" \
		 height=\"72\"><p>{message}</p></body></html>"
	);
	Reply {
		status,
		content_type: "text/html; charset=utf-8",
		cache_control: "no-store",
		body: body.into_bytes(),
	}
}

fn valid(value: &str, max: usize) -> bool {
	!value.is_empty() && value.len() <= max
}

fn parse_query(query: &str) -> HashMap<String, String> {
	url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn body(reply: &Reply) -> serde_json::Value {
		serde_json::from_slice(&reply.body).expect("json body")
	}

	#[test]
	fn origin_follows_the_advertised_url() {
		assert_eq!(public_origin("wss://bridge.coyo.dev"), "https://bridge.coyo.dev");
		assert_eq!(
			public_origin("wss://bridge.example:8443/"),
			"https://bridge.example:8443"
		);
	}

	#[test]
	fn metadata_names_the_bridge_as_client_and_redirect() {
		let reply = client_metadata("https://bridge.example");
		let doc = body(&reply);
		assert_eq!(doc["client_id"], "https://bridge.example/oauth/client.json");
		assert_eq!(doc["redirect_uris"][0], "https://bridge.example/oauth/callback");
		assert_eq!(doc["token_endpoint_auth_method"], "none");
	}

	#[test]
	fn a_code_is_collected_once_by_its_state() {
		let parked = ParkedCodes::default();
		let reply = callback("code=abc&state=s1", &parked);
		assert_eq!(reply.status, "200 OK");

		assert_eq!(collect("state=other", &parked).status, "404 Not Found");
		let first = collect("state=s1", &parked);
		assert_eq!(body(&first), serde_json::json!({ "code": "abc" }));
		assert_eq!(collect("state=s1", &parked).status, "404 Not Found");
	}

	#[test]
	fn a_denial_is_handed_over_too() {
		let parked = ParkedCodes::default();
		callback("error=access_denied&error_description=User+denied&state=s2", &parked);
		assert_eq!(
			body(&collect("state=s2", &parked)),
			serde_json::json!({ "error": "User denied" })
		);
	}

	#[test]
	fn incomplete_callbacks_park_nothing() {
		let parked = ParkedCodes::default();
		assert_eq!(callback("code=abc", &parked).status, "400 Bad Request");
		assert_eq!(callback("state=s3", &parked).status, "400 Bad Request");
		assert_eq!(collect("state=s3", &parked).status, "404 Not Found");
	}
}
