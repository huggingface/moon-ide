//! HF Inference Providers HTTP client.
//!
//! OpenAI-compatible API surface against
//! `https://router.huggingface.co/v1`. Authentication uses the OAuth
//! access token from [`crate::auth::Authenticator`]; the client wraps
//! its own `reqwest::Client` and refreshes-on-401 automatically.
//!
//! Both the non-streaming `chat_completion` and the streaming
//! `chat_completion_stream` paths exist. The runner uses the
//! streaming variant for live tokens (Phase 6.1); the non-streaming
//! one stays around for places that don't want a callback shape
//! (sub-agents, future test fixtures).

use std::sync::Arc;

use futures_util::StreamExt as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::auth::Authenticator;
use crate::defaults::HF_ROUTER_BASE;
use crate::error::CoderError;
use crate::models::SharedCoderModels;

/// Some providers (DeepInfra at least) serialize "this chunk has no
/// tool calls" as `tool_calls: null` instead of just omitting the
/// field. Serde's `#[serde(default)]` covers *missing*, not
/// *explicit-null*, so we need a custom deserializer that maps both
/// to `T::default()`. Used on every `Vec` field that's part of an
/// inference response — adding it costs nothing and we'd rather
/// not have streams die mid-token because a provider was generous
/// with `null`s.
fn null_or_missing_as_default<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
	T: Default + Deserialize<'de>,
	D: Deserializer<'de>,
{
	let opt = Option::<T>::deserialize(deserializer)?;
	Ok(opt.unwrap_or_default())
}

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
		#[serde(
			default,
			deserialize_with = "null_or_missing_as_default",
			skip_serializing_if = "Vec::is_empty"
		)]
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
	/// `true` requests SSE deltas. The router enforces "completions
	/// without tool calls return a single delta" so we get the same
	/// shape either way; just buffered when streaming is off.
	stream: bool,
	/// `include_usage: true` makes OpenAI-compatible providers emit
	/// a final SSE chunk with `usage: { prompt_tokens, … }` right
	/// before `[DONE]`. Powers the per-turn token counter and the
	/// auto-compaction trigger. Some providers ignore this flag and
	/// never emit usage; the runner falls back to a bytes/4 estimate
	/// in that case.
	#[serde(skip_serializing_if = "Option::is_none")]
	stream_options: Option<StreamOptions>,
}

#[derive(Debug, Clone, Serialize)]
struct StreamOptions {
	include_usage: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
	pub choices: Vec<Choice>,
	#[serde(default)]
	pub usage: Option<TokenUsage>,
}

/// OpenAI-compatible usage report. `prompt_tokens` is the
/// load-bearing field for the project: it tells us *exactly* how
/// much of the model's context window the next round-trip will
/// have to fit the system prompt + history into. `completion_tokens`
/// is just the model's output for this single response.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TokenUsage {
	#[serde(default)]
	pub prompt_tokens: u32,
	#[serde(default)]
	pub completion_tokens: u32,
	#[serde(default)]
	pub total_tokens: u32,
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
///
/// `thinking` carries the model's reasoning trace when the
/// underlying provider exposes one. Different providers use
/// different field names — DeepSeek and Qwen send
/// `reasoning_content`, others send `reasoning` — so the
/// deserializer accepts both as aliases. We don't echo it back to
/// the model in subsequent chat turns: most providers don't expect
/// their own reasoning in the history (and Anthropic-style
/// "thinking blocks" with crypto-signing are out of scope here).
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantResponse {
	#[serde(default)]
	pub content: Option<String>,
	#[serde(default, alias = "reasoning_content", alias = "reasoning")]
	pub thinking: Option<String>,
	#[serde(default, deserialize_with = "null_or_missing_as_default")]
	pub tool_calls: Vec<ToolCall>,
	/// Provider-reported usage for the round-trip that produced
	/// this response. `None` when the provider didn't emit a
	/// usage chunk; the runner falls back to a bytes/4 estimate
	/// in that case. Skipped on serialization (we don't echo this
	/// back to the model) and not part of the wire `Assistant`
	/// message — see `response_to_message` in `runner.rs`.
	#[serde(default, skip_serializing)]
	pub usage: Option<TokenUsage>,
}

