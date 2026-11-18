//! Read-only Slack access for the agent (ADR 0122).
//!
//! Two tools, `slack_search` and `slack_read`, call Slack's Web API
//! from the IDE process with a **user token** that carries read scopes
//! only: nothing here can post, react or mark anything read, and
//! Slack would refuse if it tried.
//!
//! The token is per workspace. It lives in the OS keyring under
//! `service=moon-ide, account=coder-slack:<workspace-id>`, never in
//! `session.json` and never in the dev container, where the agent's
//! own `bash` could read it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use moon_protocol::coder_slack::{SlackConnection, SlackStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::CoderError;

const KEYRING_SERVICE: &str = "moon-ide";
const SLACK_API_BASE: &str = "https://slack.com/api";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

const DEFAULT_SEARCH_COUNT: u32 = 20;
const MAX_SEARCH_COUNT: u32 = 50;
const DEFAULT_READ_LIMIT: u32 = 50;
const MAX_READ_LIMIT: u32 = 200;

/// A search hit is a pointer to a thread, so its text is cut short;
/// `slack_read` returns the whole thing.
const MAX_MATCH_CHARS: usize = 1200;
const MAX_MESSAGE_CHARS: usize = 6000;
/// Text returned by one `slack_read`, across its messages.
const MAX_READ_CHARS: usize = 80_000;
/// `users.info` calls one tool call may spend on turning ids into
/// names. Ids past this stay ids.
const MAX_NAME_LOOKUPS: usize = 25;

/// User-token scopes the tools need. None of them writes.
pub const SCOPES: &[&str] = &[
	"search:read",
	"channels:history",
	"groups:history",
	"im:history",
	"mpim:history",
	"channels:read",
	"groups:read",
	"im:read",
	"mpim:read",
	"users:read",
];

/// Why a Slack call didn't produce an answer, in words fit to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackError {
	Aborted,
	Failed(String),
}

impl std::fmt::Display for SlackError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			SlackError::Aborted => f.write_str("cancelled"),
			SlackError::Failed(message) => f.write_str(message),
		}
	}
}

impl SlackError {
	fn failed(message: impl Into<String>) -> Self {
		SlackError::Failed(message.into())
	}

	/// As the error of `tool`'s call.
	pub fn into_tool(self, tool: &str) -> CoderError {
		match self {
			SlackError::Aborted => CoderError::Aborted,
			SlackError::Failed(message) => CoderError::tool_failed(tool, message),
		}
	}
}

/// What `slack_read` was pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadTarget {
	/// The thread `ts` belongs to (or starts) in `channel`.
	Thread { channel: String, ts: String },
	/// The latest messages of `channel`.
	Channel { channel: String },
}

