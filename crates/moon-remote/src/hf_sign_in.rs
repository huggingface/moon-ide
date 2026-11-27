//! Hugging Face sign-in for a headless IDE, started from the phone.
//!
//! When the IDE is enrolled with a public bridge, it signs in through
//! the bridge's OAuth client metadata document (CIMD,
//! [ADR 0130](../../../specs/decisions/0130-hf-sign-in-through-the-bridge.md)):
//! the phone opens one authorize link, Hugging Face redirects to the
//! bridge, and this process collects the code there and exchanges it
//! with the PKCE verifier only it holds. Without such a bridge it
//! falls back to the device flow, where the user types a code.
//!
//! Either way the IDE waits for the approval itself; the phone only
//! shows the link and watches `coder_status`.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use moon_coder::defaults::HF_HUB_BASE;
use moon_coder::CoderHandle;
use sha2::{Digest, Sha256};

/// What a CIMD client may be granted is fixed by the Hub (its MCP
/// scope set); moon-ide's own app additionally has `read-billing`,
/// which only feeds the billing picker.
const CIMD_SCOPES: &str = "openid profile inference-api contribute-repos";
const COLLECT_EVERY: Duration = Duration::from_secs(2);
const COLLECT_FOR: Duration = Duration::from_secs(10 * 60);

/// What the phone shows: a link, and for the device flow a code to
/// type there.
#[derive(serde::Serialize)]
pub struct SignInStart {
	pub verification_uri: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub user_code: Option<String>,
	pub expires_in: u64,
}

pub async fn start(coder: &CoderHandle) -> anyhow::Result<SignInStart> {
	if let Some(origin) = public_bridge_origin() {
		return start_through_bridge(coder, origin);
	}
	let device = coder.start_device_flow().await?;
	let start = SignInStart {
		verification_uri: device
			.verification_uri_complete
			.clone()
			.unwrap_or_else(|| device.verification_uri.clone()),
		user_code: Some(device.user_code.clone()),
		expires_in: device.expires_in,
	};
	let coder = coder.clone();
	tokio::spawn(async move {
		match coder.poll_device_code(device).await {
			Ok(identity) => tracing::info!(user = %identity.username, "signed in to Hugging Face from the phone"),
			Err(err) => tracing::warn!(error = %err, "Hugging Face sign-in from the phone did not complete"),
		}
	});
	Ok(start)
}

fn start_through_bridge(coder: &CoderHandle, origin: String) -> anyhow::Result<SignInStart> {
	let verifier = random_token()?;
	let state = random_token()?;
	let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
	let client_id = format!("{origin}/oauth/client.json");
	let redirect_uri = format!("{origin}/oauth/callback");
	let authorize = url::Url::parse_with_params(
		&format!("{HF_HUB_BASE}/oauth/authorize"),
		&[
			("client_id", client_id.as_str()),
			("redirect_uri", redirect_uri.as_str()),
			("response_type", "code"),
			("scope", CIMD_SCOPES),
			("state", state.as_str()),
			("code_challenge", challenge.as_str()),
			("code_challenge_method", "S256"),
		],
	)?;
	let coder = coder.clone();
	tokio::spawn(async move {
		match collect_and_exchange(&coder, &origin, &state, &client_id, &redirect_uri, &verifier).await {
			Ok(user) => tracing::info!(%user, "signed in to Hugging Face from the phone"),
			Err(err) => tracing::warn!(error = %err, "Hugging Face sign-in from the phone did not complete"),
		}
	});
	Ok(SignInStart {
		verification_uri: authorize.into(),
		user_code: None,
		expires_in: COLLECT_FOR.as_secs(),
	})
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Collected {
	Code(String),
	Error(String),
}

async fn collect_and_exchange(
	coder: &CoderHandle,
	origin: &str,
	state: &str,
	client_id: &str,
	redirect_uri: &str,
	verifier: &str,
) -> anyhow::Result<String> {
	let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
	let collect = url::Url::parse_with_params(&format!("{origin}/oauth/code"), &[("state", state)])?;
	let started = std::time::Instant::now();
	let collected = loop {
		if started.elapsed() > COLLECT_FOR {
			anyhow::bail!("nobody approved the sign-in in time");
		}
		tokio::time::sleep(COLLECT_EVERY).await;
		let Ok(response) = http.get(collect.clone()).send().await else {
			continue;
		};
		if !response.status().is_success() {
			continue;
		}
		break response.json::<Collected>().await?;
	};
	let code = match collected {
		Collected::Code(code) => code,
		Collected::Error(error) => anyhow::bail!("Hugging Face refused: {error}"),
	};
	let identity = coder
		.exchange_authorization_code(client_id, redirect_uri, &code, verifier)
		.await?;
	Ok(identity.username)
}

/// The `https://` origin of the bridge this IDE is enrolled with, if
/// Hugging Face can reach it: CIMD has the Hub fetch the client
/// document, so a LAN bridge (an IP, `.local`) can't serve as client.
fn public_bridge_origin() -> Option<String> {
	let credential = crate::relay::load_credential().ok().flatten()?;
	let url = url::Url::parse(&credential.bridge_url).ok()?;
	if url.scheme() != "wss" {
		return None;
	}
	let host = url.host()?;
	let url::Host::Domain(domain) = host else {
		return None;
	};
	if domain == "localhost" || domain.ends_with(".local") || !domain.contains('.') {
		return None;
	}
	Some(match url.port() {
		Some(port) => format!("https://{domain}:{port}"),
		None => format!("https://{domain}"),
	})
}

fn random_token() -> anyhow::Result<String> {
	let mut bytes = [0u8; 32];
	getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
	Ok(URL_SAFE_NO_PAD.encode(bytes))
}