/// One SSE chunk in the OpenAI streaming shape. Fields use the same
/// `delta` indirection: each chunk's `choices[0].delta` carries
/// either a content fragment or a tool-call fragment, never both at
/// once in practice (some providers do mix; the accumulator below
/// handles both).
#[derive(Debug, Clone, Deserialize)]
struct StreamChunk {
	#[serde(default, deserialize_with = "null_or_missing_as_default")]
	choices: Vec<StreamChoice>,
	/// Final-chunk usage report. Only present on the very last
	/// chunk of a stream when `stream_options.include_usage` was
	/// set in the request. Most chunks have `choices` and no
	/// `usage`; the final usage chunk has empty `choices` and a
	/// populated `usage`.
	#[serde(default)]
	usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamChoice {
	#[serde(default)]
	delta: StreamDelta,
	/// Provider-reported reason for the stream end (`stop`,
	/// `tool_calls`, `length`, …). The runner doesn't branch on
	/// this — `tool_calls.is_empty()` already tells us whether to
	/// recurse — but we accept the field so the parser doesn't
	/// reject the chunk that carries it.
	#[serde(default, rename = "finish_reason")]
	#[allow(dead_code)]
	_finish_reason: Option<String>,
}

/// Per-chunk delta. Every field is optional — a chunk may carry
/// just `role`, just `content`, just `reasoning_content`, just
/// `tool_calls`, or some mix.
///
/// `role` itself is not consumed by the runner (we always know we
/// asked for an assistant turn) but we accept the field so its
/// presence in a chunk doesn't trip `deny_unknown_fields` if a
/// future Serde knob turns that on.
///
/// Reasoning streams under one of two field names depending on the
/// provider — DeepSeek / Qwen use `reasoning_content`, others use
/// `reasoning`. We accept both. A single chunk carrying both is
/// theoretical; if it ever lands we concatenate both into the
/// thinking buffer in `accumulate_chunk`.
#[derive(Debug, Clone, Default, Deserialize)]
struct StreamDelta {
	#[serde(default, rename = "role")]
	#[allow(dead_code)]
	_role: Option<String>,
	#[serde(default)]
	content: Option<String>,
	#[serde(default)]
	reasoning_content: Option<String>,
	#[serde(default)]
	reasoning: Option<String>,
	#[serde(default, deserialize_with = "null_or_missing_as_default")]
	tool_calls: Vec<ToolCallDelta>,
}

/// A streaming tool call shows up across multiple chunks. `index`
/// identifies *which* call this fragment belongs to (the model can
/// emit multiple parallel tool calls); `id` and `function.name`
/// arrive on the first fragment, then `function.arguments` arrives
/// piecewise as a partial JSON string we concatenate.
#[derive(Debug, Clone, Deserialize)]
struct ToolCallDelta {
	#[serde(default)]
	index: usize,
	#[serde(default)]
	id: Option<String>,
	#[serde(default, rename = "type")]
	kind: Option<String>,
	#[serde(default)]
	function: Option<FunctionCallDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FunctionCallDelta {
	#[serde(default)]
	name: Option<String>,
	#[serde(default)]
	arguments: Option<String>,
}

/// Inference HTTP client. Cheap to clone; the underlying
/// `reqwest::Client` does its own connection pooling. Holds a
/// reference to the [`SharedCoderModels`] handle so every request
/// reads the current `bill_to` setting without the runner having to
/// thread it through each call site.
#[derive(Clone)]
pub struct InferenceClient {
	http: reqwest::Client,
	auth: Authenticator,
	base_url: String,
	models: SharedCoderModels,
}

impl InferenceClient {
	pub fn new(auth: Authenticator, models: SharedCoderModels) -> Result<Self, CoderError> {
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.build()
			.map_err(CoderError::from)?;
		Ok(Self {
			http,
			auth,
			base_url: HF_ROUTER_BASE.to_string(),
			models,
		})
	}

	pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
		self.base_url = base_url.into();
		self
	}

