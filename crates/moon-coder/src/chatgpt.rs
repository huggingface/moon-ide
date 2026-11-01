//! ChatGPT subscription route (`kind=chat_gpt`, ADR 0101): the Codex
//! backend's OpenAI Responses endpoint.
//!
//! How it differs from the chat-completions path:
//!
//! - **Auth:** an OAuth access token plus `chatgpt-account-id`, from
//!   [`ChatGptAuth`]. A 401 forces one refresh and one resend.
//! - **Shape:** the system prompt goes in `instructions`, history goes
//!   in `input` as typed items (`message` / `function_call` /
//!   `function_call_output` / `reasoning`), and tools are flat
//!   `{type, name, description, parameters}`.
//! - **Stateless:** `store: false` is mandatory, so the encrypted
//!   reasoning comes back via `include` and is replayed from
//!   [`ThinkingBlock::ResponsesOutput`] on the next request.
//! - **Streaming only:** the backend rejects `stream: false` and any
//!   output-token cap, so the non-streaming entry point streams too.
//! - **Bodies are zstd-compressed:** the backend rejects
//!   uncompressed bodies past about 1 MB.
//! - **Plan limits:** a usage-limit 429 is not retried, because the
//!   window won't reset within any backoff we'd sit through.

use std::collections::BTreeMap;

use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::chatgpt_auth::{ChatGptAuth, ChatGptCredentials};
use crate::error::{request_id_of, CoderError};
use crate::inference::{
	extract_data_lines, find_event_boundary, normalize_stop_reason, rate_limit_delay, retry_after_of,
	session_affinity_uuid, truncate_for_log, AssistantResponse, ChatMessage, FunctionCall, ImageAttachment,
	ResolvedRoute, StreamEvent, ThinkingBlock, TokenUsage, ToolCall, ToolDefinition, RATE_LIMIT_MAX_RETRIES,
};
use moon_protocol::coder_models::ProviderModelSummary;

/// Sent on every request. The backend requires an originator; it is
/// not an allow-list (OpenCode and Pi send their own names).
const ORIGINATOR: &str = "moon-ide";
/// The backend filters its model catalog by client version. Recent
/// enough to list every model the Codex CLI's bundled catalog lists.
const CODEX_CLIENT_VERSION: &str = "0.155.0";
const ZSTD_LEVEL: i32 = 3;

/// Everything one request needs beyond the messages.
pub(crate) struct ChatGptRequest<'a> {
	pub(crate) http: &'a reqwest::Client,
	pub(crate) auth: &'a ChatGptAuth,
	pub(crate) route: &'a ResolvedRoute,
	pub(crate) provider_id: &'a str,
	pub(crate) model: &'a str,
	pub(crate) reasoning_effort: Option<String>,
}

pub(crate) async fn chat_completion(
	req: ChatGptRequest<'_>,
	messages: &[ChatMessage],
	tools: &[ToolDefinition],
	cancel: &tokio_util::sync::CancellationToken,
) -> Result<AssistantResponse, CoderError> {
	chat_completion_stream(req, messages, tools, cancel, |_| {}).await
}