fn is_conversation_id(text: &str) -> bool {
	text.len() >= 9
		&& text.starts_with(['C', 'G', 'D'])
		&& text.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn is_ts(text: &str) -> bool {
	text.split_once('.').is_some_and(|(secs, micros)| {
		secs.len() >= 9 && micros.len() == 6 && (secs.bytes().chain(micros.bytes())).all(|b| b.is_ascii_digit())
	})
}

/// `p1700000000123456` (a permalink's last segment) → `1700000000.123456`.
fn ts_from_permalink_segment(segment: &str) -> Option<String> {
	let digits = segment.strip_prefix('p')?;
	if digits.len() < 16 || !digits.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	let (secs, micros) = digits.split_at(digits.len() - 6);
	Some(format!("{secs}.{micros}"))
}

/// Work out what to read from `target` (a message permalink, a channel
/// URL or a conversation id) and an optional `thread_ts`.
pub fn parse_target(target: &str, thread_ts: Option<&str>) -> Result<ReadTarget, String> {
	let target = target.trim();
	let thread_ts = thread_ts.map(str::trim).filter(|ts| !ts.is_empty());
	if let Some(ts) = thread_ts {
		if !is_ts(ts) {
			return Err(format!("thread_ts must look like 1700000000.123456, got {ts}"));
		}
	}
	let with_thread = |channel: String, ts: Option<String>| match thread_ts.map(str::to_owned).or(ts) {
		Some(ts) => ReadTarget::Thread { channel, ts },
		None => ReadTarget::Channel { channel },
	};
	if is_conversation_id(target) {
		return Ok(with_thread(target.to_owned(), None));
	}
	let Ok(url) = url::Url::parse(target) else {
		return Err(format!(
			"target must be a Slack message permalink, a channel URL or a channel ID (C…/G…/D…), got {target:?}. \
			 To read a channel by name, find its ID with slack_search (`in:#name`)."
		));
	};
	let segments: Vec<&str> = url.path_segments().map(Iterator::collect).unwrap_or_default();
	// `<team>.slack.com/archives/<channel>[/p<ts>]`, or the web
	// client's `app.slack.com/client/<team>/<channel>`.
	let channel = match segments.as_slice() {
		["archives", channel, ..] => channel,
		["client", _team, channel, ..] => channel,
		_ => return Err(format!("not a Slack message or channel URL: {target}")),
	};
	if !is_conversation_id(channel) {
		return Err(format!("not a Slack message or channel URL: {target}"));
	}
	// A reply's permalink names its thread in the query.
	let root = url
		.query_pairs()
		.find(|(key, _)| key == "thread_ts")
		.map(|(_, value)| value.into_owned())
		.filter(|ts| is_ts(ts));
	let message = segments.get(2).and_then(|segment| ts_from_permalink_segment(segment));
	Ok(with_thread((*channel).to_owned(), root.or(message)))
}

/// Ids of the users `raw` mentions without spelling out their name.
fn mentioned_users(raw: &str) -> Vec<&str> {
	let mut ids = Vec::new();
	let mut rest = raw;
	while let Some(start) = rest.find("<@") {
		let after = &rest[start + 2..];
		let Some(end) = after.find('>') else {
			break;
		};
		let inner = &after[..end];
		if !inner.contains('|') {
			ids.push(inner);
		}
		rest = &after[end + 1..];
	}
	ids
}

fn render_token(inner: &str, names: &HashMap<String, String>) -> String {
	let (target, label) = match inner.split_once('|') {
		Some((target, label)) if !label.is_empty() => (target, Some(label)),
		Some((target, _)) => (target, None),
		None => (inner, None),
	};
	if let Some(id) = target.strip_prefix('@') {
		let name = label.or_else(|| names.get(id).map(String::as_str)).unwrap_or(id);
		return format!("@{name}");
	}
	if let Some(id) = target.strip_prefix('#') {
		let name = label.or_else(|| names.get(id).map(String::as_str)).unwrap_or(id);
		return format!("#{name}");
	}
	if let Some(command) = target.strip_prefix('!') {
		// `<!here>`, `<!subteam^S123|@team>`, `<!date^…|fallback>`.
		return match label {
			Some(label) => label.to_owned(),
			None => format!("@{}", command.split('^').next().unwrap_or(command)),
		};
	}
	match label {
		Some(label) if label != target => format!("{label} ({target})"),
		_ => target.to_owned(),
	}
}

/// Slack's message markup as plain text: `<@U1>` → `@name`,
/// `<#C1|general>` → `#general`, `<https://x|label>` → `label
/// (https://x)`, and the three escaped characters back to themselves.
/// Unknown ids are left as they are.
pub fn render_text(raw: &str, names: &HashMap<String, String>) -> String {
	let mut out = String::with_capacity(raw.len());
	let mut rest = raw;
	while let Some(start) = rest.find('<') {
		out.push_str(&rest[..start]);
		let tail = &rest[start + 1..];
		// A literal `<` is always escaped, so this is markup; an
		// unclosed or multi-line one is somebody's broken payload.
		match tail.find('>') {
			Some(end) if !tail[..end].contains('\n') => {
				out.push_str(&render_token(&tail[..end], names));
				rest = &tail[end + 1..];
			}
			_ => {
				out.push('<');
				rest = tail;
			}
		}
	}
	out.push_str(rest);
	out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `text` cut to `max` characters, with a mark when something went.
fn clip(text: String, max: usize) -> String {
	match text.char_indices().nth(max) {
		Some((cut, _)) => format!("{}… [cut]", &text[..cut]),
		None => text,
	}
}

/// Slack's `ts` (`1700000000.123456`) as an ISO 8601 UTC time.
fn time_of(ts: &str) -> String {
	let secs = ts.split('.').next().and_then(|secs| secs.parse::<i64>().ok());
	match secs {
		Some(secs) => {
			let iso = crate::sessions::iso8601_utc_ms(secs * 1000);
			iso.replace(".000Z", "Z")
		}
		None => ts.to_owned(),
	}
}

#[derive(Debug, Deserialize)]
struct RawMatch {
	channel: RawMatchChannel,
	#[serde(default)]
	user: Option<String>,
	#[serde(default)]
	username: Option<String>,
	ts: String,
	#[serde(default)]
	text: String,
	#[serde(default)]
	permalink: String,
	#[serde(default)]
	attachments: Vec<RawAttachment>,
}

#[derive(Debug, Deserialize)]
struct RawMatchChannel {
	id: String,
	#[serde(default)]
	name: String,
	#[serde(default)]
	is_im: bool,
	#[serde(default)]
	is_mpim: bool,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
	#[serde(default)]
	user: Option<String>,
	#[serde(default)]
	username: Option<String>,
	#[serde(default)]
	bot_profile: Option<RawBotProfile>,
	ts: String,
	#[serde(default)]
	thread_ts: Option<String>,
	#[serde(default)]
	text: String,
	#[serde(default)]
	reply_count: u32,
	#[serde(default)]
	attachments: Vec<RawAttachment>,
	#[serde(default)]
	files: Vec<RawFile>,
}

#[derive(Debug, Deserialize)]
struct RawBotProfile {
	#[serde(default)]
	name: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawAttachment {
	#[serde(default)]
	title: Option<String>,
	#[serde(default)]
	text: Option<String>,
	#[serde(default)]
	fallback: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	title: Option<String>,
}

/// A message's text with what its attachments say appended: bots
/// often put the whole message there and leave `text` empty.
fn full_text(text: &str, attachments: &[RawAttachment]) -> String {
	let mut out = text.to_owned();
	for attachment in attachments {
		let body = match (attachment.title.as_deref(), attachment.text.as_deref()) {
			(Some(title), Some(text)) if !title.is_empty() && !text.is_empty() => format!("{title}\n{text}"),
			(Some(only), _) | (_, Some(only)) if !only.is_empty() => only.to_owned(),
			_ => attachment.fallback.clone().unwrap_or_default(),
		};
		if body.is_empty() || out.contains(&body) {
			continue;
		}
		if !out.is_empty() {
			out.push('\n');
		}
		out.push_str(&body);
	}
	out
}

/// How a failed Web API call reads to whoever has to fix it.
fn describe_api_error(method: &str, code: &str, needed: Option<&str>) -> String {
	match code {
		"missing_scope" => format!(
			"the Slack token lacks the `{}` scope ({method}). Add it under the app's User Token Scopes, reinstall the app, \
			 and reconnect in Coder settings → Slack.",
			needed.unwrap_or("required")
		),
		"not_allowed_token_type" => {
			format!("{method} needs a user token (xoxp-…); this looks like a bot token.")
		}
		"invalid_auth" | "not_authed" | "token_revoked" | "token_expired" | "account_inactive" => {
			format!("Slack rejected the token ({code}). Reconnect in Coder settings → Slack.")
		}
		"channel_not_found" => "Slack: channel not found, or the token's user is not in it.".to_owned(),
		"thread_not_found" => "Slack: no message with that timestamp in that channel.".to_owned(),
		_ => format!("Slack API error on {method}: {code}"),
	}
}

/// Web API client plus the workspace's token. Cheap to clone.
#[derive(Clone)]
pub struct SlackClient {
	http: reqwest::Client,
	/// Where the Web API lives; only tests point it elsewhere.
	base: String,
	/// Keyring account of this workspace's token.
	account: String,
	/// The token, cached so the sync tool advertiser never touches
	/// the keyring.
	token: Arc<RwLock<Option<String>>>,
	/// Who the token is, once `auth.test` has said.
	connection: Arc<RwLock<Option<SlackConnection>>>,
	/// User id → display name and conversation id → label, for the
	/// life of the process.
	names: Arc<RwLock<HashMap<String, String>>>,
}

impl SlackClient {
	pub fn new(workspace_id: &str) -> Result<Self, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.timeout(HTTP_TIMEOUT)
			.build()
			.map_err(CoderError::from)?;
		let account = format!("coder-slack:{workspace_id}");
		let initial = load_token(&account).unwrap_or_else(|err| {
			tracing::warn!(error = %err, "failed to load the Slack token from the keyring; treating as not connected");
			None
		});
		Ok(Self {
			http,
			base: SLACK_API_BASE.to_owned(),
			account,
			token: Arc::new(RwLock::new(initial)),
			connection: Arc::default(),
			names: Arc::default(),
		})
	}

	/// A client with no token that never consults the keyring.
	#[cfg(test)]
	pub(crate) fn for_tests() -> Self {
		Self {
			http: reqwest::Client::new(),
			base: SLACK_API_BASE.to_owned(),
			account: "coder-slack:tests".to_owned(),
			token: Arc::default(),
			connection: Arc::default(),
			names: Arc::default(),
		}
	}

	/// Stand in for a successful connect, without Slack or the keyring.
	#[cfg(test)]
	pub(crate) fn set_token_for_tests(&self, token: Option<&str>) {
		self.remember(token.map(str::to_owned), None);
	}

	/// Whether this workspace has a token: the tools are advertised
	/// only then.
	pub fn is_connected(&self) -> bool {
		self.token.read().map(|guard| guard.is_some()).unwrap_or(false)
	}

	fn current_token(&self) -> Option<String> {
		self.token.read().ok().and_then(|guard| guard.clone())
	}

	/// Check `token` against Slack and keep it for this workspace.
	/// Search is tried too: it is the one call that needs a user
	/// token, so a bot token or a missing scope shows up here rather
	/// than on the agent's first search.
	pub async fn connect(&self, token: &str) -> Result<SlackConnection, SlackError> {
		let token = token.trim();
		if token.is_empty() {
			return Err(SlackError::failed("Paste a Slack user token (xoxp-…) first."));
		}
		let cancel = CancellationToken::new();
		let connection = self.who(token, &cancel).await?;
		self
			.call_with(token, "search.messages", &[("query", "a"), ("count", "1")], &cancel)
			.await?;
		entry(&self.account)
			.and_then(|entry| entry.set_password(token))
			.map_err(|err| SlackError::failed(format!("could not store the token in the OS keyring: {err}")))?;
		self.remember(Some(token.to_owned()), Some(connection.clone()));
		Ok(connection)
	}

	/// Forget this workspace's token. Idempotent.
	pub fn disconnect(&self) -> Result<(), SlackError> {
		match entry(&self.account).and_then(|entry| entry.delete_credential()) {
			Ok(()) | Err(keyring::Error::NoEntry) => {}
			Err(err) => {
				return Err(SlackError::failed(format!(
					"could not remove the token from the OS keyring: {err}"
				)))
			}
		}
		self.remember(None, None);
		Ok(())
	}

	fn remember(&self, token: Option<String>, connection: Option<SlackConnection>) {
		if let Ok(mut guard) = self.token.write() {
			*guard = token;
		}
		if let Ok(mut guard) = self.connection.write() {
			*guard = connection;
		}
		if let Ok(mut guard) = self.names.write() {
			guard.clear();
		}
	}

	/// [`Self::connection`] plus the scopes to ask for, as the
	/// settings UI wants them.
	pub async fn status(&self) -> Result<SlackStatus, SlackError> {
		Ok(SlackStatus {
			connection: self.connection().await?,
			scopes: SCOPES.iter().map(|scope| (*scope).to_owned()).collect(),
		})
	}

	/// The Slack workspace and user behind this workspace's token.
	/// `None` when there is no token; an error when Slack no longer
	/// accepts it.
	pub async fn connection(&self) -> Result<Option<SlackConnection>, SlackError> {
		let Some(token) = self.current_token() else {
			return Ok(None);
		};
		if let Some(known) = self.connection.read().ok().and_then(|guard| guard.clone()) {
			return Ok(Some(known));
		}
		let connection = self.who(&token, &CancellationToken::new()).await?;
		if let Ok(mut guard) = self.connection.write() {
			*guard = Some(connection.clone());
		}
		Ok(Some(connection))
	}

	async fn who(&self, token: &str, cancel: &CancellationToken) -> Result<SlackConnection, SlackError> {
		let body = self.call_with(token, "auth.test", &[], cancel).await?;
		let field = |name: &str| body.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
		Ok(SlackConnection {
			team: field("team"),
			user: field("user"),
			url: field("url"),
		})
	}

	async fn call(&self, method: &str, params: &[(&str, &str)], cancel: &CancellationToken) -> Result<Value, SlackError> {
		let Some(token) = self.current_token() else {
			return Err(SlackError::failed(
				"Slack is not connected for this workspace. Connect it in Coder settings → Slack.",
			));
		};
		self.call_with(&token, method, params, cancel).await
	}

	async fn call_with(
		&self,
		token: &str,
		method: &str,
		params: &[(&str, &str)],
		cancel: &CancellationToken,
	) -> Result<Value, SlackError> {
		let request = self
			.http
			.get(format!("{}/{method}", self.base))
			.bearer_auth(token)
			.query(params)
			.send();
		let response = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(SlackError::Aborted),
			result = request => result.map_err(|err| SlackError::failed(format!("could not reach Slack ({method}): {err}")))?,
		};
		let status = response.status();
		if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
			let wait = response
				.headers()
				.get(reqwest::header::RETRY_AFTER)
				.and_then(|value| value.to_str().ok())
				.unwrap_or("a few");
			return Err(SlackError::failed(format!(
				"Slack is rate limiting {method}; try again in {wait} seconds."
			)));
		}
		let text = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(SlackError::Aborted),
			result = response.text() => result.map_err(|err| SlackError::failed(format!("could not read Slack's answer ({method}): {err}")))?,
		};
		if !status.is_success() {
			return Err(SlackError::failed(format!("Slack answered {status} to {method}.")));
		}
		let body: Value = serde_json::from_str(&text)
			.map_err(|err| SlackError::failed(format!("could not parse Slack's answer to {method}: {err}")))?;
		if body.get("ok").and_then(Value::as_bool) != Some(true) {
			let code = body.get("error").and_then(Value::as_str).unwrap_or("unknown");
			let needed = body.get("needed").and_then(Value::as_str);
			return Err(SlackError::failed(describe_api_error(method, code, needed)));
		}
		Ok(body)
	}

	fn known_names(&self) -> HashMap<String, String> {
		self.names.read().map(|guard| guard.clone()).unwrap_or_default()
	}

	fn learn(&self, id: &str, name: String) {
		if let Ok(mut guard) = self.names.write() {
			guard.insert(id.to_owned(), name);
		}
	}

	/// Look up the display names of `ids` not seen before, a bounded
	/// number per call. Best-effort: a failed lookup leaves the id.
	async fn resolve_users(&self, ids: impl IntoIterator<Item = &str>, cancel: &CancellationToken) {
		let known = self.known_names();
		let wanted: HashSet<&str> = ids
			.into_iter()
			.filter(|id| id.starts_with(['U', 'W']) && !known.contains_key(*id))
			.collect();
		let lookups = wanted.into_iter().take(MAX_NAME_LOOKUPS).map(|id| async move {
			let body = self.call("users.info", &[("user", id)], cancel).await.ok()?;
			let user = body.get("user")?;
			let text = |pointer: &str| {
				user
					.pointer(pointer)
					.and_then(Value::as_str)
					.filter(|name| !name.is_empty())
			};
			let name = text("/profile/display_name")
				.or_else(|| text("/real_name"))
				.or_else(|| text("/name"))?;
			Some((id.to_owned(), name.to_owned()))
		});
		for (id, name) in futures_util::future::join_all(lookups).await.into_iter().flatten() {
			self.learn(&id, name);
		}
	}

	/// `#name`, `DM with @user` or the bare id for conversation
	/// `channel`. Best-effort.
	async fn channel_label(&self, channel: &str, cancel: &CancellationToken) -> String {
		if let Some(known) = self.known_names().get(channel) {
			return known.clone();
		}
		let Ok(body) = self.call("conversations.info", &[("channel", channel)], cancel).await else {
			return channel.to_owned();
		};
		let info = body.get("channel").cloned().unwrap_or(Value::Null);
		let label = if info.get("is_im").and_then(Value::as_bool) == Some(true) {
			let other = info.get("user").and_then(Value::as_str).unwrap_or_default().to_owned();
			self.resolve_users([other.as_str()], cancel).await;
			format!("DM with @{}", self.known_names().get(&other).cloned().unwrap_or(other))
		} else {
			match info.get("name").and_then(Value::as_str) {
				Some(name) if !name.is_empty() => format!("#{name}"),
				_ => channel.to_owned(),
			}
		};
		self.learn(channel, label.clone());
		label
	}

	/// `slack_search`: messages matching `query`, in Slack's own
	/// search syntax, across everything the token's user can see.
	pub async fn search(
		&self,
		query: &str,
		count: Option<u32>,
		page: Option<u32>,
		newest_first: bool,
		cancel: &CancellationToken,
	) -> Result<Value, SlackError> {
		let query = query.trim();
		if query.is_empty() {
			return Err(SlackError::failed("query must not be empty"));
		}
		let count = count
			.unwrap_or(DEFAULT_SEARCH_COUNT)
			.clamp(1, MAX_SEARCH_COUNT)
			.to_string();
		let page = page.unwrap_or(1).max(1).to_string();
		let body = self
			.call(
				"search.messages",
				&[
					("query", query),
					("count", &count),
					("page", &page),
					("highlight", "false"),
					("sort", if newest_first { "timestamp" } else { "score" }),
				],
				cancel,
			)
			.await?;
		let matches: Vec<RawMatch> = body
			.pointer("/messages/matches")
			.cloned()
			.and_then(|value| serde_json::from_value(value).ok())
			.unwrap_or_default();

		let mut ids: Vec<&str> = Vec::new();
		for found in &matches {
			ids.extend(mentioned_users(&found.text));
			if found.channel.is_im {
				ids.push(&found.channel.name);
			}
			if found.username.as_deref().unwrap_or_default().is_empty() {
				ids.extend(found.user.as_deref());
			}
		}
		self.resolve_users(ids, cancel).await;
		let names = self.known_names();
		let name_of = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_owned());

		let results: Vec<Value> = matches
			.iter()
			.map(|found| {
				let channel = if found.channel.is_im {
					format!("DM with @{}", name_of(&found.channel.name))
				} else if found.channel.is_mpim || found.channel.name.is_empty() {
					found.channel.name.clone()
				} else {
					format!("#{}", found.channel.name)
				};
				let user = match found.username.as_deref() {
					Some(name) if !name.is_empty() => name.to_owned(),
					_ => found
						.user
						.as_deref()
						.map(&name_of)
						.unwrap_or_else(|| "unknown".to_owned()),
				};
				json!({
					"channel": channel,
					"channel_id": found.channel.id,
					"user": user,
					"time": time_of(&found.ts),
					"text": clip(render_text(&full_text(&found.text, &found.attachments), &names), MAX_MATCH_CHARS),
					"permalink": found.permalink,
				})
			})
			.collect();
		let paging = |field: &str| {
			body
				.pointer(&format!("/messages/paging/{field}"))
				.and_then(Value::as_u64)
		};
		Ok(json!({
			"query": query,
			"total": paging("total").unwrap_or(results.len() as u64),
			"page": paging("page").unwrap_or(1),
			"pages": paging("pages").unwrap_or(1),
			"count": results.len(),
			"results": results,
		}))
	}

	/// `slack_read`: a thread, or the latest messages of a channel,
	/// oldest first.
	pub async fn read(
		&self,
		target: &ReadTarget,
		limit: Option<u32>,
		cursor: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<Value, SlackError> {
		let limit = limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, MAX_READ_LIMIT).to_string();
		let cursor = cursor.map(str::trim).filter(|cursor| !cursor.is_empty());
		let (channel, body, thread) = match target {
			ReadTarget::Channel { channel } => {
				let mut params = vec![("channel", channel.as_str()), ("limit", limit.as_str())];
				params.extend(cursor.map(|cursor| ("cursor", cursor)));
				(
					channel,
					self.call("conversations.history", &params, cancel).await?,
					None,
				)
			}
			ReadTarget::Thread { channel, ts } => {
				let limit = limit.as_str();
				let fetch = |ts: String| async move {
					let mut params = vec![("channel", channel.as_str()), ("ts", ts.as_str()), ("limit", limit)];
					params.extend(cursor.map(|cursor| ("cursor", cursor)));
					self.call("conversations.replies", &params, cancel).await
				};
				let mut body = fetch(ts.clone()).await?;
				let mut root = ts.clone();
				// Asked by a reply's timestamp, Slack returns that one
				// message; it names the thread it belongs to.
				let parent = body
					.pointer("/messages/0/thread_ts")
					.and_then(Value::as_str)
					.filter(|parent| *parent != ts.as_str())
					.map(str::to_owned);
				if let Some(parent) = parent {
					body = fetch(parent.clone()).await?;
					root = parent;
				}
				(channel, body, Some(root))
			}
		};
		let mut messages: Vec<RawMessage> = body
			.get("messages")
			.cloned()
			.and_then(|value| serde_json::from_value(value).ok())
			.unwrap_or_default();

		let mut ids: Vec<&str> = Vec::new();
		for message in &messages {
			ids.extend(message.user.as_deref());
			ids.extend(mentioned_users(&message.text));
		}
		self.resolve_users(ids, cancel).await;
		let label = self.channel_label(channel, cancel).await;
		let names = self.known_names();

		// In the order Slack sent them: a channel comes newest first,
		// so running out of room drops its oldest messages; a thread
		// comes oldest first and loses its tail.
		let mut budget = MAX_READ_CHARS;
		let mut truncated = false;
		let mut out: Vec<Value> = Vec::with_capacity(messages.len());
		for message in messages.drain(..) {
			let text = clip(
				render_text(&full_text(&message.text, &message.attachments), &names),
				MAX_MESSAGE_CHARS,
			);
			if text.len() > budget {
				truncated = true;
				break;
			}
			budget -= text.len();
			let user = message
				.user
				.as_deref()
				.map(|id| names.get(id).cloned().unwrap_or_else(|| id.to_owned()))
				.or(message.username)
				.or(message.bot_profile.map(|bot| bot.name))
				.unwrap_or_else(|| "unknown".to_owned());
			let mut entry = json!({ "user": user, "time": time_of(&message.ts), "ts": message.ts, "text": text });
			// In a channel read, a count says there is a thread to
			// open; inside the thread it would repeat on the root.
			if thread.is_none() && message.reply_count > 0 {
				entry["replies"] = json!(message.reply_count);
				entry["thread_ts"] = json!(message.thread_ts.unwrap_or_else(|| message.ts.clone()));
			}
			let files: Vec<String> = message
				.files
				.into_iter()
				.filter_map(|file| file.title.or(file.name))
				.collect();
			if !files.is_empty() {
				entry["files"] = json!(files);
			}
			out.push(entry);
		}
		if thread.is_none() {
			out.reverse();
		}
		let next_cursor = body
			.pointer("/response_metadata/next_cursor")
			.and_then(Value::as_str)
			.filter(|cursor| !cursor.is_empty());
		let mut result = json!({
			"channel": label,
			"channel_id": channel,
			"kind": if thread.is_some() { "thread" } else { "channel" },
			"count": out.len(),
			"messages": out,
		});
		if let Some(root) = thread {
			result["thread_ts"] = json!(root);
		}
		if let Some(cursor) = next_cursor {
			result["next_cursor"] = json!(cursor);
		}
		if truncated {
			result["truncated"] = json!(true);
		}
		Ok(result)
	}
}