	/// Snapshot of the `X-HF-Bill-To` value to send on the next
	/// request. Read fresh per request so a settings flip mid-turn
	/// applies to the very next round-trip.
	async fn current_bill_to(&self) -> Option<String> {
		self.models.read().await.bill_to().map(str::to_owned)
	}

	/// GET `/v1/models` → list of [`RouterModel`]s the picker
	/// renders. Auth uses the user's OAuth token; the router gates
	/// model visibility on the token's scopes + the user's plan,
	/// so the list a free user sees is a subset of what a Pro user
	/// sees. We don't try to second-guess that — we just forward.
	///
	/// Bill-to header is **not** sent for the catalog call. The
	/// router returns the same catalog regardless and we want a
	/// failed `coder_set_models { bill_to = "org" }` to still let
	/// the picker reload after the user fixes the org name.
	pub async fn list_models(&self) -> Result<Vec<moon_protocol::coder_models::RouterModel>, CoderError> {
		use moon_protocol::coder_models::{RouterModel, RouterPricing, RouterProvider};
		let token = self.auth.current_access_token().await?;
		let endpoint = format!("{}/models", self.base_url);
		let response = self
			.http
			.get(&endpoint)
			.bearer_auth(&token)
			.send()
			.await
			.map_err(CoderError::from)?;
		let status = response.status();
		let body = response.text().await.map_err(CoderError::from)?;
		if !status.is_success() {
			return Err(CoderError::http(endpoint, status.as_u16(), body));
		}

		// Mirror the wire shape just for the decode step — we
		// translate to the trimmed protocol shape immediately so
		// the rest of the codebase doesn't see the verbose
		// `architecture`/`is_model_author` cruft.
		#[derive(Deserialize)]
		struct ListBody {
			data: Vec<RawModel>,
		}
		#[derive(Deserialize)]
		struct RawModel {
			id: String,
			#[serde(default)]
			owned_by: String,
			#[serde(default)]
			providers: Vec<RawProvider>,
		}
		#[derive(Deserialize)]
		struct RawProvider {
			provider: String,
			#[serde(default)]
			context_length: Option<u32>,
			#[serde(default)]
			supports_tools: bool,
			#[serde(default)]
			pricing: Option<RawPricing>,
			#[serde(default)]
			first_token_latency_ms: Option<f64>,
			#[serde(default)]
			throughput: Option<f64>,
		}
		#[derive(Deserialize)]
		struct RawPricing {
			input: f64,
			output: f64,
		}

		let raw: ListBody = crate::auth::decode_body(&endpoint, &body)?;
		let mut out = Vec::with_capacity(raw.data.len());
		for m in raw.data {
			let providers: Vec<RouterProvider> = m
				.providers
				.into_iter()
				.map(|p| RouterProvider {
					provider: p.provider,
					context_length: p.context_length,
					supports_tools: p.supports_tools,
					pricing: p.pricing.map(|p| RouterPricing {
						input: p.input,
						output: p.output,
					}),
					first_token_latency_ms: p.first_token_latency_ms,
					throughput: p.throughput,
				})
				.collect();
			let supports_tools_anywhere = providers.iter().any(|p| p.supports_tools);
			out.push(RouterModel {
				id: m.id,
				owned_by: m.owned_by,
				supports_tools_anywhere,
				providers,
			});
		}
		Ok(out)
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
			stream: false,
			stream_options: None,
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
		let usage = parsed.usage;
		parsed
			.choices
			.into_iter()
			.next()
			.map(|c| {
				let mut msg = c.message;
				msg.usage = usage;
				msg
			})
			.ok_or_else(|| CoderError::decode(&endpoint, "response had no choices"))
	}

