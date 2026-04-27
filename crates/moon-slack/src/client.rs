//! Hand-rolled Slack Web API client.
//!
//! Phase 11.0 endpoints:
//! - `auth.test` — identify the human whose token we hold
//! - `conversations.list?types=im` — list the user's open DMs
//! - `users.info` — pull `is_bot` + display metadata for each DM partner
//!
//! See [`specs/slack-chat.md`](../../specs/slack-chat.md) for the
//! "scan-the-user's-own-DMs" approach and why we don't paginate
//! `users.list` over the whole workspace.
//!
//! Per `specs/slack-chat.md`, we deliberately avoid `slack-morphism` /
//! `slack_api`: those crates carry OAuth flows, signing, and a type
//! universe we don't use, all on the user-token critical path.

use moon_protocol::slack::{SlackBotProfile, SlackIdentity};
use serde::{Deserialize, Serialize};

use crate::error::SlackError;

const SLACK_API_BASE: &str = "https://slack.com/api";

/// Number of DMs we scan when looking for bots. Slack returns DMs in
/// recency order, so this is "the user's 50 most recently active
/// DMs". The cap is also surfaced in the UI (connect modal + picker
/// copy) — keep this value in sync with the strings in
/// `ChatConnectModal.svelte` and `ChatPanel.svelte`. See
/// `specs/slack-chat.md#cost` for why 50 is the right size.
pub const DM_SCAN_LIMIT: usize = 50;

#[derive(Clone)]
pub struct SlackClient {
	http: reqwest::Client,
	token: String,
}

impl SlackClient {
	pub fn new(token: String) -> Result<Self, SlackError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(|err| SlackError::Transport(err.to_string()))?;
		Ok(Self { http, token })
	}

	/// `auth.test` — succeeds iff the token is valid; returns the
	/// human's identity. Used for the connect handshake and the
	/// periodic "still connected?" check.
	pub async fn auth_test(&self) -> Result<SlackIdentity, SlackError> {
		let body: AuthTestResponse = self.call("auth.test", &[]).await?;
		Ok(SlackIdentity {
			user_id: body.user_id,
			user_name: body.user,
			team_id: body.team_id,
			team: body.team,
			url: body.url,
		})
	}

	/// Scan the authenticated user's [`DM_SCAN_LIMIT`] most recently
	/// active DMs and return every partner that's a bot.
	///
	/// Implementation: one `conversations.list?types=im&limit=50` call
	/// (Slack returns DMs newest-first), then `users.info` per
	/// partner — `users.info` is tier-4 (100+ /min) so 50 sequential
	/// calls finish in ~10–20 s in the typical case and well within
	/// the rate-limit budget. Filters to `is_bot && !deleted`.
	///
	/// Returns matches in the same order Slack returned the DMs, so
	/// the bot the user has talked to most recently sorts toward the
	/// top of the picker.
	///
	/// Bots living in DMs older than the 50th will not be discovered
	/// — by design. The number is surfaced upfront in the connect
	/// modal so the user knows to bump older bot DMs by sending a
	/// quick "hi" from regular Slack before connecting.
	pub async fn list_dm_bots(&self) -> Result<Vec<SlackBotProfile>, SlackError> {
		let ims = self.list_im_channels().await?;
		let mut bots = Vec::with_capacity(ims.len() / 4);
		for im in ims {
			let user = match self.users_info(&im.user).await {
				Ok(user) => user,
				Err(SlackError::Api { code, .. }) if code == "user_not_found" => {
					tracing::debug!(user_id = %im.user, "users.info returned user_not_found; skipping");
					continue;
				}
				Err(err) => return Err(err),
			};
			if !user.is_bot.unwrap_or(false) || user.deleted.unwrap_or(false) {
				continue;
			}
			bots.push(SlackBotProfile {
				user_id: user.id,
				dm_channel_id: im.id,
				username: user.name.unwrap_or_default(),
				real_name: user.real_name.unwrap_or_default(),
				display_name: user
					.profile
					.as_ref()
					.and_then(|p| p.display_name.clone())
					.filter(|s| !s.is_empty()),
				image_url: user.profile.as_ref().and_then(|p| {
					p.image_72
						.clone()
						.or_else(|| p.image_48.clone())
						.or_else(|| p.image_24.clone())
				}),
			});
		}
		Ok(bots)
	}

	async fn list_im_channels(&self) -> Result<Vec<ImChannel>, SlackError> {
		let limit = DM_SCAN_LIMIT.to_string();
		let body: ConversationsListResponse = self
			.call("conversations.list", &[("types", "im"), ("limit", limit.as_str())])
			.await?;
		Ok(body.channels)
	}

	async fn users_info(&self, user_id: &str) -> Result<SlackUser, SlackError> {
		let body: UsersInfoResponse = self.call("users.info", &[("user", user_id)]).await?;
		Ok(body.user)
	}

	async fn call<R: for<'de> Deserialize<'de>>(&self, method: &str, params: &[(&str, &str)]) -> Result<R, SlackError> {
		let url = format!("{SLACK_API_BASE}/{method}");
		let response = self
			.http
			.get(&url)
			.bearer_auth(&self.token)
			.query(params)
			.send()
			.await
			.map_err(|err| SlackError::Transport(err.to_string()))?;

		let status = response.status();
		let bytes = response
			.bytes()
			.await
			.map_err(|err| SlackError::Transport(err.to_string()))?;
		if !status.is_success() {
			return Err(SlackError::Http {
				status: status.as_u16(),
				body: String::from_utf8_lossy(&bytes).into_owned(),
			});
		}

		let envelope: SlackEnvelope = serde_json::from_slice(&bytes).map_err(|err| SlackError::Decode(err.to_string()))?;
		if !envelope.ok {
			return Err(SlackError::Api {
				method: method.to_string(),
				code: envelope.error.unwrap_or_else(|| "unknown".to_string()),
			});
		}
		serde_json::from_slice(&bytes).map_err(|err| SlackError::Decode(err.to_string()))
	}
}

