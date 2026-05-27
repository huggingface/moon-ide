//! Anthropic Messages API (`/v1/messages`) translation.
//!
//! Anthropic is **not** OpenAI-compatible. The wire shape diverges
//! in too many places to fake through the OpenRouter-shaped layer
//! the rest of `moon-coder` speaks, so this module owns the full
//! translation when the active route is [`RouteKind::Anthropic`]:
//!
//! - **Auth:** `x-api-key: <key>` (no `Authorization: Bearer`).
//! - **System prompt:** top-level `system` field, not the first
//!   message in the list.
//! - **Tools:** flat `[{ name, description, input_schema }]` rather
//!   than the OpenAI `[{ "type": "function", "function": { ... } }]`
//!   wrapping.
//! - **Tool calls / results:** structured content blocks
//!   (`tool_use` / `tool_result`) inside assistant / user messages,
//!   not a separate `tool` role.
//! - **Images:** `{type:"image", source:{type:"base64", media_type,
//!   data}}` blocks, not the OpenAI vision-API `image_url` block.
//! - **Prompt cache:** native `cache_control: {type:"ephemeral"}`
//!   markers inline on text / tool_result blocks (we mark the
//!   final block of the system prompt and the final user-role
//!   message — same strategy as the OpenRouter path).
//! - **Streaming SSE:** `message_start` / `content_block_*` /
//!   `message_delta` / `message_stop` event grammar instead of
//!   OpenAI chunk-shaped `data:` payloads.
//!
//! Required headers per Anthropic's docs:
//! - `x-api-key: <key>`
//! - `anthropic-version: 2023-06-01`
//! - `content-type: application/json`
//!
//! `max_tokens` is required by the API; we set it to a value large
//! enough for any single agent turn. `stream` is the OpenAI-shape
//! flag we already toggle on the request.

use std::collections::HashMap;

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::error::{request_id_of, CoderError};
use crate::inference::{
	extract_data_lines, find_event_boundary, truncate_for_log, AssistantResponse, ChatMessage, FunctionCall,
	ResolvedRoute, StreamEvent, TokenUsage, ToolCall, ToolDefinition,
};
use moon_protocol::coder_models::{ProviderModelSummary, ProviderProbeResult};

/// Ceiling on the model's reply length for one round-trip. Anthropic
/// requires this field; pick something large enough that a verbose
/// answer plus a few tool calls comfortably fits, but bounded so a
/// runaway model can't burn the whole window in one shot. The
/// runner handles the actual context bookkeeping via the
/// `compaction` layer.
const MAX_TOKENS: u32 = 8_192;

const ANTHROPIC_VERSION: &str = "2023-06-01";

const API_KEY_HEADER: &str = "x-api-key";
const VERSION_HEADER: &str = "anthropic-version";

#[derive(Debug, Serialize)]
struct Request<'a> {
	model: &'a str,
	max_tokens: u32,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	system: Vec<TextBlock<'a>>,
	messages: Vec<Message<'a>>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	tools: Vec<Tool<'a>>,
	stream: bool,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
	role: &'static str,
	content: Vec<Block<'a>>,
}

/// Owned text block — used in the top-level `system` array, where
/// we own the string for the duration of the request and want a
/// borrowed view that survives the request build.
#[derive(Debug, Serialize)]
struct TextBlock<'a> {
	#[serde(rename = "type")]
	kind: &'static str,
	text: &'a str,
	#[serde(skip_serializing_if = "Option::is_none")]
	cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block<'a> {
	Text {
		text: &'a str,
		#[serde(skip_serializing_if = "Option::is_none")]
		cache_control: Option<CacheControl>,
	},
	Image {
		source: ImageSource<'a>,
	},
	ToolUse {
		id: &'a str,
		name: &'a str,
		input: serde_json::Value,
	},
	ToolResult {
		tool_use_id: &'a str,
		content: &'a str,
		#[serde(skip_serializing_if = "Option::is_none")]
		cache_control: Option<CacheControl>,
	},
}

#[derive(Debug, Serialize)]
struct ImageSource<'a> {
	#[serde(rename = "type")]
	kind: &'static str,
	media_type: &'a str,
	data: &'a str,
}

#[derive(Debug, Serialize)]
struct Tool<'a> {
	name: &'a str,
	description: &'a str,
	input_schema: &'a serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct CacheControl {
	#[serde(rename = "type")]
	kind: &'static str,
}

impl CacheControl {
	const EPHEMERAL: Self = Self { kind: "ephemeral" };
}

/// Translation of one [`ChatMessage`] slice into the request body
/// the Messages API expects, plus the data we need to hold for the
/// duration of serialisation.
///
/// `system` holds the concatenated system prompt(s) Anthropic
/// expects at the top level; `messages` is the alternating
/// user/assistant transcript with `tool` role messages already
/// merged into adjacent user messages as `tool_result` blocks.
struct Translated<'a> {
	system: Vec<TextBlock<'a>>,
	messages: Vec<Message<'a>>,
}