pub(crate) async fn chat_completion_stream<F>(
	req: ChatGptRequest<'_>,
	messages: &[ChatMessage],
	tools: &[ToolDefinition],
	cancel: &tokio_util::sync::CancellationToken,
	mut on_event: F,
) -> Result<AssistantResponse, CoderError>
where
	F: FnMut(StreamEvent<'_>),
{
	let endpoint = format!("{}/responses", req.route.base_url);
	let body = build_body(req.model, messages, tools, req.reasoning_effort.as_deref());
	let raw = serde_json::to_vec(&body).map_err(|err| CoderError::Internal(err.to_string()))?;
	let compressed = zstd::bulk::compress(&raw, ZSTD_LEVEL).map_err(|err| CoderError::Internal(err.to_string()))?;

	let mut creds = abortable(cancel, req.auth.credentials(req.http, req.provider_id, false)).await?;
	let mut refreshed = false;
	let mut attempt: u32 = 0;
	let response = loop {
		let send = responses_builder(req.http, &endpoint, &creds)
			.header(reqwest::header::CONTENT_ENCODING, "zstd")
			.body(compressed.clone())
			.send();
		let response = abortable(cancel, async { send.await.map_err(CoderError::from) }).await?;
		let status = response.status();
		if status.is_success() {
			break response;
		}
		if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
			tracing::info!("ChatGPT backend returned 401; refreshing token and retrying once");
			refreshed = true;
			creds = abortable(cancel, req.auth.credentials(req.http, req.provider_id, true)).await?;
			continue;
		}
		let retry_after = retry_after_of(&response);
		let request_id = request_id_of(&response);
		let text = abortable(cancel, async { response.text().await.map_err(CoderError::from) }).await?;
		if let Some(message) = plan_limit_message(status, &text) {
			return Err(CoderError::http(endpoint, status.as_u16(), message, request_id));
		}
		if !is_retryable(status) || attempt >= RATE_LIMIT_MAX_RETRIES {
			return Err(CoderError::http(endpoint, status.as_u16(), text, request_id));
		}
		let delay = rate_limit_delay(attempt, retry_after);
		attempt += 1;
		tracing::warn!(status = %status, attempt, delay_s = delay.as_secs(), "ChatGPT backend busy; backing off");
		crate::runner::emit_turn_event(crate::CoderEvent::RetryBackoff {
			model: req.model.to_owned(),
			status: status.as_u16(),
			attempt,
			max_attempts: RATE_LIMIT_MAX_RETRIES,
			delay_ms: delay.as_millis() as u64,
			rotated_to: None,
		});
		abortable(cancel, async {
			tokio::time::sleep(delay).await;
			Ok(())
		})
		.await?;
	};
	consume_stream(response, req.model, cancel, &mut on_event).await
}

/// `GET <base>/models`: the plan's model catalog, minus hidden entries.
pub(crate) async fn list_models(
	http: &reqwest::Client,
	auth: &ChatGptAuth,
	provider_id: &str,
	base_url: &str,
) -> Result<Vec<ProviderModelSummary>, CoderError> {
	#[derive(Deserialize)]
	struct Catalog {
		#[serde(default)]
		models: Vec<CatalogModel>,
	}
	#[derive(Deserialize)]
	struct CatalogModel {
		slug: String,
		#[serde(default)]
		display_name: Option<String>,
		#[serde(default)]
		description: Option<String>,
		#[serde(default)]
		context_window: Option<u32>,
		#[serde(default)]
		visibility: Option<String>,
		#[serde(default)]
		input_modalities: Option<Vec<String>>,
		#[serde(default)]
		supported_reasoning_levels: Option<Vec<ReasoningLevel>>,
		#[serde(default)]
		default_reasoning_level: Option<String>,
	}
	#[derive(Deserialize)]
	struct ReasoningLevel {
		effort: String,
	}
	let endpoint = format!(
		"{}/models?client_version={CODEX_CLIENT_VERSION}",
		base_url.trim_end_matches('/')
	);
	let creds = auth.credentials(http, provider_id, false).await?;
	let response = request_builder(http, reqwest::Method::GET, &endpoint, &creds)
		.timeout(std::time::Duration::from_secs(30))
		.send()
		.await
		.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}
	let catalog: Catalog = crate::auth::decode_body(&endpoint, &body)?;
	Ok(
		catalog
			.models
			.into_iter()
			.filter(|m| m.visibility.as_deref() != Some("hide"))
			.map(|m| ProviderModelSummary {
				id: m.slug,
				owned_by: Some("openai".into()),
				name: m.display_name,
				context_length: m.context_window,
				pricing_in_per_million: None,
				pricing_out_per_million: None,
				supports_image_input: m.input_modalities.map(|mods| mods.iter().any(|x| x == "image")),
				description: m.description,
				reasoning_levels: m
					.supported_reasoning_levels
					.map(|levels| levels.into_iter().map(|l| l.effort).collect()),
				default_reasoning_level: m.default_reasoning_level,
			})
			.collect(),
	)
}

fn request_builder(
	http: &reqwest::Client,
	method: reqwest::Method,
	endpoint: &str,
	creds: &ChatGptCredentials,
) -> reqwest::RequestBuilder {
	http
		.request(method, endpoint)
		.bearer_auth(&creds.access_token)
		.header("chatgpt-account-id", &creds.account_id)
		.header("originator", ORIGINATOR)
}