fn entry(account: &str) -> Result<keyring::Entry, keyring::Error> {
	keyring::Entry::new(KEYRING_SERVICE, account)
}

fn load_token(account: &str) -> Result<Option<String>, keyring::Error> {
	match entry(account)?.get_password() {
		Ok(token) if token.trim().is_empty() => Ok(None),
		Ok(token) => Ok(Some(token)),
		Err(keyring::Error::NoEntry) => Ok(None),
		Err(err) => Err(err),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn names(pairs: &[(&str, &str)]) -> HashMap<String, String> {
		pairs
			.iter()
			.map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
			.collect()
	}

	#[test]
	fn permalinks_channel_urls_and_ids_are_understood() {
		let thread = |channel: &str, ts: &str| ReadTarget::Thread {
			channel: channel.to_owned(),
			ts: ts.to_owned(),
		};
		let channel = |channel: &str| ReadTarget::Channel {
			channel: channel.to_owned(),
		};
		assert_eq!(
			parse_target("https://acme.slack.com/archives/C0123ABCDE/p1700000000123456", None),
			Ok(thread("C0123ABCDE", "1700000000.123456"))
		);
		// A reply's permalink reads the thread it is in.
		assert_eq!(
			parse_target(
				"https://acme.slack.com/archives/C0123ABCDE/p1700000500000200?thread_ts=1700000000.123456&cid=C0123ABCDE",
				None
			),
			Ok(thread("C0123ABCDE", "1700000000.123456"))
		);
		assert_eq!(
			parse_target("https://acme.slack.com/archives/C0123ABCDE", None),
			Ok(channel("C0123ABCDE"))
		);
		assert_eq!(
			parse_target("https://app.slack.com/client/T0123ABCD/G0123ABCDE", None),
			Ok(channel("G0123ABCDE"))
		);
		assert_eq!(parse_target(" D0123ABCDE ", None), Ok(channel("D0123ABCDE")));
		assert_eq!(
			parse_target("C0123ABCDE", Some("1700000000.123456")),
			Ok(thread("C0123ABCDE", "1700000000.123456"))
		);
	}

	#[test]
	fn other_targets_are_refused_with_a_hint() {
		assert!(parse_target("#general", None).unwrap_err().contains("in:#name"));
		assert!(parse_target("https://example.com/archives/nope", None).is_err());
		assert!(parse_target("https://acme.slack.com/files/U1/F1/x.png", None).is_err());
		assert!(parse_target("C0123ABCDE", Some("yesterday"))
			.unwrap_err()
			.contains("thread_ts"));
		assert!(parse_target("", None).is_err());
	}

	#[test]
	fn markup_becomes_plain_text() {
		let known = names(&[("U1", "alex"), ("C9", "ops")]);
		assert_eq!(
			render_text("<@U1> and <@U2|sam>: see <#C1|general>, <#C9> &amp; <#C7>", &known),
			"@alex and @sam: see #general, #ops & #C7"
		);
		assert_eq!(
			render_text("<https://x.test/a|the doc> or <https://x.test/b>", &known),
			"the doc (https://x.test/a) or https://x.test/b"
		);
		assert_eq!(
			render_text(
				"<!here> <!channel> <!subteam^S1|@infra> <!date^1700000000^{date}|Nov 14>",
				&known
			),
			"@here @channel @infra Nov 14"
		);
		assert_eq!(
			render_text("if a &lt; b &amp;&amp; c &gt; d", &known),
			"if a < b && c > d"
		);
	}

	#[test]
	fn broken_markup_is_left_as_text() {
		let known = HashMap::new();
		assert_eq!(render_text("a < b", &known), "a < b");
		assert_eq!(render_text("x <not\nclosed> y", &known), "x <not\nclosed> y");
		assert_eq!(render_text("&amp;lt;", &known), "&lt;");
	}

	#[test]
	fn only_unlabelled_user_mentions_need_a_lookup() {
		assert_eq!(mentioned_users("<@U1> <@U2|sam> <#C1> <@W3> <@U4"), ["U1", "W3"]);
	}

	#[test]
	fn attachments_fill_in_an_empty_bot_message() {
		let attachment = |title: Option<&str>, text: Option<&str>, fallback: Option<&str>| RawAttachment {
			title: title.map(str::to_owned),
			text: text.map(str::to_owned),
			fallback: fallback.map(str::to_owned),
		};
		assert_eq!(
			full_text(
				"",
				&[attachment(Some("Deploy failed"), Some("exit 1"), Some("ignored"))]
			),
			"Deploy failed\nexit 1"
		);
		assert_eq!(
			full_text("hello", &[attachment(None, None, Some("a preview"))]),
			"hello\na preview"
		);
		// An unfurl that repeats the message adds nothing.
		assert_eq!(
			full_text("see exit 1", &[attachment(None, Some("exit 1"), None)]),
			"see exit 1"
		);
		assert_eq!(full_text("plain", &[]), "plain");
	}

	#[test]
	fn long_text_is_cut_on_a_character_boundary() {
		assert_eq!(clip("héllo".to_owned(), 2), "hé… [cut]");
		assert_eq!(clip("short".to_owned(), 10), "short");
	}

	#[test]
	fn timestamps_read_as_utc() {
		assert_eq!(time_of("1700000000.123456"), "2023-11-14T22:13:20Z");
		assert_eq!(time_of("garbage"), "garbage");
	}

	#[test]
	fn api_errors_say_what_to_do() {
		let missing = describe_api_error("conversations.history", "missing_scope", Some("channels:history"));
		assert!(
			missing.contains("`channels:history`") && missing.contains("reinstall"),
			"{missing}"
		);
		assert!(describe_api_error("search.messages", "not_allowed_token_type", None).contains("user token"));
		assert!(describe_api_error("auth.test", "invalid_auth", None).contains("Reconnect"));
		assert_eq!(
			describe_api_error("users.info", "user_not_found", None),
			"Slack API error on users.info: user_not_found"
		);
	}

	type Requests = Arc<std::sync::Mutex<Vec<String>>>;

	/// A fake Web API: `reply(method, query)` is the status line, extra
	/// headers and JSON body for each request. Returns a client that
	/// talks to it with the token `xoxp-test`, and the request heads
	/// it saw.
	async fn fake_slack(reply: fn(&str, &str) -> (&'static str, &'static str, Value)) -> (SlackClient, Requests) {
		use tokio::io::{AsyncReadExt, AsyncWriteExt};
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		let requests: Requests = Arc::default();
		let seen = requests.clone();
		tokio::spawn(async move {
			loop {
				let Ok((mut sock, _)) = listener.accept().await else {
					return;
				};
				let seen = seen.clone();
				tokio::spawn(async move {
					let mut buf = Vec::new();
					let mut chunk = [0u8; 8192];
					while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
						let n = sock.read(&mut chunk).await.unwrap();
						if n == 0 {
							return;
						}
						buf.extend_from_slice(&chunk[..n]);
					}
					let head = String::from_utf8_lossy(&buf).into_owned();
					let target = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
					let (path, query) = target.split_once('?').unwrap_or((&target, ""));
					let (status, extra, body) = reply(path.trim_start_matches('/'), query);
					seen.lock().unwrap().push(head);
					let body = body.to_string();
					let response = format!(
						"HTTP/1.1 {status}\r\n{extra}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
						body.len()
					);
					sock.write_all(response.as_bytes()).await.unwrap();
				});
			}
		});
		let client = SlackClient {
			base,
			token: Arc::new(RwLock::new(Some("xoxp-test".to_owned()))),
			..SlackClient::for_tests()
		};
		(client, requests)
	}

	fn ok(body: Value) -> (&'static str, &'static str, Value) {
		("200 OK", "", body)
	}

	fn user_reply(query: &str) -> Value {
		let name = if query.contains("user=U1") { "alex" } else { "sam" };
		json!({ "ok": true, "user": { "name": "x", "real_name": "Real Name", "profile": { "display_name": name } } })
	}

	#[tokio::test]
	async fn search_shapes_matches_and_only_ever_reads() {
		let (client, requests) = fake_slack(|method, query| match method {
			"search.messages" => ok(json!({
				"ok": true,
				"messages": {
					"paging": { "count": 20, "total": 41, "page": 1, "pages": 3 },
					"matches": [
						{
							"channel": { "id": "C0123ABCDE", "name": "infra", "is_im": false },
							"user": "U1", "username": "alex", "ts": "1700000000.123456",
							"text": format!("ping <@U2> about <#C9|ops> {}", "x".repeat(MAX_MATCH_CHARS)),
							"permalink": "https://acme.slack.com/archives/C0123ABCDE/p1700000000123456"
						},
						{
							"channel": { "id": "D0123ABCDE", "name": "U2", "is_im": true },
							"user": "U1", "username": "", "ts": "1700000100.000000",
							"text": "", "attachments": [{ "fallback": "from a bot" }],
							"permalink": "https://acme.slack.com/archives/D0123ABCDE/p1700000100000000"
						}
					]
				}
			})),
			"users.info" => ok(user_reply(query)),
			other => panic!("unexpected call to {other}"),
		})
		.await;

		let result = client
			.search("  deploy failed  ", Some(500), None, true, &CancellationToken::new())
			.await
			.unwrap();

		assert_eq!(
			(
				result["total"].as_u64(),
				result["pages"].as_u64(),
				result["count"].as_u64()
			),
			(Some(41), Some(3), Some(2))
		);
		let first = &result["results"][0];
		assert_eq!(
			(first["channel"].as_str(), first["user"].as_str()),
			(Some("#infra"), Some("alex"))
		);
		assert_eq!(first["time"], "2023-11-14T22:13:20Z");
		let text = first["text"].as_str().unwrap();
		assert!(
			text.starts_with("ping @sam about #ops x") && text.ends_with("… [cut]"),
			"{text}"
		);
		let second = &result["results"][1];
		assert_eq!(second["channel"], "DM with @sam");
		assert_eq!(
			(second["user"].as_str(), second["text"].as_str()),
			(Some("alex"), Some("from a bot"))
		);

		let requests = requests.lock().unwrap();
		let search = requests.iter().find(|head| head.contains("search.messages")).unwrap();
		assert!(
			search.starts_with("GET /search.messages?query=deploy+failed&count=50&page=1&highlight=false&sort=timestamp "),
			"{search}"
		);
		assert!(
			search.to_lowercase().contains("authorization: bearer xoxp-test"),
			"{search}"
		);
		// Nothing but reads, and each unknown user is asked about once.
		assert!(requests.iter().all(|head| head.starts_with("GET ")));
		assert_eq!(requests.iter().filter(|head| head.contains("users.info")).count(), 2);
	}

	#[tokio::test]
	async fn reading_by_a_replys_timestamp_returns_its_whole_thread() {
		let (client, requests) = fake_slack(|method, query| match method {
			"conversations.replies" if query.contains("ts=1700000500.000200") => ok(json!({
				"ok": true,
				"messages": [{ "user": "U2", "ts": "1700000500.000200", "thread_ts": "1700000000.123456", "text": "a reply" }]
			})),
			"conversations.replies" => ok(json!({
				"ok": true,
				"messages": [
					{ "user": "U1", "ts": "1700000000.123456", "thread_ts": "1700000000.123456", "text": "the question", "reply_count": 2 },
					{ "bot_profile": { "name": "deploybot" }, "ts": "1700000300.000100", "thread_ts": "1700000000.123456", "text": "", "attachments": [{ "title": "Deploy", "text": "failed" }] },
					{ "user": "U2", "ts": "1700000500.000200", "thread_ts": "1700000000.123456", "text": "a reply", "files": [{ "name": "log.txt" }] }
				],
				"response_metadata": { "next_cursor": "" }
			})),
			"conversations.info" => ok(json!({ "ok": true, "channel": { "id": "C0123ABCDE", "name": "infra" } })),
			"users.info" => ok(user_reply(query)),
			other => panic!("unexpected call to {other}"),
		})
		.await;
		let target = parse_target("C0123ABCDE", Some("1700000500.000200")).unwrap();

		let result = client
			.read(&target, None, None, &CancellationToken::new())
			.await
			.unwrap();

		assert_eq!(
			(result["kind"].as_str(), result["channel"].as_str()),
			(Some("thread"), Some("#infra"))
		);
		assert_eq!(result["thread_ts"], "1700000000.123456");
		let seen: Vec<_> = result["messages"]
			.as_array()
			.unwrap()
			.iter()
			.map(|m| (m["user"].as_str().unwrap(), m["text"].as_str().unwrap()))
			.collect();
		assert_eq!(
			seen,
			[
				("alex", "the question"),
				("deploybot", "Deploy\nfailed"),
				("sam", "a reply")
			]
		);
		assert_eq!(result["messages"][2]["files"], json!(["log.txt"]));
		// Inside a thread the root carries no "open me" hint.
		assert!(result["messages"][0].get("replies").is_none());
		assert!(result.get("next_cursor").is_none());
		let replies = requests
			.lock()
			.unwrap()
			.iter()
			.filter(|head| head.contains("conversations.replies"))
			.count();
		assert_eq!(replies, 2);
	}

	#[tokio::test]
	async fn a_channel_reads_oldest_first_and_points_at_its_threads() {
		let (client, requests) = fake_slack(|method, _| match method {
			"conversations.history" => ok(json!({
				"ok": true,
				"messages": [
					{ "user": "U1", "ts": "1700000900.000000", "text": "newest" },
					{ "user": "U1", "ts": "1700000000.123456", "thread_ts": "1700000000.123456", "text": "older, with a thread", "reply_count": 4 }
				],
				"has_more": true,
				"response_metadata": { "next_cursor": "bmV4dA==" }
			})),
			"conversations.info" => ok(json!({ "ok": true, "channel": { "id": "D0123ABCDE", "is_im": true, "user": "U2" } })),
			"users.info" => ok(json!({ "ok": true, "user": { "name": "handle", "profile": { "display_name": "" } } })),
			other => panic!("unexpected call to {other}"),
		})
		.await;
		let target = parse_target("D0123ABCDE", None).unwrap();

		let result = client
			.read(&target, Some(2), Some("Y3Vyc29y"), &CancellationToken::new())
			.await
			.unwrap();

		assert_eq!(
			(result["kind"].as_str(), result["channel"].as_str()),
			(Some("channel"), Some("DM with @handle"))
		);
		assert_eq!(result["messages"][0]["text"], "older, with a thread");
		assert_eq!(
			(
				result["messages"][0]["replies"].as_u64(),
				result["messages"][0]["thread_ts"].as_str()
			),
			(Some(4), Some("1700000000.123456"))
		);
		assert_eq!(result["messages"][1]["text"], "newest");
		assert_eq!(result["next_cursor"], "bmV4dA==");
		let requests = requests.lock().unwrap();
		let history = requests
			.iter()
			.find(|head| head.contains("conversations.history"))
			.unwrap();
		assert!(
			history.starts_with("GET /conversations.history?channel=D0123ABCDE&limit=2&cursor=Y3Vyc29y "),
			"{history}"
		);
	}

	#[tokio::test]
	async fn failures_come_back_as_something_to_act_on() {
		let (client, _) = fake_slack(|method, _| match method {
			"search.messages" => ok(json!({ "ok": false, "error": "missing_scope", "needed": "search:read" })),
			"conversations.history" => (
				"429 Too Many Requests",
				"retry-after: 30\r\n",
				json!({ "ok": false, "error": "ratelimited" }),
			),
			_ => ("500 Internal Server Error", "", json!({})),
		})
		.await;
		let cancel = CancellationToken::new();

		let scope = client
			.search("x", None, None, false, &cancel)
			.await
			.unwrap_err()
			.to_string();
		assert!(scope.contains("`search:read`"), "{scope}");
		let limited = client
			.read(&parse_target("C0123ABCDE", None).unwrap(), None, None, &cancel)
			.await
			.unwrap_err()
			.to_string();
		assert!(
			limited.contains("rate limiting") && limited.contains("30 seconds"),
			"{limited}"
		);
		let broken = client
			.read(
				&parse_target("C0123ABCDE", Some("1700000000.123456")).unwrap(),
				None,
				None,
				&cancel,
			)
			.await
			.unwrap_err();
		assert_eq!(
			broken.clone().into_tool("slack_read").to_string(),
			format!("tool slack_read failed: {broken}")
		);

		let nobody = SlackClient::for_tests();
		assert!(!nobody.is_connected());
		let unconnected = nobody
			.search("x", None, None, false, &cancel)
			.await
			.unwrap_err()
			.to_string();
		assert!(
			unconnected.contains("not connected for this workspace"),
			"{unconnected}"
		);
		assert_eq!(nobody.connection().await, Ok(None));

		cancel.cancel();
		assert_eq!(
			client.search("x", None, None, false, &cancel).await,
			Err(SlackError::Aborted)
		);
	}

	#[tokio::test]
	async fn a_long_read_stops_at_its_budget() {
		let (client, _) = fake_slack(|method, _| match method {
			"conversations.history" => {
				let messages: Vec<Value> = (0..40)
					.map(
						|i| json!({ "ts": format!("17000{i:05}.000000"), "username": "bot", "text": "y".repeat(MAX_MESSAGE_CHARS) }),
					)
					.collect();
				ok(json!({ "ok": true, "messages": messages }))
			}
			_ => ok(json!({ "ok": false, "error": "channel_not_found" })),
		})
		.await;

		let result = client
			.read(
				&parse_target("C0123ABCDE", None).unwrap(),
				Some(40),
				None,
				&CancellationToken::new(),
			)
			.await
			.unwrap();

		assert_eq!(result["truncated"], true);
		let kept = result["messages"].as_array().unwrap();
		assert!(
			kept.len() < 40 && kept.len() >= MAX_READ_CHARS / (MAX_MESSAGE_CHARS + 16),
			"{}",
			kept.len()
		);
		// Newest first on the wire, so the newest survive, oldest first.
		assert_eq!(kept.last().unwrap()["ts"], "1700000000.000000");
		// The channel's name couldn't be read: its id stands in.
		assert_eq!(result["channel"], "C0123ABCDE");
	}

	#[test]
	fn the_scopes_are_read_only() {
		for scope in SCOPES {
			assert!(
				scope.ends_with(":read") || scope.ends_with(":history"),
				"{scope} is not a read scope"
			);
		}
	}
}