/// Build the Anthropic-shaped messages array from our internal
/// [`ChatMessage`]s. Drives all the wire-shape quirks in one place:
///
/// - `System` messages → top-level `system` array entries.
/// - `User { content, images }` → user message with text + image blocks.
/// - `Assistant { content, tool_calls }` → assistant message with
///   text block (when content non-empty) + one `tool_use` block
///   per tool call.
/// - `Tool { tool_call_id, content }` → user message with a single
///   `tool_result` block. Adjacent tool messages (and an
///   immediately-following user message) get merged into one user
///   message with multiple blocks — Anthropic rejects two
///   consecutive same-role messages.
///
/// Cache markers: when `mark_system_cache` is true, the final block
/// in the system array carries `cache_control: ephemeral`. When
/// `mark_last_user_cache` is true, the final block of the last
/// user-role message carries the same marker. Both fire only on
/// the assumption the route is Anthropic-native — the caller
/// (`chat_completion` in this module) just always passes both as
/// `true`; OpenRouter's marker logic stays where it was.
fn translate<'a>(messages: &'a [ChatMessage], mark_system_cache: bool, mark_last_user_cache: bool) -> Translated<'a> {
	let mut system: Vec<TextBlock<'a>> = Vec::new();
	let mut out: Vec<Message<'a>> = Vec::new();

	for msg in messages {
		match msg {
			ChatMessage::System { content } => {
				system.push(TextBlock {
					kind: "text",
					text: content,
					cache_control: None,
				});
			}
			ChatMessage::User { content, images } => {
				let mut blocks: Vec<Block<'a>> = Vec::with_capacity(images.len() + 1);
				let trimmed = content.trim();
				if !trimmed.is_empty() {
					blocks.push(Block::Text {
						text: content,
						cache_control: None,
					});
				}
				for img in images {
					if let Some((media_type, data)) = split_data_url(&img.data_url) {
						blocks.push(Block::Image {
							source: ImageSource {
								kind: "base64",
								media_type,
								data,
							},
						});
					} else {
						tracing::warn!(mime = %img.mime, "skipping image attachment with unparsable data URL");
					}
				}
				// Anthropic rejects empty content arrays *and*
				// whitespace-only text blocks (`messages: text
				// content blocks must contain non-whitespace
				// text`). A user message with only whitespace +
				// no images carries no signal anyway, so drop
				// the row entirely rather than synthesising a
				// no-op block that 400s.
				if blocks.is_empty() {
					tracing::debug!("dropping empty user message before sending to Anthropic");
					continue;
				}
				push_or_merge_user(&mut out, blocks);
			}
			ChatMessage::Assistant { content, tool_calls } => {
				let mut blocks: Vec<Block<'a>> = Vec::with_capacity(tool_calls.len() + 1);
				if let Some(text) = content.as_deref() {
					if !text.trim().is_empty() {
						blocks.push(Block::Text {
							text,
							cache_control: None,
						});
					}
				}
				for call in tool_calls {
					let input = parse_tool_args(&call.function.arguments);
					blocks.push(Block::ToolUse {
						id: &call.id,
						name: &call.function.name,
						input,
					});
				}
				// Empty-shell assistant turns (no text, no tool
				// calls) used to be persisted by older runners
				// whenever a provider bailed mid-stream. Anthropic
				// rejects whitespace-only blocks now, so drop the
				// row instead of papering over it with a space.
				// The runner refuses to persist these going
				// forward; this arm guards historical sessions
				// loaded off disk.
				if blocks.is_empty() {
					tracing::debug!("dropping empty assistant message before sending to Anthropic");
					continue;
				}
				out.push(Message {
					role: "assistant",
					content: blocks,
				});
			}
			ChatMessage::Tool { tool_call_id, content } => {
				let block = Block::ToolResult {
					tool_use_id: tool_call_id,
					content,
					cache_control: None,
				};
				push_or_merge_user(&mut out, vec![block]);
			}
		}
	}

	if mark_system_cache {
		if let Some(last) = system.last_mut() {
			last.cache_control = Some(CacheControl::EPHEMERAL);
		}
	}
	if mark_last_user_cache {
		if let Some(msg) = out.iter_mut().rev().find(|m| m.role == "user") {
			set_cache_marker_on_last_block(&mut msg.content);
		}
	}

	Translated { system, messages: out }
}

fn push_or_merge_user<'a>(out: &mut Vec<Message<'a>>, mut blocks: Vec<Block<'a>>) {
	match out.last_mut() {
		Some(prev) if prev.role == "user" => prev.content.append(&mut blocks),
		_ => out.push(Message {
			role: "user",
			content: blocks,
		}),
	}
}

fn set_cache_marker_on_last_block(blocks: &mut [Block<'_>]) {
	let Some(last) = blocks.last_mut() else {
		return;
	};
	match last {
		Block::Text { cache_control, .. } | Block::ToolResult { cache_control, .. } => {
			*cache_control = Some(CacheControl::EPHEMERAL);
		}
		Block::Image { .. } | Block::ToolUse { .. } => {
			// Anthropic doesn't allow `cache_control` on every
			// block kind; image / tool_use don't accept it. Skip
			// the marker rather than emit an invalid request.
		}
	}
}

/// Best-effort parse of the OpenAI-shaped `arguments` JSON string
/// back into a structured value. The Anthropic API expects the
/// `tool_use.input` field to be an object, not a string. If the
/// model emitted invalid JSON (it sometimes does, especially
/// mid-thought or on empty calls) we fall back to an empty object;
/// the model's next turn typically self-corrects.
fn parse_tool_args(arguments: &str) -> serde_json::Value {
	if arguments.trim().is_empty() {
		return serde_json::Value::Object(serde_json::Map::new());
	}
	match serde_json::from_str::<serde_json::Value>(arguments) {
		Ok(v) => v,
		Err(err) => {
			tracing::warn!(
				error = %err,
				args = %truncate_for_log(arguments),
				"tool_use arguments aren't valid JSON; sending empty object",
			);
			serde_json::Value::Object(serde_json::Map::new())
		}
	}
}

/// `data:image/png;base64,XXXX` → `("image/png", "XXXX")`.
/// Returns `None` for any other shape; the caller drops the
/// attachment and logs.
fn split_data_url(data_url: &str) -> Option<(&str, &str)> {
	let rest = data_url.strip_prefix("data:")?;
	let (header, payload) = rest.split_once(',')?;
	let media_type = header.strip_suffix(";base64").unwrap_or(header);
	if media_type.is_empty() {
		return None;
	}
	Some((media_type, payload))
}

/// Issue one non-streaming `/v1/messages` round trip and translate
/// the response back into the runner's [`AssistantResponse`] shape.
pub(crate) async fn chat_completion(
	http: &reqwest::Client,
	route: &ResolvedRoute,
	model: &str,
	messages: &[ChatMessage],
	tools: &[ToolDefinition],
	cancel: &tokio_util::sync::CancellationToken,
) -> Result<AssistantResponse, CoderError> {
	let endpoint = format!("{}/v1/messages", route.base_url);
	let translated = translate(messages, true, true);
	let tool_views: Vec<Tool<'_>> = tools
		.iter()
		.map(|t| Tool {
			name: &t.function.name,
			description: &t.function.description,
			input_schema: &t.function.parameters,
		})
		.collect();
	let body = Request {
		model,
		max_tokens: MAX_TOKENS,
		system: translated.system,
		messages: translated.messages,
		tools: tool_views,
		stream: false,
	};

	let send = http
		.post(&endpoint)
		.header(API_KEY_HEADER, route.auth_token.as_deref().unwrap_or_default())
		.header(VERSION_HEADER, ANTHROPIC_VERSION)
		.json(&body)
		.send();
	let response = tokio::select! {
		biased;
		_ = cancel.cancelled() => return Err(CoderError::Aborted),
		resp = send => resp.map_err(CoderError::from)?,
	};

	let status = response.status();
	let request_id = request_id_of(&response);
	let recv = response.text();
	let text = tokio::select! {
		biased;
		_ = cancel.cancelled() => return Err(CoderError::Aborted),
		out = recv => out.map_err(CoderError::from)?,
	};
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), text, request_id));
	}

	let parsed: NonStreamResponse = crate::auth::decode_body(&endpoint, &text)?;
	Ok(parsed.into_assistant_response())
}