fn responses_builder(http: &reqwest::Client, endpoint: &str, creds: &ChatGptCredentials) -> reqwest::RequestBuilder {
	let builder = request_builder(http, reqwest::Method::POST, endpoint, creds)
		.header("OpenAI-Beta", "responses=experimental")
		.header(reqwest::header::ACCEPT, "text/event-stream")
		.header(reqwest::header::CONTENT_TYPE, "application/json");
	let Some(session) = session_affinity_uuid() else {
		return builder;
	};
	builder
		.header("session-id", session.as_str())
		.header("x-client-request-id", session.as_str())
}

async fn abortable<T>(
	cancel: &tokio_util::sync::CancellationToken,
	fut: impl std::future::Future<Output = Result<T, CoderError>>,
) -> Result<T, CoderError> {
	tokio::select! {
		biased;
		_ = cancel.cancelled() => Err(CoderError::Aborted),
		out = fut => out,
	}
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
	status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// A human message for a plan-quota error (not worth retrying), or
/// `None` for anything else.
fn plan_limit_message(status: reqwest::StatusCode, body: &str) -> Option<String> {
	if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
		return None;
	}
	let parsed: Value = serde_json::from_str(body).ok()?;
	let err = parsed.get("error")?;
	let code = err
		.get("type")
		.or_else(|| err.get("code"))
		.and_then(Value::as_str)
		.unwrap_or_default();
	let plan = err
		.get("plan_type")
		.and_then(Value::as_str)
		.map(|p| format!(" ({} plan)", p.to_lowercase()))
		.unwrap_or_default();
	match code {
		"usage_limit_reached" => {
			let when = err
				.get("resets_at")
				.and_then(Value::as_u64)
				.map(|at| {
					let now = std::time::SystemTime::now()
						.duration_since(std::time::UNIX_EPOCH)
						.map(|d| d.as_secs())
						.unwrap_or(0);
					let mins = at.saturating_sub(now).div_ceil(60);
					format!(" Resets in about {mins} min.")
				})
				.unwrap_or_default();
			Some(format!("ChatGPT usage limit reached{plan}.{when}"))
		}
		"usage_not_included" => Some(format!("This ChatGPT plan{plan} does not include Codex usage.")),
		"insufficient_quota" => Some(format!("ChatGPT quota exhausted{plan}.")),
		_ => None,
	}
}

fn build_body(model: &str, messages: &[ChatMessage], tools: &[ToolDefinition], effort: Option<&str>) -> Value {
	let (instructions, input) = translate(messages, model);
	let mut body = json!({
		"model": model,
		"store": false,
		"stream": true,
		"instructions": instructions,
		"input": input,
		"include": ["reasoning.encrypted_content"],
		"parallel_tool_calls": true,
		"reasoning": { "summary": "auto" },
	});
	if let Some(effort) = effort.map(str::trim).filter(|e| !e.is_empty()) {
		body["reasoning"]["effort"] = Value::String(effort.to_owned());
	}
	if let Some(session) = session_affinity_uuid() {
		body["prompt_cache_key"] = Value::String(session);
	}
	if !tools.is_empty() {
		body["tool_choice"] = Value::String("auto".into());
		body["tools"] = Value::Array(
			tools
				.iter()
				.map(|t| {
					json!({
						"type": "function",
						"name": t.function.name,
						"description": t.function.description,
						"parameters": t.function.parameters,
					})
				})
				.collect(),
		);
	}
	body
}