// --- Wire types -----------------------------------------------------------
//
// Kept private; only the protocol-level types in `moon-protocol::slack`
// cross the IPC boundary. Slack response shapes are *much* wider than
// what we need, so we deserialise only the fields we touch.

#[derive(Debug, Deserialize)]
struct SlackEnvelope {
	ok: bool,
	#[serde(default)]
	error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthTestResponse {
	user_id: String,
	user: String,
	team_id: String,
	team: String,
	url: String,
}

#[derive(Debug, Deserialize)]
struct ConversationsListResponse {
	channels: Vec<ImChannel>,
}

/// One entry of `conversations.list?types=im`. The `user` field is
/// the *other* party in the DM (never the authenticated user).
#[derive(Debug, Deserialize)]
struct ImChannel {
	id: String,
	user: String,
}

#[derive(Debug, Deserialize)]
struct UsersInfoResponse {
	user: SlackUser,
}

/// Slack `objects.User`. Marked `Serialize` only for test fixtures —
/// it never leaves this crate.
#[derive(Debug, Deserialize, Serialize)]
struct SlackUser {
	id: String,
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	real_name: Option<String>,
	#[serde(default)]
	is_bot: Option<bool>,
	#[serde(default)]
	deleted: Option<bool>,
	#[serde(default)]
	profile: Option<SlackUserProfile>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SlackUserProfile {
	#[serde(default)]
	display_name: Option<String>,
	#[serde(default)]
	real_name: Option<String>,
	#[serde(default)]
	image_24: Option<String>,
	#[serde(default)]
	image_48: Option<String>,
	#[serde(default)]
	image_72: Option<String>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn deserializes_im_list() {
		let json = r#"{
			"ok": true,
			"channels": [
				{ "id": "D01", "user": "U01" },
				{ "id": "D02", "user": "U02" }
			]
		}"#;
		let r: ConversationsListResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.channels.len(), 2);
		assert_eq!(r.channels[0].id, "D01");
		assert_eq!(r.channels[1].user, "U02");
	}

	#[test]
	fn deserializes_users_info_bot() {
		let json = r#"{
			"ok": true,
			"user": {
				"id": "U0AGNMHHQ1H",
				"name": "moon_bot",
				"real_name": "Moon Bot",
				"is_bot": true,
				"deleted": false,
				"profile": {
					"display_name": "",
					"real_name": "Moon Bot",
					"image_72": "https://example.com/avatar.png"
				}
			}
		}"#;
		let r: UsersInfoResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.user.id, "U0AGNMHHQ1H");
		assert_eq!(r.user.is_bot, Some(true));
		assert_eq!(r.user.real_name.as_deref(), Some("Moon Bot"));
	}

	#[test]
	fn deserializes_users_info_human() {
		let json = r#"{
			"ok": true,
			"user": {
				"id": "U02SCR6D6U8",
				"name": "eliott",
				"real_name": "Eli Ott",
				"is_bot": false,
				"deleted": false
			}
		}"#;
		let r: UsersInfoResponse = serde_json::from_str(json).unwrap();
		assert_eq!(r.user.is_bot, Some(false));
		assert!(r.user.profile.is_none());
	}

	#[test]
	fn auth_failure_classification() {
		let api = SlackError::Api {
			method: "auth.test".into(),
			code: "invalid_auth".into(),
		};
		assert!(api.is_auth_failure());

		let other = SlackError::Api {
			method: "auth.test".into(),
			code: "ratelimited".into(),
		};
		assert!(!other.is_auth_failure());

		let transport = SlackError::Transport("dns failure".into());
		assert!(!transport.is_auth_failure());
	}
}