/// Streaming `/v1/messages` round trip. Same shape as
/// [`InferenceClient::chat_completion_stream`]: `on_event` fires
/// for every parsed delta as bytes arrive, and the assembled
/// [`AssistantResponse`] returns once the stream ends.
pub(crate) async fn chat_completion_stream<F>(
	http: &reqwest::Client,
	route: &ResolvedRoute,
	model: &str,
	messages: &[ChatMessage],
	tools: &[ToolDefinition],
	cancel: &tokio_util::sync::CancellationToken,
	mut on_event: F,
) -> Result<AssistantResponse, CoderError>
where
	F: FnMut(StreamEvent<'_>),
{
	let endpoint = format!("{}/v1/messages", route.base_url);
	let translated = translate(messages, true, true);
	let tool_views: Vec<Tool<'_>> = tools
		.iter()
		.map(|t| Tool {
			name: &t.function.name,
			description: &t.function.description,
			input_schema: &t.function.parameters,
		})
		.collect();
	let body = Request {
		model,
		max_tokens: MAX_TOKENS,
		system: translated.system,
		messages: translated.messages,
		tools: tool_views,
		stream: true,
	};

	let send = http
		.post(&endpoint)
		.header(API_KEY_HEADER, route.auth_token.as_deref().unwrap_or_default())
		.header(VERSION_HEADER, ANTHROPIC_VERSION)
		.header("accept", "text/event-stream")
		.json(&body)
		.send();
	let response = tokio::select! {
		biased;
		_ = cancel.cancelled() => return Err(CoderError::Aborted),
		resp = send => resp.map_err(CoderError::from)?,
	};

	let status = response.status();
	if !status.is_success() {
		let request_id = request_id_of(&response);
		let recv = response.text();
		let text = tokio::select! {
			biased;
			_ = cancel.cancelled() => return Err(CoderError::Aborted),
			out = recv => out.map_err(CoderError::from)?,
		};
		return Err(CoderError::http(endpoint, status.as_u16(), text, request_id));
	}

	consume_stream(response, cancel, &mut on_event).await
}

async fn consume_stream<F>(
	response: reqwest::Response,
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
		let bytes = chunk.map_err(CoderError::from)?;
		sse_buf.extend_from_slice(&bytes);

		while let Some(end) = find_event_boundary(&sse_buf) {
			let event_bytes = sse_buf.drain(..end.boundary_end).collect::<Vec<u8>>();
			let event_text = std::str::from_utf8(&event_bytes[..end.body_end])
				.map_err(|err| CoderError::decode("anthropic stream", format!("invalid utf-8 in SSE event: {err}")))?;
			for data in extract_data_lines(event_text) {
				let event: StreamEventBody = serde_json::from_str(data).map_err(|err| {
					CoderError::decode(
						"anthropic stream",
						format!("could not parse SSE event: {err}; raw={}", truncate_for_log(data)),
					)
				})?;
				if state.apply(event, on_event)? {
					return Ok(state.finalize());
				}
			}
		}
	}

	Ok(state.finalize())
}

#[derive(Default)]
struct StreamState {
	content: String,
	thinking: String,
	tool_calls: Vec<ToolCallBuf>,
	/// Map from Anthropic `content_block` index → position in
	/// `tool_calls`. Anthropic content blocks are indexed
	/// per-message and intermix text / tool_use freely; the
	/// runner-side `ToolCall` list packs only the tool_use ones,
	/// so we keep the projection here.
	block_to_tool: HashMap<u32, usize>,
	usage: Option<TokenUsage>,
}

#[derive(Default)]
struct ToolCallBuf {
	id: String,
	name: String,
	arguments: String,
}