/// History → `(instructions, input)`. The first system message becomes
/// `instructions`; later ones go in as `developer` messages.
fn translate(messages: &[ChatMessage], model: &str) -> (String, Vec<Value>) {
	let mut instructions = String::new();
	let mut input = Vec::with_capacity(messages.len());
	for (index, message) in messages.iter().enumerate() {
		match message {
			ChatMessage::System { content } => {
				if index == 0 {
					instructions = content.clone();
				} else if !content.trim().is_empty() {
					input.push(json!({ "role": "developer", "content": content }));
				}
			}
			ChatMessage::User { content, images } => {
				let mut parts = Vec::with_capacity(images.len() + 1);
				if !content.is_empty() {
					parts.push(json!({ "type": "input_text", "text": content }));
				}
				parts.extend(images.iter().map(input_image));
				if !parts.is_empty() {
					input.push(json!({ "role": "user", "content": parts }));
				}
			}
			ChatMessage::Assistant {
				content,
				thinking_blocks,
				tool_calls,
			} => push_assistant(&mut input, model, content.as_deref(), thinking_blocks, tool_calls),
			ChatMessage::Tool {
				tool_call_id,
				content,
				images,
				..
			} => {
				let output = if images.is_empty() {
					Value::String(content.clone())
				} else {
					let mut parts = Vec::with_capacity(images.len() + 1);
					if !content.is_empty() {
						parts.push(json!({ "type": "input_text", "text": content }));
					}
					parts.extend(images.iter().map(input_image));
					Value::Array(parts)
				};
				input.push(json!({
					"type": "function_call_output",
					"call_id": tool_call_id,
					"output": output,
				}));
			}
		}
	}
	if instructions.trim().is_empty() {
		instructions = "You are a helpful assistant.".into();
	}
	(instructions, input)
}

fn input_image(image: &ImageAttachment) -> Value {
	json!({ "type": "input_image", "detail": "auto", "image_url": image.data_url })
}

/// One assistant turn. When the turn came from this same model, its
/// stored output items are replayed in their original order (reasoning
/// verbatim, message and call items with their original ids), with the
/// text and calls taken from the history so edits by the runner (arg
/// repair, id dedupe) win. Anything not covered is appended id-less.
fn push_assistant(
	input: &mut Vec<Value>,
	model: &str,
	content: Option<&str>,
	thinking_blocks: &[ThinkingBlock],
	tool_calls: &[ToolCall],
) {
	let text = content.filter(|t| !t.trim().is_empty());
	let mut text_sent = false;
	let mut calls_sent = vec![false; tool_calls.len()];
	let stored = thinking_blocks.iter().find_map(|b| match b {
		ThinkingBlock::ResponsesOutput { model: m, items } if m == model => Some(items),
		_ => None,
	});
	for item in stored.into_iter().flatten() {
		match item.get("type").and_then(Value::as_str) {
			Some("reasoning") => input.push(item.clone()),
			Some("message") => {
				let Some(text) = text.filter(|_| !text_sent) else {
					continue;
				};
				text_sent = true;
				input.push(json!({
					"type": "message",
					"role": "assistant",
					"id": item.get("id"),
					"status": "completed",
					"content": [{ "type": "output_text", "text": text, "annotations": [] }],
				}));
			}
			Some("function_call") => {
				let call_id = item.get("call_id").and_then(Value::as_str);
				let Some(pos) = tool_calls
					.iter()
					.enumerate()
					.position(|(i, c)| !calls_sent[i] && Some(c.id.as_str()) == call_id)
				else {
					continue;
				};
				calls_sent[pos] = true;
				input.push(function_call_item(&tool_calls[pos], item.get("id")));
			}
			_ => {}
		}
	}
	if let Some(text) = text.filter(|_| !text_sent) {
		input.push(json!({ "role": "assistant", "content": text }));
	}
	for (call, _) in tool_calls.iter().zip(&calls_sent).filter(|(_, sent)| !**sent) {
		input.push(function_call_item(call, None));
	}
}

fn function_call_item(call: &ToolCall, id: Option<&Value>) -> Value {
	let mut item = json!({
		"type": "function_call",
		"call_id": call.id,
		"name": call.function.name,
		"arguments": if call.function.arguments.trim().is_empty() { "{}" } else { call.function.arguments.as_str() },
	});
	if let Some(id) = id.filter(|v| v.is_string()) {
		item["id"] = id.clone();
	}
	item
}

