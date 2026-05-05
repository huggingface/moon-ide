//! HF Inference Providers HTTP client (non-streaming).
//!
//! OpenAI-compatible API surface against
//! `https://router.huggingface.co/v1`. Authentication uses the OAuth
//! access token from [`crate::auth::Authenticator`]; the client wraps
//! its own `reqwest::Client` and refreshes-on-401 automatically.
//!
//! Streaming + provider routing knobs land in 6.1.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::auth::Authenticator;
use crate::defaults::HF_ROUTER_BASE;
use crate::error::CoderError;

/// One message in the conversation, in the OpenAI chat-completions
/// shape. We keep the wire shape verbatim so the router doesn't need
/// adapter code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
	System {
		content: String,
	},
	User {
		content: String,
	},
	Assistant {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		content: Option<String>,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		tool_calls: Vec<ToolCall>,
	},
	Tool {
		tool_call_id: String,
		content: String,
	},
}

/// One tool call the model emitted. The `function.arguments` field
/// is a JSON-encoded string (this is OpenAI's wire convention; the
/// router preserves it). Callers must `serde_json::from_str` the
/// string before treating it as structured data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
	pub id: String,
	#[serde(rename = "type", default = "default_tool_type")]
	pub kind: String,
	pub function: FunctionCall,
}

fn default_tool_type() -> String {
	"function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
	pub name: String,
	pub arguments: String,
}

/// Tool definition handed to the model in the request. Mirrors
/// OpenAI's `{ "type": "function", "function": { ... } }` shape.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
	#[serde(rename = "type")]
	pub kind: &'static str,
	pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
	pub name: String,
	pub description: String,
	pub parameters: serde_json::Value,
}

impl ToolDefinition {
	pub fn function(name: impl Into<String>, description: impl Into<String>, parameters: serde_json::Value) -> Self {
		Self {
			kind: "function",
			function: FunctionDef {
				name: name.into(),
				description: description.into(),
				parameters,
			},
		}
	}
}

#[derive(Debug, Clone, Serialize)]
struct ChatCompletionRequest<'a> {
	model: &'a str,
	messages: &'a [ChatMessage],
	#[serde(skip_serializing_if = "<[ToolDefinition]>::is_empty")]
	tools: &'a [ToolDefinition],
	#[serde(skip_serializing_if = "Option::is_none")]
	tool_choice: Option<&'static str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
	pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
	pub message: AssistantResponse,
	#[serde(default)]
	pub finish_reason: Option<String>,
}

/// Slimmed view of `choices[].message`. Different from
/// [`ChatMessage::Assistant`] because the wire shape uses a flat
/// `role` field that we don't echo back; this keeps deserialisation
/// simple without coupling to the input enum's tagging.
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantResponse {
	#[serde(default)]
	pub content: Option<String>,
	#[serde(default)]
	pub tool_calls: Vec<ToolCall>,
}

/// Inference HTTP client. Cheap to clone; the underlying
/// `reqwest::Client` does its own connection pooling.
#[derive(Clone)]
pub struct InferenceClient {
	http: reqwest::Client,
	auth: Authenticator,
	base_url: String,
}

impl InferenceClient {
	pub fn new(auth: Authenticator) -> Result<Self, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(CoderError::from)?;
		Ok(Self {
			http,
			auth,
			base_url: HF_ROUTER_BASE.to_string(),
		})
	}

	pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
		self.base_url = base_url.into();
		self
	}

	/// One non-streaming chat-completions round trip.
	///
	/// Auto-refresh on 401: the first response that comes back as
	/// `Unauthorized` triggers a refresh-then-retry; the second 401
	/// surfaces as `NotSignedIn` to force the panel back into the
	/// device-flow modal.
	pub async fn chat_completion(
		&self,
		model: &str,
		messages: &[ChatMessage],
		tools: &[ToolDefinition],
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<AssistantResponse, CoderError> {
		let endpoint = format!("{}/chat/completions", self.base_url);
		let body = ChatCompletionRequest {
			model,
			messages,
			tools,
			tool_choice: if tools.is_empty() { None } else { Some("auto") },
		};

		let access = self.auth.current_access_token().await?;
		let mut response = self.send_once(&endpoint, &access, &body, cancel).await?;

		if response.status() == reqwest::StatusCode::UNAUTHORIZED {
			tracing::info!("inference returned 401; refreshing token and retrying once");
			let refreshed = self.auth.refresh_now().await?;
			response = self.send_once(&endpoint, &refreshed, &body, cancel).await?;
		}

		let status = response.status();
		let recv = response.text();
		let text = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			out = recv => out.map_err(CoderError::from)?,
		};
		if !status.is_success() {
			return Err(CoderError::http(endpoint, status.as_u16(), text));
		}

		let parsed: ChatCompletionResponse = crate::auth::decode_body(&endpoint, &text)?;
		parsed
			.choices
			.into_iter()
			.next()
			.map(|c| c.message)
			.ok_or_else(|| CoderError::decode(&endpoint, "response had no choices"))
	}

	async fn send_once(
		&self,
		endpoint: &str,
		access_token: &str,
		body: &ChatCompletionRequest<'_>,
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, CoderError> {
		let send = self.http.post(endpoint).bearer_auth(access_token).json(body).send();
		tokio::select! {
			biased;
			_ = cancel.cancelled() => Err(CoderError::Aborted),
			resp = send => resp.map_err(CoderError::from),
		}
	}
}

/// Convenience wrapper used by the runner so the type can be dropped
/// through `Arc<...>` without dragging the auth handle along
/// separately.
pub type SharedInference = Arc<InferenceClient>;