	/// Streaming chat-completions round trip.
	///
	/// Calls `on_event` for every parsed delta as bytes arrive
	/// (`StreamEvent::ContentDelta` / `StreamEvent::ToolCallDelta`),
	/// then returns the assembled [`AssistantResponse`] once the
	/// stream ends. The accumulated response is what the runner
	/// pushes back into the chat history — the UI side already saw
	/// the same content via the events.
	///
	/// Same auth-refresh story as [`Self::chat_completion`]: a 401
	/// triggers one retry. SSE consumption itself is wrapped in
	/// `tokio::select!` against `cancel` so an Esc-abort drops the
	/// connection without waiting for the next chunk.
	pub async fn chat_completion_stream<F>(
		&self,
		model: &str,
		messages: &[ChatMessage],
		tools: &[ToolDefinition],
		cancel: &tokio_util::sync::CancellationToken,
		mut on_event: F,
	) -> Result<AssistantResponse, CoderError>
	where
		F: FnMut(StreamEvent<'_>),
	{
		let endpoint = format!("{}/chat/completions", self.base_url);
		let body = ChatCompletionRequest {
			model,
			messages,
			tools,
			tool_choice: if tools.is_empty() { None } else { Some("auto") },
			stream: true,
			// `include_usage: true` makes the provider emit a final
			// SSE chunk with the round-trip's `prompt_tokens` /
			// `completion_tokens` / `total_tokens`. `consume_sse_stream`
			// captures it; the runner reads
			// `AssistantResponse.usage` to drive the context-usage
			// indicator and the auto-compaction trigger.
			stream_options: Some(StreamOptions { include_usage: true }),
		};

		let access = self.auth.current_access_token().await?;
		let mut response = self.send_once_stream(&endpoint, &access, &body, cancel).await?;

		if response.status() == reqwest::StatusCode::UNAUTHORIZED {
			tracing::info!("inference returned 401; refreshing token and retrying once");
			let refreshed = self.auth.refresh_now().await?;
			response = self.send_once_stream(&endpoint, &refreshed, &body, cancel).await?;
		}

		let status = response.status();
		if !status.is_success() {
			// Drain the body for the error message; failures aren't
			// SSE-shaped, they're a plain JSON error body.
			let recv = response.text();
			let text = tokio::select! {
				biased;
				_ = cancel.cancelled() => return Err(CoderError::Aborted),
				out = recv => out.map_err(CoderError::from)?,
			};
			return Err(CoderError::http(endpoint, status.as_u16(), text));
		}

		consume_sse_stream(response, cancel, |chunk| {
			apply_chunk(chunk, &mut on_event);
		})
		.await
	}

	async fn send_once(
		&self,
		endpoint: &str,
		access_token: &str,
		body: &ChatCompletionRequest<'_>,
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, CoderError> {
		let bill_to = self.current_bill_to().await;
		let mut builder = self.http.post(endpoint).bearer_auth(access_token).json(body);
		if let Some(org) = bill_to {
			builder = builder.header(BILL_TO_HEADER, org);
		}
		let send = builder.send();
		tokio::select! {
			biased;
			_ = cancel.cancelled() => Err(CoderError::Aborted),
			resp = send => resp.map_err(CoderError::from),
		}
	}

	async fn send_once_stream(
		&self,
		endpoint: &str,
		access_token: &str,
		body: &ChatCompletionRequest<'_>,
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, CoderError> {
		// Same shape as `send_once`; a separate method exists only to
		// mirror it — no header difference today *except* the
		// explicit `accept: text/event-stream` to nudge providers
		// that default to JSON, plus the `X-HF-Bill-To` re-read for
		// the streaming path too (`bill_to` flips mid-turn just
		// changes whose pocket the next call comes out of).
		let bill_to = self.current_bill_to().await;
		let mut builder = self
			.http
			.post(endpoint)
			.bearer_auth(access_token)
			.header("accept", "text/event-stream")
			.json(body);
		if let Some(org) = bill_to {
			builder = builder.header(BILL_TO_HEADER, org);
		}
		let send = builder.send();
		tokio::select! {
			biased;
			_ = cancel.cancelled() => Err(CoderError::Aborted),
			resp = send => resp.map_err(CoderError::from),
		}
	}
}

/// Lowercase per the HF docs example (`x-hf-bill-to`). Reqwest is
/// case-insensitive on the way out but we keep the docs' casing for
/// grep-ability against moon-landing's `Middlewares.ts`.
const BILL_TO_HEADER: &str = "x-hf-bill-to";

/// One parsed delta, handed to the streaming caller's callback as
/// bytes arrive. Borrowed strings keep the hot path allocation-free
/// — the runner copies into owned `String`s only when it actually
/// builds a `CoderEvent`.
#[derive(Debug)]
pub enum StreamEvent<'a> {
	/// Append `delta` to the assistant's text content.
	ContentDelta { delta: &'a str },
	/// Append `delta` to the assistant's *reasoning* trace.
	/// Provider-dependent: DeepSeek / Qwen reasoning models stream
	/// thinking under `reasoning_content`, others under
	/// `reasoning`. Both field names map to the same callback shape
	/// here. Models that don't expose reasoning at all simply never
	/// fire this variant.
	ThinkingDelta { delta: &'a str },
	/// A tool-call fragment landed. Mostly informational — the
	/// runner does not surface these; the registry only dispatches
	/// once the whole call is assembled at end-of-stream.
	ToolCallDelta {
		index: usize,
		id: Option<&'a str>,
		name: Option<&'a str>,
		arguments_delta: Option<&'a str>,
	},
}

/// Pulls the SSE byte stream off `response` and feeds parsed chunks
/// to `on_chunk`. The accumulator state (`content_buf`,
/// `thinking_buf`, `tool_call_bufs`) lives here too so the public
/// API stays a single async function returning the assembled
/// [`AssistantResponse`].
async fn consume_sse_stream<F>(
	response: reqwest::Response,
	cancel: &tokio_util::sync::CancellationToken,
	mut on_chunk: F,
) -> Result<AssistantResponse, CoderError>
where
	F: FnMut(&StreamChunk),
{
	let mut content_buf = String::new();
	let mut thinking_buf = String::new();
	let mut tool_call_bufs: Vec<ToolCallBuffer> = Vec::new();
	let mut usage: Option<TokenUsage> = None;
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

		// Drain whole events out of the buffer. SSE event boundaries
		// are `\n\n` (and `\r\n\r\n` for chunked transfers via some
		// proxies); we accept either by matching on the trailing
		// blank line.
		while let Some(end) = find_event_boundary(&sse_buf) {
			let event_bytes = sse_buf.drain(..end.boundary_end).collect::<Vec<u8>>();
			let event_text = std::str::from_utf8(&event_bytes[..end.body_end])
				.map_err(|err| CoderError::decode("inference stream", format!("invalid utf-8 in SSE event: {err}")))?;
			for data in extract_data_lines(event_text) {
				if data == "[DONE]" {
					return Ok(finalize_response(content_buf, thinking_buf, tool_call_bufs, usage));
				}
				let chunk: StreamChunk = serde_json::from_str(data).map_err(|err| {
					CoderError::decode(
						"inference stream",
						format!("could not parse SSE chunk: {err}; raw={}", truncate_for_log(data)),
					)
				})?;
				accumulate_chunk(&chunk, &mut content_buf, &mut thinking_buf, &mut tool_call_bufs);
				if let Some(u) = chunk.usage {
					// Last-write-wins: providers occasionally emit a
					// usage block on multiple chunks (e.g. thinking
					// vs final phases). The terminal chunk's numbers
					// are what we care about.
					usage = Some(u);
				}
				on_chunk(&chunk);
			}
		}
	}

	// Some providers close the stream without an explicit `[DONE]`
	// — treat clean EOF as success.
	Ok(finalize_response(content_buf, thinking_buf, tool_call_bufs, usage))
}

/// Working state for one in-progress tool call. Only `arguments` is
/// genuinely delta-streamed (the provider chunks the JSON-encoded
/// argument string into arbitrary slices); `id`, `kind`, and `name`
/// are *set-once* identifiers. The OpenAI-compatible chat-completions
/// SSE schema doesn't strictly require these to appear on the first
/// chunk only, and providers routed through HF Inference vary in
/// practice — some send `id` once, others re-emit the full value on
/// every chunk for idempotence. Concatenating those re-sends would
/// bloat the call `id` to hundreds of KB and re-feed the bloated id
/// on every subsequent prompt, so the accumulator overwrites rather
/// than appends.
#[derive(Debug, Default)]
struct ToolCallBuffer {
	id: String,
	kind: String,
	name: String,
	arguments: String,
}

fn accumulate_chunk(
	chunk: &StreamChunk,
	content: &mut String,
	thinking: &mut String,
	tool_calls: &mut Vec<ToolCallBuffer>,
) {
	let Some(choice) = chunk.choices.first() else {
		return;
	};
	if let Some(text) = choice.delta.content.as_deref() {
		content.push_str(text);
	}
	if let Some(text) = choice.delta.reasoning_content.as_deref() {
		thinking.push_str(text);
	}
	if let Some(text) = choice.delta.reasoning.as_deref() {
		thinking.push_str(text);
	}
	for tc in &choice.delta.tool_calls {
		while tool_calls.len() <= tc.index {
			tool_calls.push(ToolCallBuffer::default());
		}
		let slot = &mut tool_calls[tc.index];
		if let Some(id) = tc.id.as_deref() {
			if !id.is_empty() {
				id.clone_into(&mut slot.id);
			}
		}
		if let Some(kind) = tc.kind.as_deref() {
			if !kind.is_empty() {
				kind.clone_into(&mut slot.kind);
			}
		}
		if let Some(func) = tc.function.as_ref() {
			if let Some(name) = func.name.as_deref() {
				if !name.is_empty() {
					name.clone_into(&mut slot.name);
				}
			}
			if let Some(args) = func.arguments.as_deref() {
				slot.arguments.push_str(args);
			}
		}
	}
}

fn apply_chunk<F>(chunk: &StreamChunk, on_event: &mut F)
where
	F: FnMut(StreamEvent<'_>),
{
	let Some(choice) = chunk.choices.first() else {
		return;
	};
	if let Some(text) = choice.delta.content.as_deref() {
		if !text.is_empty() {
			on_event(StreamEvent::ContentDelta { delta: text });
		}
	}
	if let Some(text) = choice.delta.reasoning_content.as_deref() {
		if !text.is_empty() {
			on_event(StreamEvent::ThinkingDelta { delta: text });
		}
	}
	if let Some(text) = choice.delta.reasoning.as_deref() {
		if !text.is_empty() {
			on_event(StreamEvent::ThinkingDelta { delta: text });
		}
	}
	for tc in &choice.delta.tool_calls {
		on_event(StreamEvent::ToolCallDelta {
			index: tc.index,
			id: tc.id.as_deref(),
			name: tc.function.as_ref().and_then(|f| f.name.as_deref()),
			arguments_delta: tc.function.as_ref().and_then(|f| f.arguments.as_deref()),
		});
	}
}

fn finalize_response(
	content: String,
	thinking: String,
	tool_calls: Vec<ToolCallBuffer>,
	usage: Option<TokenUsage>,
) -> AssistantResponse {
	AssistantResponse {
		content: if content.is_empty() { None } else { Some(content) },
		thinking: if thinking.is_empty() { None } else { Some(thinking) },
		tool_calls: tool_calls
			.into_iter()
			.filter(|b| !b.id.is_empty() || !b.name.is_empty())
			.map(|b| ToolCall {
				id: b.id,
				kind: if b.kind.is_empty() { default_tool_type() } else { b.kind },
				function: FunctionCall {
					name: b.name,
					arguments: b.arguments,
				},
			})
			.collect(),
		usage,
	}
}

#[derive(Debug)]
struct EventBoundary {
	/// Offset (exclusive) of the last byte of the event body — i.e.
	/// the position of the trailing `\n` that immediately precedes
	/// the blank-line separator.
	body_end: usize,
	/// Offset (exclusive) of the byte *after* the blank-line
	/// separator. Drain `0..boundary_end` to consume the event.
	boundary_end: usize,
}

/// Find the next `\n\n` (or `\r\n\r\n`) boundary in the buffer.
/// Returns `None` when the buffer doesn't yet contain a complete
/// event — the caller pulls more bytes and tries again.
fn find_event_boundary(buf: &[u8]) -> Option<EventBoundary> {
	if let Some(idx) = find_subsequence(buf, b"\r\n\r\n") {
		return Some(EventBoundary {
			body_end: idx,
			boundary_end: idx + 4,
		});
	}
	if let Some(idx) = find_subsequence(buf, b"\n\n") {
		return Some(EventBoundary {
			body_end: idx,
			boundary_end: idx + 2,
		});
	}
	None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|w| w == needle)
}

/// Pull every `data: ...` line out of one SSE event. Lines starting
/// with `:` are comments (provider keep-alives); blank lines never
/// reach this function because `find_event_boundary` already
/// trimmed at the boundary.
fn extract_data_lines(event: &str) -> Vec<&str> {
	let mut out = Vec::new();
	for line in event.split('\n') {
		let line = line.strip_suffix('\r').unwrap_or(line);
		if line.is_empty() {
			continue;
		}
		if line.starts_with(':') {
			continue;
		}
		if let Some(rest) = line.strip_prefix("data:") {
			out.push(rest.strip_prefix(' ').unwrap_or(rest));
		}
	}
	out
}

fn truncate_for_log(s: &str) -> String {
	const LIMIT: usize = 256;
	if s.len() <= LIMIT {
		return s.to_string();
	}
	let mut idx = LIMIT;
	while idx > 0 && !s.is_char_boundary(idx) {
		idx -= 1;
	}
	format!("{}…", &s[..idx])
}

/// Convenience wrapper used by the runner so the type can be dropped
/// through `Arc<...>` without dragging the auth handle along
/// separately.
pub type SharedInference = Arc<InferenceClient>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extract_data_skips_comments_and_keepalives() {
		let event = ": ping\ndata: hello\n";
		assert_eq!(extract_data_lines(event), vec!["hello"]);
	}

	#[test]
	fn extract_data_handles_multi_data_lines() {
		// SSE allows multiple `data:` lines per event; the spec
		// joins them with `\n`. OpenAI doesn't do this in practice,
		// but supporting it is free and prevents a future provider
		// change from breaking us.
		let event = "data: a\ndata: b\n";
		assert_eq!(extract_data_lines(event), vec!["a", "b"]);
	}

	#[test]
	fn finalize_response_drops_empty_buffers() {
		// Some providers emit a "warm-up" tool-call slot that never
		// gets an id or name. Filter it so the chat-history append
		// doesn't carry a phantom call.
		let buf = vec![ToolCallBuffer::default()];
		let resp = finalize_response(String::new(), String::new(), buf, None);
		assert!(resp.tool_calls.is_empty());
		assert!(resp.content.is_none());
		assert!(resp.thinking.is_none());
	}

	#[test]
	fn stream_chunk_parses_explicit_nulls_for_array_fields() {
		// DeepInfra (and likely others) serialize "no value" as an
		// explicit `null` rather than omitting the field. Without
		// the `null_or_missing_as_default` deserializer this chunk
		// rejects with "invalid type: null, expected a sequence"
		// and the stream dies mid-token. Pin the behaviour.
		let raw = r#"{
			"choices": [{
				"delta": {
					"role": "assistant",
					"content": "",
					"reasoning_content": null,
					"tool_calls": null
				},
				"finish_reason": null
			}]
		}"#;
		let chunk: StreamChunk = serde_json::from_str(raw).expect("parses with explicit nulls");
		assert_eq!(chunk.choices.len(), 1);
		assert!(chunk.choices[0].delta.tool_calls.is_empty());

		// A `choices: null` chunk (some providers emit one as the
		// final usage-only frame) also has to round-trip.
		let usage_chunk = r#"{"choices": null, "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}"#;
		let chunk: StreamChunk = serde_json::from_str(usage_chunk).expect("parses choices: null");
		assert!(chunk.choices.is_empty());
		assert!(chunk.usage.is_some());
	}

	#[test]
	fn accumulate_chunk_concatenates_arguments() {
		// Realistic streaming sequence: id + name in chunk 1,
		// arguments split across two chunks.
		let chunks = [
			r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
			r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\""}}]}}]}"#,
			r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"foo.rs\"}"}}]}}]}"#,
		];
		let mut content = String::new();
		let mut thinking = String::new();
		let mut tcs = Vec::new();
		for raw in chunks {
			let chunk: StreamChunk = serde_json::from_str(raw).unwrap();
			accumulate_chunk(&chunk, &mut content, &mut thinking, &mut tcs);
		}
		let resp = finalize_response(content, thinking, tcs, None);
		assert_eq!(resp.tool_calls.len(), 1);
		assert_eq!(resp.tool_calls[0].id, "call_x");
		assert_eq!(resp.tool_calls[0].function.name, "read_file");
		assert_eq!(resp.tool_calls[0].function.arguments, r#"{"path":"foo.rs"}"#);
	}

	#[test]
	fn accumulate_chunk_set_once_for_id_and_name_when_provider_re_emits() {
		// Some providers routed through HF Inference re-send the
		// full `id` (and sometimes `name`) on every delta chunk for
		// idempotence, not just the first. A naive `push_str`
		// accumulator concatenates them and ships >100 KB tool-call
		// ids back to the model on the next iteration — confirmed
		// in the field on real sessions. Pin the set-once
		// behaviour: id, type, and name overwrite; only arguments
		// accumulate.
		let id_chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"write_file","arguments":""}}]}}]}"#;
		let arg_chunk_with_id = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"write_file","arguments":"a"}}]}}]}"#;
		let mut content = String::new();
		let mut thinking = String::new();
		let mut tcs = Vec::new();
		// One initial chunk plus a thousand follow-up chunks each
		// re-emitting the full id/name alongside one byte of args.
		let chunk: StreamChunk = serde_json::from_str(id_chunk).unwrap();
		accumulate_chunk(&chunk, &mut content, &mut thinking, &mut tcs);
		for _ in 0..1000 {
			let chunk: StreamChunk = serde_json::from_str(arg_chunk_with_id).unwrap();
			accumulate_chunk(&chunk, &mut content, &mut thinking, &mut tcs);
		}
		let resp = finalize_response(content, thinking, tcs, None);
		assert_eq!(resp.tool_calls.len(), 1);
		assert_eq!(
			resp.tool_calls[0].id, "call_x",
			"id must not concatenate across re-emits"
		);
		assert_eq!(
			resp.tool_calls[0].function.name, "write_file",
			"name must not concatenate across re-emits"
		);
		assert_eq!(
			resp.tool_calls[0].kind, "function",
			"type must not concatenate across re-emits"
		);
		assert_eq!(
			resp.tool_calls[0].function.arguments.len(),
			1000,
			"arguments is the only field that should accumulate"
		);
	}

	#[test]
	fn accumulate_chunk_collects_reasoning_under_either_field_name() {
		// Some providers emit `reasoning_content` (DeepSeek, Qwen),
		// others use `reasoning`. We accept both; concatenation
		// order follows wire order.
		let chunks = [
			r#"{"choices":[{"delta":{"reasoning_content":"Let me think. "}}]}"#,
			r#"{"choices":[{"delta":{"reasoning":"Maybe a "}}]}"#,
			r#"{"choices":[{"delta":{"reasoning_content":"plan helps."}}]}"#,
			r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
		];
		let mut content = String::new();
		let mut thinking = String::new();
		let mut tcs = Vec::new();
		for raw in chunks {
			let chunk: StreamChunk = serde_json::from_str(raw).unwrap();
			accumulate_chunk(&chunk, &mut content, &mut thinking, &mut tcs);
		}
		let resp = finalize_response(content, thinking, tcs, None);
		assert_eq!(resp.content.as_deref(), Some("Hello"));
		assert_eq!(resp.thinking.as_deref(), Some("Let me think. Maybe a plan helps."));
	}

	#[test]
	fn assistant_response_message_form_accepts_reasoning_alias() {
		// Non-streaming response shape: the underlying provider
		// can return reasoning under either field name, and we
		// must round-trip it as `thinking`.
		let raw = r#"{"content":"hi","reasoning_content":"thought trail"}"#;
		let resp: AssistantResponse = serde_json::from_str(raw).unwrap();
		assert_eq!(resp.content.as_deref(), Some("hi"));
		assert_eq!(resp.thinking.as_deref(), Some("thought trail"));
	}

	#[test]
	fn find_event_boundary_handles_lf_and_crlf() {
		assert!(find_event_boundary(b"data: x\n\nrest").is_some());
		assert!(find_event_boundary(b"data: x\r\n\r\nrest").is_some());
		assert!(find_event_boundary(b"data: x\n").is_none());
	}
}