async fn consume_stream<F>(
	response: reqwest::Response,
	model: &str,
	cancel: &tokio_util::sync::CancellationToken,
	on_event: &mut F,
) -> Result<AssistantResponse, CoderError>
where
	F: FnMut(StreamEvent<'_>),
{
	let mut state = StreamState::default();
	let mut byte_stream = response.bytes_stream();
	let mut sse_buf: Vec<u8> = Vec::new();
	loop {
		let next = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			chunk = byte_stream.next() => chunk,
		};
		let Some(chunk) = next else {
			break;
		};
		sse_buf.extend_from_slice(&chunk.map_err(CoderError::from)?);
		while let Some(end) = find_event_boundary(&sse_buf) {
			let event_bytes = sse_buf.drain(..end.boundary_end).collect::<Vec<u8>>();
			let event_text = std::str::from_utf8(&event_bytes[..end.body_end])
				.map_err(|err| CoderError::decode("chatgpt stream", format!("invalid utf-8 in SSE event: {err}")))?;
			for data in extract_data_lines(event_text) {
				if data.trim() == "[DONE]" {
					continue;
				}
				let event: Value = serde_json::from_str(data).map_err(|err| {
					CoderError::decode(
						"chatgpt stream",
						format!("could not parse SSE event: {err}; raw={}", truncate_for_log(data)),
					)
				})?;
				if state.apply(&event, on_event)? {
					return Ok(state.finalize(model));
				}
			}
		}
	}
	Err(CoderError::decode(
		"chatgpt stream",
		"stream closed before the response completed",
	))
}

#[derive(Default)]
struct CallBuf {
	call_id: String,
	name: String,
	arguments: String,
}

#[derive(Default)]
struct StreamState {
	content: String,
	thinking: String,
	/// Function calls by `output_index`.
	calls: BTreeMap<u64, CallBuf>,
	/// Completed output items by `output_index`, reduced for replay.
	items: BTreeMap<u64, Value>,
	usage: Option<TokenUsage>,
	incomplete_reason: Option<String>,
	/// Set by `reasoning_summary_part.added` after the first part, so
	/// summary parts read as paragraphs.
	pending_break: bool,
}