impl StreamState {
	/// Apply one decoded SSE event. Returns `Ok(true)` when the
	/// stream's terminal event has landed (`message_stop`).
	fn apply<F>(&mut self, event: StreamEventBody, on_event: &mut F) -> Result<bool, CoderError>
	where
		F: FnMut(StreamEvent<'_>),
	{
		match event {
			StreamEventBody::MessageStart { message } => {
				if let Some(usage) = message.usage {
					self.usage = Some(merge_usage(self.usage, usage));
				}
			}
			StreamEventBody::ContentBlockStart { index, content_block } => match content_block {
				ContentBlockSpec::Text { .. } => {
					// nothing to do; the text accumulates via deltas
				}
				ContentBlockSpec::Thinking { .. } => {}
				ContentBlockSpec::ToolUse { id, name, .. } => {
					let pos = self.tool_calls.len();
					self.tool_calls.push(ToolCallBuf {
						id: id.clone(),
						name: name.clone(),
						arguments: String::new(),
					});
					self.block_to_tool.insert(index, pos);
					on_event(StreamEvent::ToolCallDelta {
						index: pos,
						id: Some(&id),
						name: Some(&name),
						arguments_delta: None,
					});
				}
			},
			StreamEventBody::ContentBlockDelta { index, delta } => match delta {
				BlockDelta::TextDelta { text } => {
					self.content.push_str(&text);
					on_event(StreamEvent::ContentDelta { delta: &text });
				}
				BlockDelta::ThinkingDelta { thinking } => {
					self.thinking.push_str(&thinking);
					on_event(StreamEvent::ThinkingDelta { delta: &thinking });
				}
				BlockDelta::InputJsonDelta { partial_json } => {
					if let Some(&pos) = self.block_to_tool.get(&index) {
						self.tool_calls[pos].arguments.push_str(&partial_json);
						on_event(StreamEvent::ToolCallDelta {
							index: pos,
							id: None,
							name: None,
							arguments_delta: Some(&partial_json),
						});
					}
				}
				BlockDelta::SignatureDelta { .. } | BlockDelta::Other => {}
			},
			StreamEventBody::ContentBlockStop { .. } => {}
			StreamEventBody::MessageDelta { usage, .. } => {
				if let Some(u) = usage {
					self.usage = Some(merge_usage(self.usage, u));
				}
			}
			StreamEventBody::MessageStop => {
				return Ok(true);
			}
			StreamEventBody::Error { error } => {
				return Err(CoderError::http(
					"anthropic stream",
					0,
					format!("{}: {}", error.kind, error.message),
					None,
				));
			}
			StreamEventBody::Ping | StreamEventBody::Other => {}
		}
		Ok(false)
	}

	fn finalize(self) -> AssistantResponse {
		AssistantResponse {
			content: if self.content.is_empty() {
				None
			} else {
				Some(self.content)
			},
			thinking: if self.thinking.is_empty() {
				None
			} else {
				Some(self.thinking)
			},
			tool_calls: self
				.tool_calls
				.into_iter()
				.filter(|b| !b.id.is_empty() || !b.name.is_empty())
				.map(|b| ToolCall {
					id: b.id,
					kind: "function".into(),
					function: FunctionCall {
						name: b.name,
						arguments: if b.arguments.is_empty() {
							"{}".into()
						} else {
							b.arguments
						},
					},
				})
				.collect(),
			usage: self.usage,
		}
	}
}

/// Anthropic splits usage between `message_start` (input numbers,
/// including the cache split) and `message_delta` (final
/// `output_tokens`). Combine the two snapshots so the runner sees
/// one consistent [`TokenUsage`].
///
/// **Anthropic's `input_tokens` is *only the non-cached portion***
/// of the prompt — `cache_read_input_tokens` and
/// `cache_creation_input_tokens` are reported alongside it, *not*
/// as a sub-breakdown. The total prompt size (what actually fills
/// the model's context window) is the sum of all three. We
/// surface that sum as `prompt_tokens` so the context-window ring
/// and the compaction trigger see the real footprint — otherwise
/// a heavily-cached long session would show `1 / 1M` and never
/// auto-compact even at 90 % of the window. The OpenAI-compatible
/// `prompt_tokens` follows the same convention (full input,
/// regardless of how it was billed); this just keeps the two
/// providers' semantics aligned.
fn merge_usage(existing: Option<TokenUsage>, incoming: AnthropicUsage) -> TokenUsage {
	let mut out = existing.unwrap_or_default();
	// Recover the raw, non-cached `input_tokens` from a prior
	// merge by subtracting the cache split we already rolled in.
	// `message_delta` typically only carries `output_tokens`, so
	// the cache fields stay stable and this round-trips cleanly.
	let mut raw_input = out
		.prompt_tokens
		.saturating_sub(out.cache_read_input_tokens)
		.saturating_sub(out.cache_creation_input_tokens);
	if let Some(v) = incoming.input_tokens {
		raw_input = v;
	}
	if let Some(v) = incoming.output_tokens {
		out.completion_tokens = v;
	}
	if let Some(v) = incoming.cache_read_input_tokens {
		out.cache_read_input_tokens = v;
	}
	if let Some(v) = incoming.cache_creation_input_tokens {
		out.cache_creation_input_tokens = v;
	}
	// Roll the cache portions into `prompt_tokens` so it matches
	// the full-input convention every other provider uses. See
	// the doc comment above for why.
	out.prompt_tokens = raw_input
		.saturating_add(out.cache_read_input_tokens)
		.saturating_add(out.cache_creation_input_tokens);
	out.total_tokens = out.prompt_tokens.saturating_add(out.completion_tokens);
	out
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEventBody {
	MessageStart {
		message: StreamStartMessage,
	},
	ContentBlockStart {
		index: u32,
		content_block: ContentBlockSpec,
	},
	ContentBlockDelta {
		index: u32,
		delta: BlockDelta,
	},
	ContentBlockStop {
		#[allow(dead_code)]
		index: u32,
	},
	MessageDelta {
		#[allow(dead_code)]
		#[serde(default)]
		delta: serde_json::Value,
		#[serde(default)]
		usage: Option<AnthropicUsage>,
	},
	MessageStop,
	Ping,
	Error {
		error: StreamError,
	},
	#[serde(other)]
	Other,
}

#[derive(Debug, Deserialize)]
struct StreamStartMessage {
	#[serde(default)]
	usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockSpec {
	Text {
		#[serde(default)]
		#[allow(dead_code)]
		text: String,
	},
	Thinking {
		#[serde(default)]
		#[allow(dead_code)]
		thinking: String,
	},
	ToolUse {
		id: String,
		name: String,
		#[serde(default)]
		#[allow(dead_code)]
		input: serde_json::Value,
	},
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BlockDelta {
	TextDelta {
		text: String,
	},
	ThinkingDelta {
		thinking: String,
	},
	InputJsonDelta {
		partial_json: String,
	},
	SignatureDelta {
		#[allow(dead_code)]
		#[serde(default)]
		signature: String,
	},
	#[serde(other)]
	Other,
}

#[derive(Debug, Deserialize)]
struct StreamError {
	#[serde(rename = "type", default)]
	kind: String,
	#[serde(default)]
	message: String,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
	#[serde(default)]
	input_tokens: Option<u32>,
	#[serde(default)]
	output_tokens: Option<u32>,
	#[serde(default)]
	cache_read_input_tokens: Option<u32>,
	#[serde(default)]
	cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct NonStreamResponse {
	#[serde(default)]
	content: Vec<NonStreamBlock>,
	#[serde(default)]
	usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NonStreamBlock {
	Text {
		#[serde(default)]
		text: String,
	},
	Thinking {
		#[serde(default)]
		thinking: String,
	},
	ToolUse {
		id: String,
		name: String,
		#[serde(default)]
		input: serde_json::Value,
	},
	#[serde(other)]
	Other,
}

impl NonStreamResponse {
	fn into_assistant_response(self) -> AssistantResponse {
		let mut content = String::new();
		let mut thinking = String::new();
		let mut tool_calls: Vec<ToolCall> = Vec::new();
		for block in self.content {
			match block {
				NonStreamBlock::Text { text } => content.push_str(&text),
				NonStreamBlock::Thinking { thinking: t } => thinking.push_str(&t),
				NonStreamBlock::ToolUse { id, name, input } => {
					let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
					tool_calls.push(ToolCall {
						id,
						kind: "function".into(),
						function: FunctionCall { name, arguments },
					});
				}
				NonStreamBlock::Other => {}
			}
		}
		let usage = self.usage.map(|u| {
			let mut out = TokenUsage::default();
			if let Some(v) = u.input_tokens {
				out.prompt_tokens = v;
			}
			if let Some(v) = u.output_tokens {
				out.completion_tokens = v;
			}
			if let Some(v) = u.cache_read_input_tokens {
				out.cache_read_input_tokens = v;
			}
			if let Some(v) = u.cache_creation_input_tokens {
				out.cache_creation_input_tokens = v;
			}
			// Anthropic's `input_tokens` is the non-cached portion
			// only; roll the cache numbers in so `prompt_tokens`
			// matches the full-input convention. See `merge_usage`
			// for the long-form rationale.
			out.prompt_tokens = out
				.prompt_tokens
				.saturating_add(out.cache_read_input_tokens)
				.saturating_add(out.cache_creation_input_tokens);
			out.total_tokens = out.prompt_tokens.saturating_add(out.completion_tokens);
			out
		});
		AssistantResponse {
			content: if content.is_empty() { None } else { Some(content) },
			thinking: if thinking.is_empty() { None } else { Some(thinking) },
			tool_calls,
			usage,
		}
	}
}

/// `GET /v1/models` against the Anthropic API. Returns a flat
/// catalog the picker can render — Anthropic doesn't expose
/// pricing or context-length here, so we emit minimal entries
/// (`id`, `name`) and the runner falls back to the static window
/// table at request time.
pub async fn list_models(
	http: &reqwest::Client,
	base_url: &str,
	api_key: Option<&str>,
) -> Result<Vec<ProviderModelSummary>, CoderError> {
	let endpoint = format!("{}/v1/models", base_url.trim_end_matches('/'));
	let mut req = http.get(&endpoint).header(VERSION_HEADER, ANTHROPIC_VERSION);
	if let Some(key) = api_key {
		req = req.header(API_KEY_HEADER, key);
	}
	let response = req.send().await.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}

	// Anthropic's `/v1/models` returns `max_input_tokens` per
	// model — the per-model context window we surface to the
	// picker as `context_length`. They also return `max_tokens`,
	// which is the cap on the *output* slot for one call (8192 for
	// Claude 4.x today, 128k with the `output-128k-2025-02-19`
	// beta header). We don't expose that as a column yet —
	// `MAX_TOKENS` in the chat-completion path is what hits that
	// limit, and it's a constant we hardcode anyway.
	#[derive(Deserialize)]
	struct ListBody {
		#[serde(default)]
		data: Vec<RawModel>,
	}
	#[derive(Deserialize)]
	struct RawModel {
		id: String,
		#[serde(default)]
		display_name: Option<String>,
		#[serde(default)]
		max_input_tokens: Option<u32>,
	}

	let raw: ListBody = crate::auth::decode_body(&endpoint, &body)?;
	let out = raw
		.data
		.into_iter()
		.map(|m| ProviderModelSummary {
			id: m.id,
			owned_by: Some("anthropic".into()),
			name: m.display_name,
			context_length: m.max_input_tokens,
			pricing_in_per_million: None,
			pricing_out_per_million: None,
			description: None,
		})
		.collect();
	Ok(out)
}

/// How many model ids the Anthropic probe surfaces back to the
/// picker. Matches `providers::PROBE_SAMPLE_LIMIT` so the modal
/// shows the same kind of "we saw these slugs" blurb the
/// OpenAI-compat path emits.
const PROBE_SAMPLE_LIMIT: usize = 5;

/// Probe a `(base_url, api_key)` pair against `/v1/models`. Same
/// surface as the OpenAI-compat probe, just with the Anthropic
/// auth header. A 200 means the credentials are good and the
/// host speaks the API; anything else propagates verbatim so the
/// modal can show "401 Unauthorized" / "couldn't reach host" /
/// etc.
pub async fn probe(
	http: &reqwest::Client,
	base_url: &str,
	api_key: Option<&str>,
) -> Result<ProviderProbeResult, CoderError> {
	let endpoint = format!("{}/v1/models", base_url.trim_end_matches('/'));
	let mut req = http.get(&endpoint).header(VERSION_HEADER, ANTHROPIC_VERSION);
	if let Some(key) = api_key {
		req = req.header(API_KEY_HEADER, key);
	}
	let response = req.send().await.map_err(CoderError::from)?;
	let status = response.status();
	let request_id = request_id_of(&response);
	let body = response.text().await.map_err(CoderError::from)?;
	if !status.is_success() {
		return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
	}
	#[derive(Deserialize)]
	struct ListBody {
		#[serde(default)]
		data: Vec<RawModel>,
	}
	#[derive(Deserialize)]
	struct RawModel {
		id: String,
	}
	let raw: ListBody = crate::auth::decode_body(&endpoint, &body)?;
	let model_count = u32::try_from(raw.data.len()).unwrap_or(u32::MAX);
	let sample_model_ids = raw.data.into_iter().take(PROBE_SAMPLE_LIMIT).map(|m| m.id).collect();
	Ok(ProviderProbeResult {
		model_count,
		sample_model_ids,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::inference::{ChatMessage, FunctionCall, ImageAttachment, ToolCall};

	fn user_msg(text: &str) -> ChatMessage {
		ChatMessage::user(text)
	}

	#[test]
	fn translate_hoists_system_messages_into_top_level() {
		let messages = vec![
			ChatMessage::System {
				content: "first".into(),
			},
			ChatMessage::System {
				content: "second".into(),
			},
			user_msg("hi"),
		];
		let t = translate(&messages, false, false);
		assert_eq!(t.system.len(), 2);
		assert_eq!(t.system[0].text, "first");
		assert_eq!(t.system[1].text, "second");
		assert_eq!(t.messages.len(), 1);
		assert_eq!(t.messages[0].role, "user");
	}

	#[test]
	fn translate_merges_consecutive_tool_results_with_following_user_into_one_message() {
		let messages = vec![
			ChatMessage::Tool {
				tool_call_id: "toolu_a".into(),
				content: "result a".into(),
			},
			ChatMessage::Tool {
				tool_call_id: "toolu_b".into(),
				content: "result b".into(),
			},
			user_msg("now do this"),
		];
		let t = translate(&messages, false, false);
		assert_eq!(t.messages.len(), 1, "merged into one user message");
		assert_eq!(t.messages[0].role, "user");
		assert_eq!(t.messages[0].content.len(), 3);
		match &t.messages[0].content[0] {
			Block::ToolResult {
				tool_use_id, content, ..
			} => {
				assert_eq!(*tool_use_id, "toolu_a");
				assert_eq!(*content, "result a");
			}
			_ => panic!("first block should be tool_result"),
		}
		match &t.messages[0].content[1] {
			Block::ToolResult { tool_use_id, .. } => assert_eq!(*tool_use_id, "toolu_b"),
			_ => panic!("second block should be tool_result"),
		}
		match &t.messages[0].content[2] {
			Block::Text { text, .. } => assert_eq!(*text, "now do this"),
			_ => panic!("third block should be text"),
		}
	}

	#[test]
	fn translate_emits_tool_use_blocks_for_assistant_tool_calls() {
		let messages = vec![
			user_msg("read foo.txt"),
			ChatMessage::Assistant {
				content: Some("on it".into()),
				tool_calls: vec![ToolCall {
					id: "toolu_x".into(),
					kind: "function".into(),
					function: FunctionCall {
						name: "read_file".into(),
						arguments: r#"{"path":"foo.txt"}"#.into(),
					},
				}],
			},
		];
		let t = translate(&messages, false, false);
		assert_eq!(t.messages.len(), 2);
		assert_eq!(t.messages[1].role, "assistant");
		assert_eq!(t.messages[1].content.len(), 2);
		match &t.messages[1].content[0] {
			Block::Text { text, .. } => assert_eq!(*text, "on it"),
			_ => panic!("first block should be text"),
		}
		match &t.messages[1].content[1] {
			Block::ToolUse { id, name, input } => {
				assert_eq!(*id, "toolu_x");
				assert_eq!(*name, "read_file");
				assert_eq!(input["path"], "foo.txt");
			}
			_ => panic!("second block should be tool_use"),
		}
	}

	#[test]
	fn translate_attaches_image_blocks_via_data_url_split() {
		let messages = vec![ChatMessage::User {
			content: "look at this".into(),
			images: vec![ImageAttachment {
				data_url: "data:image/png;base64,AAAA".into(),
				mime: "image/png".into(),
			}],
		}];
		let t = translate(&messages, false, false);
		assert_eq!(t.messages.len(), 1);
		assert_eq!(t.messages[0].content.len(), 2);
		match &t.messages[0].content[1] {
			Block::Image { source } => {
				assert_eq!(source.kind, "base64");
				assert_eq!(source.media_type, "image/png");
				assert_eq!(source.data, "AAAA");
			}
			_ => panic!("expected image block"),
		}
	}

	#[test]
	fn translate_marks_cache_on_last_system_block_and_last_user_block_when_requested() {
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			user_msg("hi"),
			ChatMessage::Tool {
				tool_call_id: "toolu_a".into(),
				content: "ok".into(),
			},
		];
		let t = translate(&messages, true, true);
		// system has the marker
		assert!(t.system[0].cache_control.is_some());
		// last user-role message has marker on its last block
		// (the tool_result, which followed the user prompt).
		let last_user = t.messages.iter().rev().find(|m| m.role == "user").unwrap();
		match last_user.content.last().unwrap() {
			Block::ToolResult { cache_control, .. } => assert!(cache_control.is_some()),
			other => panic!("unexpected last block {other:?}"),
		}
	}

	#[test]
	fn split_data_url_handles_standard_shape() {
		assert_eq!(
			split_data_url("data:image/png;base64,AAAA"),
			Some(("image/png", "AAAA"))
		);
		assert_eq!(split_data_url("not a data url"), None);
		assert_eq!(split_data_url("data:,nope"), None);
	}

	#[test]
	fn parse_tool_args_returns_empty_object_for_invalid_json() {
		let v = parse_tool_args("not json at all");
		assert!(v.is_object());
		assert!(v.as_object().unwrap().is_empty());
	}

	#[test]
	fn parse_tool_args_parses_valid_json() {
		let v = parse_tool_args(r#"{"path":"/a"}"#);
		assert_eq!(v["path"], "/a");
	}

	#[test]
	fn non_stream_response_aggregates_text_and_tool_use_blocks() {
		let body = serde_json::json!({
			"content": [
				{"type": "text", "text": "hello "},
				{"type": "text", "text": "world"},
				{"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "x"}}
			],
			"usage": {"input_tokens": 12, "output_tokens": 5}
		});
		let parsed: NonStreamResponse = serde_json::from_value(body).unwrap();
		let resp = parsed.into_assistant_response();
		assert_eq!(resp.content.as_deref(), Some("hello world"));
		assert_eq!(resp.tool_calls.len(), 1);
		assert_eq!(resp.tool_calls[0].id, "toolu_1");
		assert_eq!(resp.tool_calls[0].function.name, "read_file");
		let usage = resp.usage.expect("usage");
		assert_eq!(usage.prompt_tokens, 12);
		assert_eq!(usage.completion_tokens, 5);
		assert_eq!(usage.total_tokens, 17);
	}

	#[test]
	fn non_stream_usage_rolls_cache_portions_into_prompt_tokens() {
		// Anthropic's `input_tokens` reports only the non-cached
		// portion of the prompt. On a long, heavily-cached session
		// it collapses to a handful of tokens while the real prompt
		// sits in `cache_read_input_tokens`. The runner treats
		// `prompt_tokens` as the full input (compaction trigger,
		// context-window ring denominator), so we roll the cache
		// numbers in here.
		let body = serde_json::json!({
			"content": [{"type": "text", "text": "ok"}],
			"usage": {
				"input_tokens": 3,
				"output_tokens": 5,
				"cache_read_input_tokens": 180_000,
				"cache_creation_input_tokens": 2_000
			}
		});
		let parsed: NonStreamResponse = serde_json::from_value(body).unwrap();
		let usage = parsed.into_assistant_response().usage.expect("usage");
		assert_eq!(usage.prompt_tokens, 182_003);
		assert_eq!(usage.cache_read_input_tokens, 180_000);
		assert_eq!(usage.cache_creation_input_tokens, 2_000);
		assert_eq!(usage.completion_tokens, 5);
		assert_eq!(usage.total_tokens, 182_008);
	}

	#[test]
	fn merge_usage_rolls_cache_portions_into_prompt_tokens() {
		// Streaming path: `message_start` carries `input_tokens` +
		// cache split; `message_delta` carries `output_tokens`.
		// After merging both snapshots `prompt_tokens` must
		// represent the full input, matching the OpenAI-compat
		// convention.
		let after_start = merge_usage(
			None,
			AnthropicUsage {
				input_tokens: Some(3),
				output_tokens: None,
				cache_read_input_tokens: Some(180_000),
				cache_creation_input_tokens: Some(2_000),
			},
		);
		assert_eq!(after_start.prompt_tokens, 182_003);
		assert_eq!(after_start.cache_read_input_tokens, 180_000);
		assert_eq!(after_start.cache_creation_input_tokens, 2_000);

		let after_delta = merge_usage(
			Some(after_start),
			AnthropicUsage {
				input_tokens: None,
				output_tokens: Some(42),
				cache_read_input_tokens: None,
				cache_creation_input_tokens: None,
			},
		);
		// `message_delta` only adds output_tokens — re-merging
		// must not double-count the cache portions into
		// `prompt_tokens`.
		assert_eq!(after_delta.prompt_tokens, 182_003);
		assert_eq!(after_delta.completion_tokens, 42);
		assert_eq!(after_delta.total_tokens, 182_045);
	}

	#[test]
	fn stream_state_assembles_text_then_tool_call_from_block_events() {
		let mut state = StreamState::default();
		let mut events: Vec<&'static str> = Vec::new();

		state
			.apply(
				StreamEventBody::MessageStart {
					message: StreamStartMessage {
						usage: Some(AnthropicUsage {
							input_tokens: Some(100),
							..Default::default()
						}),
					},
				},
				&mut |_| {},
			)
			.unwrap();
		state
			.apply(
				StreamEventBody::ContentBlockStart {
					index: 0,
					content_block: ContentBlockSpec::Text { text: String::new() },
				},
				&mut |_| {},
			)
			.unwrap();
		state
			.apply(
				StreamEventBody::ContentBlockDelta {
					index: 0,
					delta: BlockDelta::TextDelta { text: "hi".into() },
				},
				&mut |ev| match ev {
					StreamEvent::ContentDelta { .. } => events.push("content"),
					_ => events.push("other"),
				},
			)
			.unwrap();
		state
			.apply(StreamEventBody::ContentBlockStop { index: 0 }, &mut |_| {})
			.unwrap();
		state
			.apply(
				StreamEventBody::ContentBlockStart {
					index: 1,
					content_block: ContentBlockSpec::ToolUse {
						id: "toolu_1".into(),
						name: "read_file".into(),
						input: serde_json::Value::Null,
					},
				},
				&mut |ev| match ev {
					StreamEvent::ToolCallDelta { .. } => events.push("tool_start"),
					_ => events.push("other"),
				},
			)
			.unwrap();
		state
			.apply(
				StreamEventBody::ContentBlockDelta {
					index: 1,
					delta: BlockDelta::InputJsonDelta {
						partial_json: r#"{"path":"x"}"#.into(),
					},
				},
				&mut |ev| match ev {
					StreamEvent::ToolCallDelta { .. } => events.push("tool_args"),
					_ => events.push("other"),
				},
			)
			.unwrap();
		state
			.apply(StreamEventBody::ContentBlockStop { index: 1 }, &mut |_| {})
			.unwrap();
		state
			.apply(
				StreamEventBody::MessageDelta {
					delta: serde_json::Value::Null,
					usage: Some(AnthropicUsage {
						output_tokens: Some(7),
						..Default::default()
					}),
				},
				&mut |_| {},
			)
			.unwrap();
		let done = state.apply(StreamEventBody::MessageStop, &mut |_| {}).unwrap();
		assert!(done);
		assert_eq!(events, vec!["content", "tool_start", "tool_args"]);

		let resp = state.finalize();
		assert_eq!(resp.content.as_deref(), Some("hi"));
		assert_eq!(resp.tool_calls.len(), 1);
		assert_eq!(resp.tool_calls[0].id, "toolu_1");
		assert_eq!(resp.tool_calls[0].function.arguments, r#"{"path":"x"}"#);
		let usage = resp.usage.expect("usage");
		assert_eq!(usage.prompt_tokens, 100);
		assert_eq!(usage.completion_tokens, 7);
	}

	#[test]
	fn list_models_parses_max_input_tokens_into_context_length() {
		// Anthropic's `/v1/models` returns `max_input_tokens` per
		// model since the 2025-11 API revision. Older sessions
		// could see a payload without the field — `Option`
		// gracefully degrades to `None`, matching how the rest of
		// the providers handle a missing context.
		let body = r#"{
			"data": [
				{
					"type": "model",
					"id": "claude-sonnet-4-5-20250929",
					"display_name": "Claude Sonnet 4.5",
					"created_at": "2025-09-29T00:00:00Z",
					"max_input_tokens": 200000,
					"max_tokens": 8192
				},
				{
					"type": "model",
					"id": "claude-haiku-4-5-20251001",
					"display_name": "Claude Haiku 4.5",
					"created_at": "2025-10-01T00:00:00Z",
					"max_input_tokens": 200000,
					"max_tokens": 8192
				},
				{
					"type": "model",
					"id": "older-shape-no-context",
					"display_name": "Older shape",
					"created_at": "2024-01-01T00:00:00Z"
				}
			]
		}"#;

		// Mirror the inline `serde_json::from_str` in
		// `list_models`. We can't call `list_models` directly here
		// without a running HTTP server, so we re-derive the
		// parser shape and assert it produces the right
		// `ProviderModelSummary`s.
		#[derive(serde::Deserialize)]
		struct ListBody {
			#[serde(default)]
			data: Vec<RawModel>,
		}
		#[derive(serde::Deserialize)]
		struct RawModel {
			id: String,
			#[serde(default)]
			display_name: Option<String>,
			#[serde(default)]
			max_input_tokens: Option<u32>,
		}
		let raw: ListBody = serde_json::from_str(body).unwrap();
		let summaries: Vec<ProviderModelSummary> = raw
			.data
			.into_iter()
			.map(|m| ProviderModelSummary {
				id: m.id,
				owned_by: Some("anthropic".into()),
				name: m.display_name,
				context_length: m.max_input_tokens,
				pricing_in_per_million: None,
				pricing_out_per_million: None,
				description: None,
			})
			.collect();

		assert_eq!(summaries.len(), 3);
		assert_eq!(summaries[0].id, "claude-sonnet-4-5-20250929");
		assert_eq!(summaries[0].context_length, Some(200000));
		assert_eq!(summaries[1].id, "claude-haiku-4-5-20251001");
		assert_eq!(summaries[1].context_length, Some(200000));
		assert_eq!(summaries[2].id, "older-shape-no-context");
		assert_eq!(summaries[2].context_length, None);
	}

	#[test]
	fn translate_drops_empty_shell_assistant_messages() {
		// Older runners persisted assistant messages with no
		// text, no thinking, and no tool calls whenever a
		// provider bailed mid-stream. On reload the runner now
		// drops those records, but a session-loader race or a
		// stale in-memory message could still feed one through —
		// the wire translator is the last line of defence.
		// Anthropic 400s with `text content blocks must contain
		// non-whitespace text` if we emit a `" "` no-op block,
		// so the translator drops the message instead.
		let messages = vec![
			ChatMessage::user("hello"),
			ChatMessage::Assistant {
				content: None,
				tool_calls: Vec::new(),
			},
		];
		let translated = translate(&messages, false, false);
		// The empty shell is gone; only the user message survives.
		assert_eq!(translated.messages.len(), 1);
		assert_eq!(translated.messages[0].role, "user");
	}

	#[test]
	fn translate_drops_whitespace_only_assistant_text() {
		// Same shape, but the empty shell came back as
		// `content: Some("   \n\t")` — Anthropic rejects that
		// just like an empty array.
		let messages = vec![
			ChatMessage::user("hello"),
			ChatMessage::Assistant {
				content: Some("   \n\t".into()),
				tool_calls: Vec::new(),
			},
		];
		let translated = translate(&messages, false, false);
		assert_eq!(translated.messages.len(), 1);
		assert_eq!(translated.messages[0].role, "user");
	}

	#[test]
	fn translate_keeps_assistant_tool_only_turns() {
		// Tool-using assistant turns with no text are valid —
		// don't drop them.
		let messages = vec![
			ChatMessage::user("run ls"),
			ChatMessage::Assistant {
				content: None,
				tool_calls: vec![ToolCall {
					id: "call-1".into(),
					kind: "function".into(),
					function: crate::inference::FunctionCall {
						name: "bash".into(),
						arguments: r#"{"command":"ls"}"#.into(),
					},
				}],
			},
		];
		let translated = translate(&messages, false, false);
		assert_eq!(translated.messages.len(), 2);
		assert_eq!(translated.messages[1].role, "assistant");
	}

	#[test]
	fn translate_drops_whitespace_only_user_message_without_images() {
		// Same rule on the user side: an empty / whitespace-only
		// user message with no images is dropped, not papered
		// over with `" "`.
		let messages = vec![
			ChatMessage::user("hello"),
			ChatMessage::User {
				content: "   ".into(),
				images: Vec::new(),
			},
		];
		let translated = translate(&messages, false, false);
		// Only the first user message survives.
		assert_eq!(translated.messages.len(), 1);
	}
}