impl StreamState {
	/// Apply one event. `Ok(true)` once the response is complete.
	fn apply<F>(&mut self, event: &Value, on_event: &mut F) -> Result<bool, CoderError>
	where
		F: FnMut(StreamEvent<'_>),
	{
		let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
		let output_index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0);
		match kind {
			"response.output_text.delta" | "response.refusal.delta" => {
				let delta = event.get("delta").and_then(Value::as_str).unwrap_or_default();
				self.content.push_str(delta);
				on_event(StreamEvent::ContentDelta { delta });
			}
			"response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
				let delta = event.get("delta").and_then(Value::as_str).unwrap_or_default();
				if self.pending_break {
					self.pending_break = false;
					self.thinking.push_str("\n\n");
					on_event(StreamEvent::ThinkingDelta { delta: "\n\n" });
				}
				self.thinking.push_str(delta);
				on_event(StreamEvent::ThinkingDelta { delta });
			}
			"response.reasoning_summary_part.added" => {
				self.pending_break = !self.thinking.is_empty();
			}
			"response.output_item.added" => {
				let item = event.get("item").unwrap_or(&Value::Null);
				if item.get("type").and_then(Value::as_str) == Some("function_call") {
					let buf = CallBuf {
						call_id: str_field(item, "call_id"),
						name: str_field(item, "name"),
						arguments: String::new(),
					};
					let index = self.calls.len();
					on_event(StreamEvent::ToolCallDelta {
						index,
						id: Some(&buf.call_id),
						name: Some(&buf.name),
						arguments_delta: None,
					});
					self.calls.insert(output_index, buf);
				}
			}
			"response.function_call_arguments.delta" => {
				let delta = event.get("delta").and_then(Value::as_str).unwrap_or_default();
				let index = self.calls.keys().position(|k| *k == output_index).unwrap_or(0);
				if let Some(buf) = self.calls.get_mut(&output_index) {
					buf.arguments.push_str(delta);
				}
				on_event(StreamEvent::ToolCallDelta {
					index,
					id: None,
					name: None,
					arguments_delta: Some(delta),
				});
			}
			"response.output_item.done" => {
				let item = event.get("item").unwrap_or(&Value::Null);
				self.item_done(output_index, item);
			}
			"response.completed" | "response.done" | "response.incomplete" => {
				let response = event.get("response").unwrap_or(&Value::Null);
				self.usage = response.get("usage").map(usage_of);
				self.incomplete_reason = response
					.get("incomplete_details")
					.and_then(|d| d.get("reason"))
					.and_then(Value::as_str)
					.map(str::to_owned);
				return Ok(true);
			}
			"response.failed" => {
				let err = event
					.get("response")
					.and_then(|r| r.get("error"))
					.unwrap_or(&Value::Null);
				return Err(stream_error(err));
			}
			"error" => {
				return Err(stream_error(event.get("error").unwrap_or(event)));
			}
			_ => {}
		}
		Ok(false)
	}

	fn item_done(&mut self, output_index: u64, item: &Value) {
		let reduced = match item.get("type").and_then(Value::as_str) {
			Some("reasoning") => item.clone(),
			Some("message") => json!({ "type": "message", "id": item.get("id") }),
			Some("function_call") => {
				let buf = self.calls.entry(output_index).or_default();
				buf.call_id = str_field(item, "call_id");
				buf.name = str_field(item, "name");
				if let Some(args) = item.get("arguments").and_then(Value::as_str) {
					buf.arguments = args.to_owned();
				}
				json!({ "type": "function_call", "id": item.get("id"), "call_id": buf.call_id })
			}
			_ => return,
		};
		self.items.insert(output_index, reduced);
	}

	fn finalize(self, model: &str) -> AssistantResponse {
		let tool_calls: Vec<ToolCall> = self
			.calls
			.into_values()
			.map(|c| ToolCall {
				id: c.call_id,
				kind: "function".into(),
				function: FunctionCall {
					name: c.name,
					arguments: c.arguments,
				},
			})
			.collect();
		let raw_reason = match self.incomplete_reason.as_deref() {
			Some("max_output_tokens") => Some("length"),
			Some("content_filter") => Some("content_filter"),
			_ => None,
		};
		let stop_reason = normalize_stop_reason(raw_reason, !tool_calls.is_empty());
		let thinking_blocks = if self.items.is_empty() {
			Vec::new()
		} else {
			vec![ThinkingBlock::ResponsesOutput {
				model: model.to_owned(),
				items: self.items.into_values().collect(),
			}]
		};
		AssistantResponse {
			content: Some(self.content).filter(|c| !c.is_empty()),
			thinking: Some(self.thinking).filter(|t| !t.is_empty()),
			thinking_blocks,
			tool_calls,
			usage: self.usage,
			stop_reason: Some(stop_reason),
		}
	}
}

fn str_field(item: &Value, key: &str) -> String {
	item.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn usage_of(usage: &Value) -> TokenUsage {
	let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0) as u32;
	let prompt = n(usage.get("input_tokens"));
	let completion = n(usage.get("output_tokens"));
	TokenUsage {
		prompt_tokens: prompt,
		completion_tokens: completion,
		total_tokens: usage
			.get("total_tokens")
			.and_then(Value::as_u64)
			.map_or(prompt + completion, |t| t as u32),
		cache_read_input_tokens: n(usage.get("input_tokens_details").and_then(|d| d.get("cached_tokens"))),
		cache_creation_input_tokens: 0,
	}
}

fn stream_error(err: &Value) -> CoderError {
	let code = err.get("code").and_then(Value::as_str).unwrap_or("error");
	let message = err
		.get("message")
		.and_then(Value::as_str)
		.unwrap_or("the ChatGPT backend reported an error");
	CoderError::Internal(format!("ChatGPT: {message} ({code})"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn call(id: &str, args: &str) -> ToolCall {
		ToolCall {
			id: id.into(),
			kind: "function".into(),
			function: FunctionCall {
				name: "read_file".into(),
				arguments: args.into(),
			},
		}
	}

	fn run(events: &[Value]) -> AssistantResponse {
		let mut state = StreamState::default();
		for e in events {
			if state.apply(e, &mut |_| {}).unwrap() {
				break;
			}
		}
		state.finalize("gpt-x")
	}

	#[test]
	fn assembles_text_reasoning_calls_and_usage() {
		let resp = run(&[
			json!({"type":"response.reasoning_summary_part.added","output_index":0}),
			json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"plan"}),
			json!({"type":"response.reasoning_summary_part.added","output_index":0}),
			json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"more"}),
			json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"enc","summary":[]}}),
			json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}),
			json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"pa"}),
			json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"th\":1}"}),
			json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":1}"}}),
			json!({"type":"response.completed","response":{"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":60},"output_tokens":20,"total_tokens":120}}}),
		]);
		assert_eq!(resp.thinking.as_deref(), Some("plan\n\nmore"));
		assert_eq!(resp.tool_calls.len(), 1);
		assert_eq!(resp.tool_calls[0].id, "call_1");
		assert_eq!(resp.tool_calls[0].function.arguments, "{\"path\":1}");
		assert_eq!(resp.stop_reason.as_deref(), Some("toolUse"));
		let usage = resp.usage.unwrap();
		assert_eq!((usage.prompt_tokens, usage.cache_read_input_tokens), (100, 60));
		let ThinkingBlock::ResponsesOutput { model, items } = &resp.thinking_blocks[0] else {
			panic!("expected responses output");
		};
		assert_eq!(model, "gpt-x");
		assert_eq!(items[0]["encrypted_content"], "enc");
		assert_eq!(items[1], json!({"type":"function_call","id":"fc_1","call_id":"call_1"}));
	}

	#[test]
	fn incomplete_for_output_cap_is_a_length_stop() {
		let resp = run(&[
			json!({"type":"response.output_text.delta","output_index":0,"delta":"partial"}),
			json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
		]);
		assert!(resp.hit_output_cap());
	}

	#[test]
	fn failed_event_is_an_error() {
		let mut state = StreamState::default();
		let err = state
			.apply(
				&json!({"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"too long"}}}),
				&mut |_| {},
			)
			.unwrap_err();
		assert!(err.to_string().contains("context_length_exceeded"));
	}

	#[test]
	fn replays_stored_items_in_order_for_the_same_model() {
		let stored = ThinkingBlock::ResponsesOutput {
			model: "gpt-x".into(),
			items: vec![
				json!({"type":"reasoning","id":"rs_1","encrypted_content":"enc"}),
				json!({"type":"message","id":"msg_1"}),
				json!({"type":"function_call","id":"fc_1","call_id":"call_1"}),
			],
		};
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("hi"),
			ChatMessage::Assistant {
				content: Some("looking".into()),
				thinking_blocks: vec![stored],
				tool_calls: vec![call("call_1", "{\"path\":\"a\"}"), call("call_2", "")],
			},
			ChatMessage::Tool {
				tool_call_id: "call_1".into(),
				tool_name: Some("read_file".into()),
				content: "ok".into(),
				images: Vec::new(),
			},
		];
		let (instructions, input) = translate(&messages, "gpt-x");
		assert_eq!(instructions, "sys");
		assert_eq!(input[1]["type"], "reasoning");
		assert_eq!(input[2]["id"], "msg_1");
		assert_eq!(input[2]["content"][0]["text"], "looking");
		assert_eq!(input[3]["id"], "fc_1");
		assert_eq!(input[3]["arguments"], "{\"path\":\"a\"}");
		assert_eq!(input[4]["call_id"], "call_2");
		assert_eq!(input[4]["arguments"], "{}");
		assert!(input[4].get("id").is_none());
		assert_eq!(input[5]["type"], "function_call_output");

		// Another model's encrypted reasoning is not replayed.
		let (_, other) = translate(&messages, "gpt-y");
		assert!(other.iter().all(|i| i["type"] != "reasoning"));
		assert!(other.iter().all(|i| i.get("id").is_none()));
	}

	#[test]
	fn usage_limit_is_named_and_not_retried() {
		let body = r#"{"error":{"type":"usage_limit_reached","plan_type":"PRO","resets_at":0}}"#;
		let msg = plan_limit_message(reqwest::StatusCode::TOO_MANY_REQUESTS, body).unwrap();
		assert!(msg.starts_with("ChatGPT usage limit reached (pro plan)."));
		let busy = r#"{"error":{"type":"rate_limit_exceeded"}}"#;
		assert!(plan_limit_message(reqwest::StatusCode::TOO_MANY_REQUESTS, busy).is_none());
	}
}
