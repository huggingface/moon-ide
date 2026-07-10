//! Inference HTTP client.
//!
//! OpenAI-compatible API surface. Two routing modes:
//!
//! - **Hugging Face** (default): `https://router.huggingface.co/v1`,
//!   OAuth access token from [`crate::auth::Authenticator`] with
//!   refresh-on-401, and `X-HF-Bill-To` on every request.
//! - **Custom OpenAI-compatible** (OpenRouter, local vLLM / Ollama /
//!   llama.cpp, …): user-supplied `base_url`, optional
//!   `Bearer <api_key>` drawn from
//!   [`crate::providers::ProviderKeyring`], no bill-to. No
//!   automatic refresh — a 401 from a user provider means the key
//!   is wrong / revoked and the user has to fix it in the picker.
//!
//! The route is resolved off [`SharedCoderModels::resolve_route`]
//! once per request, so a settings flip mid-turn applies on the
//! very next call.
//!
//! Both the non-streaming `chat_completion` and the streaming
//! `chat_completion_stream` paths exist. The runner uses the
//! streaming variant for live tokens (Phase 6.1); the non-streaming
//! one stays around for places that don't want a callback shape
//! (sub-agents, future test fixtures).

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::auth::Authenticator;
use crate::defaults::HF_ROUTER_BASE;
use crate::error::{request_id_of, CoderError};
use crate::models::{ResolvedProvider, SharedCoderModels};
use crate::providers::ProviderKeyring;

/// Ceiling on the TCP+TLS connect phase for inference HTTP requests.
/// Protects against black-holed endpoints that accept the connection
/// then never respond — without this a stuck handshake parks the turn
/// in an uncancellable state. See [`InferenceClient::new`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-request timeout for non-streaming inference calls (the
/// `chat_completion` JSON path and the `/v1/models` catalog fetch).
/// Streaming sends deliberately omit this — a long generation is
/// legitimate and is bounded by the cancel token + SSE liveness
/// instead.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

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

/// One image the user attached to a prompt (typically by pasting a
/// screenshot into the composer). `data_url` is the canonical
/// representation — `data:<mime>;base64,<payload>` — and is what
/// gets shipped to the model verbatim inside an `image_url` content
/// block. `mime` is cached separately so we don't have to re-parse
/// the prefix every time we serialise. We never store the raw
/// bytes: the data-URL form is what providers want on the wire and
/// what the JSONL transcript replays back into context, so going
/// through bytes would be an extra encode/decode round-trip with no
/// upside.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageAttachment {
	pub data_url: String,
	pub mime: String,
}

/// One reasoning block the model emitted on an assistant turn,
/// preserved verbatim so it can be replayed back to the provider on
/// the next round-trip.
///
/// **Only the native Anthropic path (`kind=anthropic`) populates
/// these.** Anthropic's extended / adaptive thinking models
/// (Fable 5, Mythos 5, Opus 4.7+, …) emit cryptographically signed
/// `thinking` blocks, and the Messages API *requires* the unmodified
/// block to be echoed back in the assistant turn that precedes a
/// `tool_result` — drop it and the next tool round-trip 400s with
/// `thinking blocks in the latest assistant message cannot be
/// modified`. We can't reconstruct the signature, so we carry the
/// whole block opaque from one turn to the next.
///
/// Every other provider (HF router, OpenAI-compat custom, OpenRouter)
/// leaves this empty: their reasoning either isn't signed or isn't
/// required on replay, so it rides in the human-readable `thinking`
/// string instead and never round-trips. Because the field is
/// `skip_serializing_if = "Vec::is_empty"` everywhere it appears,
/// non-Anthropic wire bodies stay byte-for-byte unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingBlock {
	/// A regular thinking block. `thinking` is the (possibly empty,
	/// when `display: "omitted"`) summary text; `signature` is the
	/// opaque encrypted full-thinking token the API hands back and
	/// expects verbatim on replay.
	Thinking {
		#[serde(default)]
		thinking: String,
		#[serde(default)]
		signature: String,
	},
	/// A safety-redacted thinking block. `data` is opaque encrypted
	/// content with no readable summary; replay it unchanged.
	RedactedThinking {
		#[serde(default)]
		data: String,
	},
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
		/// Images the user attached to this prompt. Empty for the
		/// vast majority of messages; non-empty only when the user
		/// pasted (or otherwise dropped) an image into the composer.
		/// On the wire we hoist these into the content-as-blocks
		/// shape (`{"type":"image_url", ...}`), so a User with no
		/// images keeps emitting the cheap string-content payload
		/// that every router has prefix-cached.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		images: Vec<ImageAttachment>,
	},
	Assistant {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		content: Option<String>,
		/// Signed/redacted reasoning blocks from the native Anthropic
		/// path, replayed verbatim before the `tool_use` blocks on the
		/// next round-trip. Empty for every other provider, so the
		/// OpenAI-compat wire body is unaffected (see [`ThinkingBlock`]).
		#[serde(
			default,
			deserialize_with = "null_or_missing_as_default",
			skip_serializing_if = "Vec::is_empty"
		)]
		thinking_blocks: Vec<ThinkingBlock>,
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

impl ChatMessage {
	/// Convenience constructor for a text-only user message — the
	/// shape every caller wants when there's no pasted image.
	/// Lets us keep the struct-with-images variant explicit at the
	/// one site (the composer) that actually attaches images.
	pub fn user(content: impl Into<String>) -> Self {
		Self::User {
			content: content.into(),
			images: Vec::new(),
		}
	}
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

/// Wire-format message: the JSON shape we actually serialise and
/// send to the provider. Differs from [`ChatMessage`] only in
/// that `content` can be a blocks array — that's how Anthropic
/// prompt-caching markers ride on an OpenAI-compatible request
/// body (via OpenRouter routing to `anthropic/*`). Non-caching
/// providers see byte-for-byte the same wire shape they did
/// before because [`WireContent::String`] serialises untagged.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum WireMessage<'a> {
	System {
		content: WireContent<'a>,
	},
	User {
		content: WireContent<'a>,
	},
	Assistant {
		#[serde(skip_serializing_if = "Option::is_none")]
		content: Option<&'a str>,
		#[serde(skip_serializing_if = "<[ToolCall]>::is_empty")]
		tool_calls: &'a [ToolCall],
	},
	Tool {
		tool_call_id: &'a str,
		content: WireContent<'a>,
	},
}

/// A message's `content` field on the wire. `String` is the
/// common path (everything we used to send for text-only
/// messages with no caching); `Blocks` kicks in when we want to
/// attach `cache_control` to a text block, when the user pasted
/// images into the prompt, or both. Untagged so Serde picks the
/// variant by JSON shape.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
	String(&'a str),
	Blocks(Vec<WireBlock<'a>>),
}

/// One block inside a content-as-array message. `text` blocks
/// carry prose (and optionally an Anthropic `cache_control`
/// marker); `image_url` blocks carry user-attached images as
/// data URLs. Both shapes are OpenAI vision-API compatible —
/// OpenRouter normalises `image_url` into Anthropic's `image`
/// block on the way through.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock<'a> {
	Text {
		text: &'a str,
		#[serde(skip_serializing_if = "Option::is_none")]
		cache_control: Option<CacheControl>,
	},
	ImageUrl {
		image_url: WireImageUrl<'a>,
	},
}

#[derive(Debug, Clone, Serialize)]
struct WireImageUrl<'a> {
	url: &'a str,
}

/// Anthropic prompt-caching marker. `ephemeral` is the only
/// `type` OpenRouter/Anthropic accept today; we leave `ttl` off
/// so it defaults to 5 minutes (which is plenty — the next turn
/// in an active conversation always lands well inside that
/// window, and the 1h TTL costs 5x more on the cache-write side
/// without buying us anything for an interactive agent loop).
#[derive(Debug, Clone, Copy, Serialize)]
struct CacheControl {
	#[serde(rename = "type")]
	kind: &'static str,
}

impl CacheControl {
	const EPHEMERAL: Self = Self { kind: "ephemeral" };
}

/// Build the wire message list from a slice of [`ChatMessage`]s,
/// optionally attaching `cache_control: ephemeral` to the
/// indexes listed in `cached_indexes`. Pass an empty slice to
/// keep the original string-content shape — that's what every
/// non-Anthropic round-trip does, so we keep the cheap path
/// cheap.
fn build_wire_messages<'a>(messages: &'a [ChatMessage], cached_indexes: &[usize]) -> Vec<WireMessage<'a>> {
	let mut out = Vec::with_capacity(messages.len());
	for (idx, msg) in messages.iter().enumerate() {
		let cache_here = cached_indexes.contains(&idx);
		let wire = match msg {
			ChatMessage::System { content } => WireMessage::System {
				content: wire_text_content(content, cache_here),
			},
			ChatMessage::User { content, images } => WireMessage::User {
				content: wire_user_content(content, images, cache_here),
			},
			ChatMessage::Assistant {
				content, tool_calls, ..
			} => WireMessage::Assistant {
				// `thinking_blocks` is Anthropic-native-only and never
				// rides on the OpenAI-compat wire, so it's dropped here.
				// `cache_breakpoint_indexes` never targets an
				// assistant turn (it skips backwards to the
				// previous tool / user message), so `cache_here`
				// is `false` for every assistant message and we
				// keep the simple string-content shape.
				content: content.as_deref(),
				tool_calls,
			},
			ChatMessage::Tool { tool_call_id, content } => WireMessage::Tool {
				tool_call_id,
				content: wire_text_content(content, cache_here),
			},
		};
		out.push(wire);
	}
	out
}

fn wire_text_content(content: &str, cache_here: bool) -> WireContent<'_> {
	if !cache_here {
		return WireContent::String(content);
	}
	WireContent::Blocks(vec![WireBlock::Text {
		text: content,
		cache_control: Some(CacheControl::EPHEMERAL),
	}])
}

/// Build the wire content for a User message, hoisting into the
/// blocks shape whenever there's an image attached or a cache
/// marker to apply. The text block always lands first; images
/// follow in attachment order. `cache_control` rides on the text
/// block so the cache write captures the prose half of the turn.
fn wire_user_content<'a>(content: &'a str, images: &'a [ImageAttachment], cache_here: bool) -> WireContent<'a> {
	if images.is_empty() {
		return wire_text_content(content, cache_here);
	}
	let mut blocks: Vec<WireBlock<'a>> = Vec::with_capacity(images.len() + 1);
	blocks.push(WireBlock::Text {
		text: content,
		cache_control: cache_here.then_some(CacheControl::EPHEMERAL),
	});
	for img in images {
		blocks.push(WireBlock::ImageUrl {
			image_url: WireImageUrl { url: &img.data_url },
		});
	}
	WireContent::Blocks(blocks)
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
	messages: Vec<WireMessage<'a>>,
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
///
/// `cache_read_input_tokens` / `cache_creation_input_tokens` are
/// Anthropic's prompt-caching split, exposed verbatim by
/// OpenRouter when the request used `cache_control: ephemeral`
/// markers. They're a *breakdown* of `prompt_tokens`, not in
/// addition to it — i.e. `prompt_tokens` is the full input,
/// `cache_read_input_tokens` says "X of those were served from
/// cache at the 90 % discount" and `cache_creation_input_tokens`
/// says "Y of those were written to cache at a 25 % surcharge".
/// Default `0` for every non-Anthropic provider (they don't emit
/// the fields) and for Anthropic requests that didn't hit any
/// cache yet.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct TokenUsage {
	#[serde(default)]
	pub prompt_tokens: u32,
	#[serde(default)]
	pub completion_tokens: u32,
	#[serde(default)]
	pub total_tokens: u32,
	#[serde(default)]
	pub cache_read_input_tokens: u32,
	#[serde(default)]
	pub cache_creation_input_tokens: u32,
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
/// deserializer accepts both as aliases. We don't echo this string
/// back to the model: most providers don't expect their own
/// reasoning in the history. The native Anthropic path is the
/// exception — its signed reasoning rides in `thinking_blocks`
/// (not this string) and *is* replayed verbatim, because the
/// Messages API requires it on tool round-trips.
#[derive(Debug, Clone, Deserialize)]
pub struct AssistantResponse {
	#[serde(default)]
	pub content: Option<String>,
	#[serde(default, alias = "reasoning_content", alias = "reasoning")]
	pub thinking: Option<String>,
	/// Signed/redacted reasoning blocks, populated only by the native
	/// Anthropic path. Carried alongside the human-readable `thinking`
	/// string because Anthropic requires them echoed back verbatim on
	/// the next tool round-trip (see [`ThinkingBlock`]).
	#[serde(default, deserialize_with = "null_or_missing_as_default", skip_serializing)]
	pub thinking_blocks: Vec<ThinkingBlock>,
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
	/// Provider-reported stop reason, normalised to pi's
	/// `stopReason` vocabulary via [`normalize_stop_reason`]
	/// (`stop` | `length` | `toolUse` | `error` | `aborted`).
	/// Populated by each provider's finalize path; `None` only on
	/// the test fixtures that build a response by hand. Persisted
	/// onto the session JSONL's assistant row (not echoed back to
	/// the model — skipped on serialization like `usage`).
	#[serde(default, skip_serializing)]
	pub stop_reason: Option<String>,
}

/// Normalise a provider-reported finish/stop reason into pi's
/// `stopReason` vocabulary: `stop` | `length` | `toolUse` |
/// `error` | `aborted`. Covers the OpenAI chat-completions values
/// (`stop` / `length` / `tool_calls` / `content_filter`) and the
/// Anthropic Messages values (`end_turn` / `max_tokens` /
/// `stop_sequence` / `tool_use` / `pause_turn` / `refusal`). When
/// the provider omitted the field or sent something we don't
/// recognise, fall back to the structural signal: a turn that
/// emitted tool calls is `toolUse`, otherwise `stop`.
pub(crate) fn normalize_stop_reason(raw: Option<&str>, has_tool_calls: bool) -> String {
	let mapped = match raw.map(str::trim) {
		Some("tool_calls") | Some("tool_use") => Some("toolUse"),
		Some("length") | Some("max_tokens") | Some("model_length") => Some("length"),
		Some("content_filter") | Some("refusal") => Some("error"),
		Some("stop") | Some("end_turn") | Some("stop_sequence") | Some("pause_turn") => Some("stop"),
		_ => None,
	};
	mapped
		.unwrap_or(if has_tool_calls { "toolUse" } else { "stop" })
		.to_string()
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
	/// this to decide recursion — `tool_calls.is_empty()` already
	/// tells us that — but the streaming accumulator captures the
	/// last non-null value to stamp `stopReason` on the persisted
	/// assistant record.
	#[serde(default)]
	finish_reason: Option<String>,
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
/// `reqwest::Client` does its own connection pooling. Holds
/// references to:
///
/// - [`Authenticator`] for HF OAuth tokens (refresh-on-401),
/// - [`SharedCoderModels`] for the active route + `bill_to`,
/// - [`ProviderKeyring`] for per-provider API keys.
///
/// Every request reads the active route fresh off [`SharedCoderModels`]
/// so a settings flip mid-turn applies to the very next round-trip.
#[derive(Clone)]
pub struct InferenceClient {
	http: reqwest::Client,
	auth: Authenticator,
	/// HF default override, only consulted when the resolved route
	/// is [`ResolvedProvider::HuggingFace`]. Tests inject a mock
	/// router here via [`Self::with_hf_base_url`]; the production
	/// code leaves it at [`HF_ROUTER_BASE`].
	hf_base_url: String,
	models: SharedCoderModels,
	provider_keys: ProviderKeyring,
}

/// Resolved request routing for one round-trip.
///
/// Built by [`InferenceClient::resolve_route_for_request`] off
/// [`SharedCoderModels::resolve_route`] + a keyring lookup. The
/// `kind` discriminator drives both the wire shape (OpenAI-compat
/// vs Anthropic native) and the auth-failure recovery story (HF
/// gets one transparent token refresh; everything else surfaces
/// 401 verbatim so the user fixes the key in the picker).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedRoute {
	pub(crate) base_url: String,
	pub(crate) auth_token: Option<String>,
	pub(crate) bill_to: Option<String>,
	pub(crate) kind: RouteKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteKind {
	HuggingFace,
	Custom,
	OpenRouter,
	Anthropic,
}

impl ResolvedRoute {
	pub(crate) fn is_huggingface(&self) -> bool {
		matches!(self.kind, RouteKind::HuggingFace)
	}
}

/// Decide which message indexes should carry an Anthropic
/// `cache_control: ephemeral` marker for this request. Returns
/// an empty list when the route/model don't support prompt
/// caching (every non-OpenRouter route, plus OpenRouter routes
/// to non-Anthropic models) — that's the path that preserves
/// the original string-content wire shape.
///
/// ## Strategy
///
/// Two breakpoints, well under Anthropic's 4-per-request cap:
///
/// 1. **System prompt** (index 0). The biggest static piece in
///    every request — about 6–8 K tokens of base prompt + folder
///    summary + bound-folders block. Marking it once means every
///    subsequent call within the 5-min TTL reads the entire
///    system prompt off cache at a 90 % discount instead of
///    paying full input price.
///
/// 2. **Last non-assistant message in the list** (the most
///    recent user prompt or tool result). The cache is written
///    for the entire prefix up to and including this marker;
///    the *next* round-trip's prefix is exactly "this prefix
///    plus the new assistant turn plus any new tool results",
///    so the longest-matching-prefix lookup at the start of
///    that next call comes back as a hit. Assistant turns get
///    skipped because our [`WireMessage::Assistant`] keeps
///    string-content (assistant content is often empty when the
///    model emitted tool calls only — there's no text block to
///    attach `cache_control` to), and walking back to the
///    previous tool / user message lands on something that
///    always has non-empty string content we *can* mark.
///
/// Anthropic silently ignores cache breakpoints on spans below
/// the 1024-token minimum, so a very short conversation pays no
/// cache-write surcharge and gets no hits — no special-case
/// needed here.
fn cache_breakpoint_indexes(messages: &[ChatMessage], route: &ResolvedRoute, model: &str) -> Vec<usize> {
	if !supports_anthropic_caching(route, model) {
		return Vec::new();
	}
	if messages.is_empty() {
		return Vec::new();
	}
	let mut indexes = vec![0_usize];
	let last = messages.len() - 1;
	if last == 0 {
		return indexes;
	}
	let mut anchor = last;
	while anchor > 0 && matches!(messages[anchor], ChatMessage::Assistant { .. }) {
		anchor -= 1;
	}
	if anchor > 0 {
		indexes.push(anchor);
	}
	indexes
}

/// True when the resolved route is OpenRouter (`openrouter.ai`
/// in the base URL) and the model id looks like an Anthropic
/// model (`anthropic/...` — the slug OpenRouter exposes for
/// every Claude variant). OpenRouter is currently the only
/// path through which we hit Anthropic models: the HF router
/// doesn't proxy them, and a "custom" provider pointing
/// directly at `api.anthropic.com` would need a heavier
/// translation layer (the native `/v1/messages` shape diverges
/// from `/v1/chat/completions` in too many places to fake).
fn supports_anthropic_caching(route: &ResolvedRoute, model: &str) -> bool {
	if !matches!(route.kind, RouteKind::OpenRouter) {
		return false;
	}
	is_anthropic_model(model)
}

fn is_anthropic_model(model: &str) -> bool {
	model.starts_with("anthropic/")
}

impl InferenceClient {
	pub fn new(
		auth: Authenticator,
		models: SharedCoderModels,
		provider_keys: ProviderKeyring,
	) -> Result<Self, CoderError> {
		// `connect_timeout` caps the TCP+TLS handshake so a black-holed
		// endpoint (proxy that accepts the connection then stalls, DNS
		// hang, …) can't park a turn forever. We deliberately do NOT set a
		// client-wide `.timeout()` here: a streaming generation can run
		// for minutes and a blanket timeout would kill legitimate long
		// turns. Non-streaming sends opt into a per-request timeout
		// instead (see `send_once`). Streaming sends rely on the
		// cancel-token `select!` plus the SSE read loop's own liveness.
		let http = reqwest::Client::builder()
			.user_agent(concat!("moon-ide/", env!("CARGO_PKG_VERSION")))
			.connect_timeout(CONNECT_TIMEOUT)
			.build()
			.map_err(CoderError::from)?;
		Ok(Self {
			http,
			auth,
			hf_base_url: HF_ROUTER_BASE.to_string(),
			models,
			provider_keys,
		})
	}

	/// Override the HF router base URL. Test-only — production
	/// uses [`HF_ROUTER_BASE`]. Has no effect on user providers
	/// (they bring their own `base_url`).
	pub fn with_hf_base_url(mut self, base_url: impl Into<String>) -> Self {
		self.hf_base_url = base_url.into();
		self
	}

	/// Resolve the request route fresh off the shared models +
	/// keyring. Called at the top of every request so a settings
	/// flip mid-turn applies to the next call without rewiring.
	///
	/// HF path: fetch (or refresh) the OAuth access token; HF
	/// returns `NotSignedIn` cleanly when the user hasn't done
	/// the device flow.
	///
	/// Custom path: snapshot the keyring entry (None = no auth
	/// header, the local-llama.cpp case). Returns an error only
	/// when the resolved entry is missing a `base_url` — a
	/// defensive check; the picker rejects empty URLs at save
	/// time, this guards against a hand-edited `state.json`.
	async fn resolve_route_for_request(&self) -> Result<ResolvedRoute, CoderError> {
		let snapshot = self.models.read().await.clone();
		let resolved = snapshot.resolve_route();
		if matches!(resolved, ResolvedProvider::HuggingFace) {
			let access = self.auth.current_access_token().await?;
			return Ok(ResolvedRoute {
				base_url: self.hf_base_url.clone(),
				auth_token: Some(access),
				bill_to: snapshot.bill_to().map(str::to_owned),
				kind: RouteKind::HuggingFace,
			});
		}
		let (kind, id, base_url) = match resolved {
			ResolvedProvider::HuggingFace => unreachable!(),
			ResolvedProvider::Custom { id, base_url } => (RouteKind::Custom, id, base_url),
			ResolvedProvider::OpenRouter { id, base_url } => (RouteKind::OpenRouter, id, base_url),
			ResolvedProvider::Anthropic { id, base_url } => (RouteKind::Anthropic, id, base_url),
		};
		if base_url.trim().is_empty() {
			return Err(CoderError::Internal(format!(
				"active provider {id} has empty base_url; fix it in the model-settings popover"
			)));
		}
		let trimmed = base_url.trim_end_matches('/').to_owned();
		let auth_token = self.provider_keys.get(&id);
		Ok(ResolvedRoute {
			base_url: trimmed,
			auth_token,
			bill_to: None,
			kind,
		})
	}

	/// Cancel-aware wrapper around [`resolve_route_for_request`].
	///
	/// The inner method can hit the network (HF OAuth token refresh)
	/// and historically ran *before* any cancel-aware `select!`,
	/// which meant a stalled OAuth endpoint parked the turn in an
	/// uncancellable `.await`: the cancel token tripped but nothing
	/// polled it, so Esc did nothing and `busy` never cleared. Racing
	/// it against `cancel` here ensures an abort lands as soon as the
	/// token fires, regardless of which phase the turn is in.
	async fn resolve_route_or_abort(
		&self,
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<ResolvedRoute, CoderError> {
		tokio::select! {
			biased;
			_ = cancel.cancelled() => Err(CoderError::Aborted),
			route = self.resolve_route_for_request() => route,
		}
	}

	/// Cancel-aware wrapper around [`Authenticator::refresh_now`],
	/// used on the 401-retry path. Same rationale as
	/// [`resolve_route_or_abort`]: the refresh round trip is a bare
	/// `.await` that Esc must be able to interrupt.
	async fn refresh_or_abort(&self, cancel: &tokio_util::sync::CancellationToken) -> Result<String, CoderError> {
		tokio::select! {
			biased;
			_ = cancel.cancelled() => Err(CoderError::Aborted),
			token = self.auth.refresh_now() => token,
		}
	}

	/// GET `/v1/models` against the HF router → rich
	/// [`RouterModel`] catalog with per-route pricing / throughput.
	/// Only valid when the active provider is HF; the runner gates
	/// the call before forwarding from the Tauri command.
	///
	/// Auth uses the user's OAuth token; the router gates model
	/// visibility on the token's scopes + the user's plan, so the
	/// list a free user sees is a subset of what a Pro user sees.
	/// We don't try to second-guess that — we just forward.
	///
	/// Bill-to header is **not** sent for the catalog call. The
	/// router returns the same catalog regardless and we want a
	/// failed `coder_set_models { bill_to = "org" }` to still let
	/// the picker reload after the user fixes the org name.
	pub async fn list_hf_models(&self) -> Result<Vec<moon_protocol::coder_models::RouterModel>, CoderError> {
		use moon_protocol::coder_models::{RouterModel, RouterPricing, RouterProvider};
		let token = self.auth.current_access_token().await?;
		let endpoint = format!("{}/models", self.hf_base_url);
		let response = self
			.http
			.get(&endpoint)
			.timeout(REQUEST_TIMEOUT)
			.bearer_auth(&token)
			.send()
			.await
			.map_err(CoderError::from)?;
		let status = response.status();
		let request_id = request_id_of(&response);
		let body = response.text().await.map_err(CoderError::from)?;
		if !status.is_success() {
			return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
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

	/// `GET <base_url>/models` against a user-added provider.
	///
	/// All OpenAI-compat servers ship `{ data: [{ id, owned_by? }] }`
	/// at minimum. Many ship more: OpenRouter adds
	/// `name` / `context_length` / `pricing` / `description`;
	/// LiteLLM exposes pricing for routes it has rates for; vLLM
	/// emits `max_model_len` (treated as context length). We
	/// parse all of those tolerantly — fields the server
	/// doesn't emit deserialize as `None` and the picker just
	/// doesn't render them.
	///
	/// Pricing comes back from OpenRouter as strings of
	/// dollars-per-token (`"0.000003"`); we multiply by
	/// `1_000_000` so the protocol type carries a uniform
	/// "$/M tokens" number regardless of source. LiteLLM emits
	/// per-million numbers directly under
	/// `input_cost_per_million_tokens` and we forward those
	/// untouched.
	///
	/// Errors propagate verbatim — a 404 from a minimal server
	/// that skips `/v1/models` lands back at the picker; the user
	/// can still type a model slug directly into the field.
	pub async fn list_provider_models(
		&self,
		base_url: &str,
		api_key: Option<&str>,
		kind: moon_protocol::coder_models::ProviderKind,
	) -> Result<Vec<moon_protocol::coder_models::ProviderModelSummary>, CoderError> {
		use moon_protocol::coder_models::ProviderKind;
		if matches!(kind, ProviderKind::Anthropic) {
			return crate::anthropic::list_models(&self.http, base_url, api_key).await;
		}
		let trimmed = base_url.trim_end_matches('/');
		let endpoint = format!("{trimmed}/models");
		let mut req = self.http.get(&endpoint);
		if let Some(key) = api_key {
			req = req.bearer_auth(key);
		}
		let response = req.send().await.map_err(CoderError::from)?;
		let status = response.status();
		let request_id = request_id_of(&response);
		let body = response.text().await.map_err(CoderError::from)?;
		if !status.is_success() {
			return Err(CoderError::http(endpoint, status.as_u16(), body, request_id));
		}

		let raw: provider_catalog::ListBody = crate::auth::decode_body(&endpoint, &body)?;
		Ok(raw.data.into_iter().map(provider_catalog::flatten).collect())
	}

	/// One non-streaming chat-completions round trip.
	///
	/// HF: auto-refresh on 401 — the first response that comes
	/// back as `Unauthorized` triggers a refresh-then-retry; the
	/// second 401 surfaces as `NotSignedIn` to force the panel
	/// back into the device-flow modal.
	///
	/// Custom providers: 401 is surfaced verbatim. There's no
	/// refresh token; the user has to fix the key in the picker.
	pub async fn chat_completion(
		&self,
		model: &str,
		messages: &[ChatMessage],
		tools: &[ToolDefinition],
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<AssistantResponse, CoderError> {
		let route = self.resolve_route_or_abort(cancel).await?;
		if route.kind == RouteKind::Anthropic {
			return crate::anthropic::chat_completion(&self.http, &route, model, messages, tools, cancel).await;
		}
		let mut route = route;
		let endpoint = format!("{}/chat/completions", route.base_url);
		let cache_indexes = cache_breakpoint_indexes(messages, &route, model);
		let body = ChatCompletionRequest {
			model,
			messages: build_wire_messages(messages, &cache_indexes),
			tools,
			tool_choice: if tools.is_empty() { None } else { Some("auto") },
			stream: false,
			stream_options: None,
		};

		let mut response = self.send_once(&endpoint, &route, &body, cancel).await?;

		if response.status() == reqwest::StatusCode::UNAUTHORIZED && route.is_huggingface() {
			tracing::info!("HF inference returned 401; refreshing token and retrying once");
			let refreshed = self.refresh_or_abort(cancel).await?;
			route.auth_token = Some(refreshed);
			response = self.send_once(&endpoint, &route, &body, cancel).await?;
		}

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

		let parsed: ChatCompletionResponse = crate::auth::decode_body(&endpoint, &text)?;
		let usage = parsed.usage;
		parsed
			.choices
			.into_iter()
			.next()
			.map(|c| {
				let finish_reason = c.finish_reason;
				let mut msg = c.message;
				msg.usage = usage;
				msg.stop_reason = Some(normalize_stop_reason(
					finish_reason.as_deref(),
					!msg.tool_calls.is_empty(),
				));
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
		let route = self.resolve_route_or_abort(cancel).await?;
		if route.kind == RouteKind::Anthropic {
			return crate::anthropic::chat_completion_stream(&self.http, &route, model, messages, tools, cancel, on_event)
				.await;
		}
		let mut route = route;
		let endpoint = format!("{}/chat/completions", route.base_url);
		let cache_indexes = cache_breakpoint_indexes(messages, &route, model);
		let body = ChatCompletionRequest {
			model,
			messages: build_wire_messages(messages, &cache_indexes),
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

		let mut response = self.send_once_stream(&endpoint, &route, &body, cancel).await?;

		if response.status() == reqwest::StatusCode::UNAUTHORIZED && route.is_huggingface() {
			tracing::info!("HF inference returned 401; refreshing token and retrying once");
			let refreshed = self.refresh_or_abort(cancel).await?;
			route.auth_token = Some(refreshed);
			response = self.send_once_stream(&endpoint, &route, &body, cancel).await?;
		}

		let status = response.status();
		if !status.is_success() {
			let request_id = request_id_of(&response);
			// Drain the body for the error message; failures aren't
			// SSE-shaped, they're a plain JSON error body.
			let recv = response.text();
			let text = tokio::select! {
				biased;
				_ = cancel.cancelled() => return Err(CoderError::Aborted),
				out = recv => out.map_err(CoderError::from)?,
			};
			return Err(CoderError::http(endpoint, status.as_u16(), text, request_id));
		}

		consume_sse_stream(response, cancel, |chunk| {
			apply_chunk(chunk, &mut on_event);
		})
		.await
	}

	async fn send_once(
		&self,
		endpoint: &str,
		route: &ResolvedRoute,
		body: &ChatCompletionRequest<'_>,
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, CoderError> {
		// Non-streaming: cap the whole round trip so a provider that
		// accepts the request then stalls can't park the turn. Streaming
		// sends skip this (see `send_once_stream`) — long generations are
		// legitimate and bounded by the cancel token instead.
		let mut builder = self.http.post(endpoint).json(body).timeout(REQUEST_TIMEOUT);
		if let Some(token) = route.auth_token.as_deref() {
			builder = builder.bearer_auth(token);
		}
		if let Some(org) = route.bill_to.as_deref() {
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
		route: &ResolvedRoute,
		body: &ChatCompletionRequest<'_>,
		cancel: &tokio_util::sync::CancellationToken,
	) -> Result<reqwest::Response, CoderError> {
		// Same shape as `send_once`; a separate method exists only to
		// mirror it — no header difference today *except* the
		// explicit `accept: text/event-stream` to nudge providers
		// that default to JSON.
		let mut builder = self
			.http
			.post(endpoint)
			.header("accept", "text/event-stream")
			.json(body);
		if let Some(token) = route.auth_token.as_deref() {
			builder = builder.bearer_auth(token);
		}
		if let Some(org) = route.bill_to.as_deref() {
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
	let mut finish_reason: Option<String> = None;
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
					return Ok(finalize_response(
						content_buf,
						thinking_buf,
						tool_call_bufs,
						usage,
						finish_reason,
					));
				}
				let chunk: StreamChunk = serde_json::from_str(data).map_err(|err| {
					CoderError::decode(
						"inference stream",
						format!("could not parse SSE chunk: {err}; raw={}", truncate_for_log(data)),
					)
				})?;
				accumulate_chunk(&chunk, &mut content_buf, &mut thinking_buf, &mut tool_call_bufs);
				if let Some(reason) = chunk.choices.first().and_then(|c| c.finish_reason.clone()) {
					finish_reason = Some(reason);
				}
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
	Ok(finalize_response(
		content_buf,
		thinking_buf,
		tool_call_bufs,
		usage,
		finish_reason,
	))
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
	finish_reason: Option<String>,
) -> AssistantResponse {
	let tool_calls: Vec<ToolCall> = tool_calls
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
		.collect();
	let stop_reason = Some(normalize_stop_reason(finish_reason.as_deref(), !tool_calls.is_empty()));
	AssistantResponse {
		content: if content.is_empty() { None } else { Some(content) },
		thinking: if thinking.is_empty() { None } else { Some(thinking) },
		// OpenAI-compat providers don't emit signed reasoning blocks.
		thinking_blocks: Vec::new(),
		tool_calls,
		usage,
		stop_reason,
	}
}

#[derive(Debug)]
pub(crate) struct EventBoundary {
	/// Offset (exclusive) of the last byte of the event body — i.e.
	/// the position of the trailing `\n` that immediately precedes
	/// the blank-line separator.
	pub(crate) body_end: usize,
	/// Offset (exclusive) of the byte *after* the blank-line
	/// separator. Drain `0..boundary_end` to consume the event.
	pub(crate) boundary_end: usize,
}

/// Find the next `\n\n` (or `\r\n\r\n`) boundary in the buffer.
/// Returns `None` when the buffer doesn't yet contain a complete
/// event — the caller pulls more bytes and tries again.
pub(crate) fn find_event_boundary(buf: &[u8]) -> Option<EventBoundary> {
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
pub(crate) fn extract_data_lines(event: &str) -> Vec<&str> {
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

pub(crate) fn truncate_for_log(s: &str) -> String {
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

/// Tolerant parser for OpenAI-compat `/v1/models` responses.
///
/// Three classes of server we want to read:
///
/// - **Minimal** (Ollama, llama.cpp): just `{ id, owned_by? }`.
/// - **OpenRouter**: adds `name`, `context_length`, pricing as
///   strings of `$/token`, and a `description`. Pricing lives
///   under `pricing.prompt` / `pricing.completion` for the
///   prompt-/completion-side respectively.
/// - **LiteLLM** when run as a router: pricing under
///   `input_cost_per_token` / `output_cost_per_token` (numbers,
///   per token), context length sometimes nested under
///   `litellm_provider` config blocks. We pick what we can; the
///   rest stays `None`.
/// - **vLLM**: exposes `max_model_len` instead of
///   `context_length`. We accept either as the source for the
///   context window.
///
/// Anything we can't parse stays `None`; the picker degrades to
/// the minimal view. Pricing is normalised to **$/million
/// tokens** at this boundary so the protocol type and UI never
/// have to second-guess units.
mod provider_catalog {
	use serde::Deserialize;

	use moon_protocol::coder_models::ProviderModelSummary;

	#[derive(Deserialize)]
	pub(super) struct ListBody {
		#[serde(default)]
		pub(super) data: Vec<RawModel>,
	}

	#[derive(Deserialize)]
	pub(super) struct RawModel {
		id: String,
		#[serde(default)]
		owned_by: Option<String>,
		/// OpenRouter ships this as the long human name; OpenAI's
		/// own catalog doesn't include it.
		#[serde(default)]
		name: Option<String>,
		#[serde(default)]
		description: Option<String>,
		/// OpenRouter: integer context window. We also accept
		/// `max_model_len` (vLLM) as a synonym; both populate
		/// this field at deserialize time via the catch-all
		/// inside `flatten`.
		#[serde(default)]
		context_length: Option<u32>,
		#[serde(default)]
		max_model_len: Option<u32>,
		/// OpenRouter shape: `{ prompt: "0.000003", completion: "0.000015" }`,
		/// strings of dollars per token. Other fields
		/// (`image`, `request`, …) ignored on purpose.
		#[serde(default)]
		pricing: Option<OpenRouterPricing>,
		/// LiteLLM shape: per-token floats at the top level.
		#[serde(default)]
		input_cost_per_token: Option<f64>,
		#[serde(default)]
		output_cost_per_token: Option<f64>,
		/// Some LiteLLM deployments pre-multiply for the user.
		/// Trust those verbatim.
		#[serde(default)]
		input_cost_per_million_tokens: Option<f64>,
		#[serde(default)]
		output_cost_per_million_tokens: Option<f64>,
	}

	#[derive(Deserialize)]
	struct OpenRouterPricing {
		/// Prompt-side price. OpenRouter sends a string; we
		/// also accept numbers in case the server normalises.
		#[serde(default)]
		prompt: Option<StringOrFloat>,
		#[serde(default)]
		completion: Option<StringOrFloat>,
	}

	#[derive(Deserialize)]
	#[serde(untagged)]
	enum StringOrFloat {
		Float(f64),
		String(String),
	}

	impl StringOrFloat {
		fn as_f64(&self) -> Option<f64> {
			match self {
				Self::Float(v) => Some(*v),
				Self::String(s) => s.trim().parse::<f64>().ok(),
			}
		}
	}

	/// Truncate a description to a sane cap so a server that
	/// returns a full README per model doesn't blow up the picker
	/// UI. Chosen by eyeball — most OpenRouter descriptions are
	/// ≤ 200 chars, the few that aren't get clipped without
	/// fanfare.
	const DESCRIPTION_CAP: usize = 240;

	pub(super) fn flatten(raw: RawModel) -> ProviderModelSummary {
		let context_length = raw.context_length.or(raw.max_model_len);
		let pricing_in_per_million = raw
			.input_cost_per_million_tokens
			.or_else(|| raw.input_cost_per_token.map(|v| v * 1_000_000.0))
			.or_else(|| {
				raw
					.pricing
					.as_ref()
					.and_then(|p| p.prompt.as_ref())
					.and_then(|v| v.as_f64())
					.map(|v| v * 1_000_000.0)
			});
		let pricing_out_per_million = raw
			.output_cost_per_million_tokens
			.or_else(|| raw.output_cost_per_token.map(|v| v * 1_000_000.0))
			.or_else(|| {
				raw
					.pricing
					.as_ref()
					.and_then(|p| p.completion.as_ref())
					.and_then(|v| v.as_f64())
					.map(|v| v * 1_000_000.0)
			});
		let description = raw.description.map(|s| {
			let trimmed = s.trim();
			if trimmed.chars().count() <= DESCRIPTION_CAP {
				trimmed.to_owned()
			} else {
				// Clip on a char boundary; never panic.
				let mut out = String::with_capacity(DESCRIPTION_CAP + 1);
				for (i, c) in trimmed.chars().enumerate() {
					if i >= DESCRIPTION_CAP {
						break;
					}
					out.push(c);
				}
				out.push('…');
				out
			}
		});
		ProviderModelSummary {
			id: raw.id,
			owned_by: raw.owned_by,
			name: raw.name.filter(|s| !s.is_empty()),
			context_length,
			pricing_in_per_million,
			pricing_out_per_million,
			description: description.filter(|s| !s.is_empty()),
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn parses_openrouter_shape() {
			let raw = r#"{
				"data": [{
					"id": "anthropic/claude-3.5-sonnet",
					"name": "Anthropic: Claude 3.5 Sonnet",
					"context_length": 200000,
					"pricing": {"prompt": "0.000003", "completion": "0.000015"},
					"description": "Anthropic's flagship..."
				}]
			}"#;
			let parsed: ListBody = serde_json::from_str(raw).unwrap();
			let row = flatten(parsed.data.into_iter().next().unwrap());
			assert_eq!(row.id, "anthropic/claude-3.5-sonnet");
			assert_eq!(row.name.as_deref(), Some("Anthropic: Claude 3.5 Sonnet"));
			assert_eq!(row.context_length, Some(200_000));
			assert!((row.pricing_in_per_million.unwrap() - 3.0).abs() < 1e-9);
			assert!((row.pricing_out_per_million.unwrap() - 15.0).abs() < 1e-9);
		}

		#[test]
		fn parses_minimal_ollama_shape() {
			let raw = r#"{
				"data": [{"id": "llama3.2", "owned_by": "library"}]
			}"#;
			let parsed: ListBody = serde_json::from_str(raw).unwrap();
			let row = flatten(parsed.data.into_iter().next().unwrap());
			assert_eq!(row.id, "llama3.2");
			assert_eq!(row.owned_by.as_deref(), Some("library"));
			assert!(row.pricing_in_per_million.is_none());
			assert!(row.context_length.is_none());
		}

		#[test]
		fn vllm_max_model_len_maps_to_context_length() {
			let raw = r#"{
				"data": [{"id": "qwen2.5-72b", "max_model_len": 32768}]
			}"#;
			let parsed: ListBody = serde_json::from_str(raw).unwrap();
			let row = flatten(parsed.data.into_iter().next().unwrap());
			assert_eq!(row.context_length, Some(32_768));
		}

		#[test]
		fn litellm_per_token_pricing_normalises_to_per_million() {
			let raw = r#"{
				"data": [{
					"id": "gpt-4o-mini",
					"input_cost_per_token": 0.00000015,
					"output_cost_per_token": 0.0000006
				}]
			}"#;
			let parsed: ListBody = serde_json::from_str(raw).unwrap();
			let row = flatten(parsed.data.into_iter().next().unwrap());
			assert!((row.pricing_in_per_million.unwrap() - 0.15).abs() < 1e-9);
			assert!((row.pricing_out_per_million.unwrap() - 0.60).abs() < 1e-9);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extract_data_skips_comments_and_keepalives() {
		let event = ": ping\ndata: hello\n";
		assert_eq!(extract_data_lines(event), vec!["hello"]);
	}

	#[test]
	fn normalize_stop_reason_maps_provider_vocabularies() {
		// OpenAI chat-completions values.
		assert_eq!(normalize_stop_reason(Some("stop"), false), "stop");
		assert_eq!(normalize_stop_reason(Some("tool_calls"), false), "toolUse");
		assert_eq!(normalize_stop_reason(Some("length"), false), "length");
		assert_eq!(normalize_stop_reason(Some("content_filter"), false), "error");
		// Anthropic Messages values.
		assert_eq!(normalize_stop_reason(Some("end_turn"), false), "stop");
		assert_eq!(normalize_stop_reason(Some("tool_use"), true), "toolUse");
		assert_eq!(normalize_stop_reason(Some("max_tokens"), false), "length");
		assert_eq!(normalize_stop_reason(Some("refusal"), false), "error");
		// Missing / unrecognised falls back to the structural signal.
		assert_eq!(normalize_stop_reason(None, true), "toolUse");
		assert_eq!(normalize_stop_reason(None, false), "stop");
		assert_eq!(normalize_stop_reason(Some("weird_new_reason"), false), "stop");
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
		let resp = finalize_response(String::new(), String::new(), buf, None, None);
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
		let resp = finalize_response(content, thinking, tcs, None, None);
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
		let resp = finalize_response(content, thinking, tcs, None, None);
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
		let resp = finalize_response(content, thinking, tcs, None, None);
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
	fn cache_breakpoints_empty_for_non_openrouter_route() {
		// HF route → no caching, regardless of model id.
		let route = ResolvedRoute {
			base_url: "https://router.huggingface.co/v1".into(),
			auth_token: None,
			bill_to: None,
			kind: RouteKind::HuggingFace,
		};
		let messages = vec![ChatMessage::System { content: "sys".into() }, ChatMessage::user("hi")];
		assert!(cache_breakpoint_indexes(&messages, &route, "anthropic/claude-sonnet-4.5").is_empty());

		// Custom (non-OpenRouter, non-Anthropic) route → no caching
		// either, even if the slug happens to start with
		// `anthropic/`. The Anthropic-native variant takes a
		// completely separate code path; it doesn't run through
		// `cache_breakpoint_indexes` at all.
		let route = ResolvedRoute {
			base_url: "https://example.com/v1".into(),
			auth_token: None,
			bill_to: None,
			kind: RouteKind::Custom,
		};
		assert!(cache_breakpoint_indexes(&messages, &route, "anthropic/claude-sonnet-4.5").is_empty());

		// OpenRouter with a non-Anthropic slug → no caching.
		let route = ResolvedRoute {
			base_url: "https://openrouter.ai/api/v1".into(),
			auth_token: None,
			bill_to: None,
			kind: RouteKind::OpenRouter,
		};
		assert!(cache_breakpoint_indexes(&messages, &route, "openai/gpt-4o").is_empty());
	}

	#[test]
	fn cache_breakpoints_marks_system_and_last_non_assistant() {
		let route = ResolvedRoute {
			base_url: "https://openrouter.ai/api/v1".into(),
			auth_token: None,
			bill_to: None,
			kind: RouteKind::OpenRouter,
		};
		let model = "anthropic/claude-sonnet-4.5";

		// Just a system prompt: only index 0 marked.
		let messages = vec![ChatMessage::System { content: "sys".into() }];
		assert_eq!(cache_breakpoint_indexes(&messages, &route, model), vec![0]);

		// System + user: both marked.
		let messages = vec![ChatMessage::System { content: "sys".into() }, ChatMessage::user("hi")];
		assert_eq!(cache_breakpoint_indexes(&messages, &route, model), vec![0, 1]);

		// System + user + assistant (with tool calls, empty content):
		// last is assistant → walk back to user at index 1.
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("hi"),
			ChatMessage::Assistant {
				content: None,
				thinking_blocks: Vec::new(),
				tool_calls: vec![ToolCall {
					id: "call_1".into(),
					kind: "function".into(),
					function: FunctionCall {
						name: "ls".into(),
						arguments: "{}".into(),
					},
				}],
			},
		];
		assert_eq!(cache_breakpoint_indexes(&messages, &route, model), vec![0, 1]);

		// System + user + assistant + tool: anchor on the tool
		// (index 3), the most up-to-date stable prefix for next call.
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("hi"),
			ChatMessage::Assistant {
				content: None,
				thinking_blocks: Vec::new(),
				tool_calls: vec![ToolCall {
					id: "call_1".into(),
					kind: "function".into(),
					function: FunctionCall {
						name: "ls".into(),
						arguments: "{}".into(),
					},
				}],
			},
			ChatMessage::Tool {
				tool_call_id: "call_1".into(),
				content: "/etc /var".into(),
			},
		];
		assert_eq!(cache_breakpoint_indexes(&messages, &route, model), vec![0, 3]);
	}

	#[test]
	fn wire_messages_no_cache_serialises_as_string_content() {
		// The cheap-path is the load-bearing case: every non-Anthropic
		// request has to come out byte-for-byte the same as the
		// pre-caching wire shape, otherwise we'd break every provider
		// that doesn't expect blocks-form content.
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("hi"),
			ChatMessage::Tool {
				tool_call_id: "call_1".into(),
				content: "ok".into(),
			},
		];
		let wire = build_wire_messages(&messages, &[]);
		let json = serde_json::to_string(&wire).unwrap();
		assert!(json.contains(r#"{"role":"system","content":"sys"}"#));
		assert!(json.contains(r#"{"role":"user","content":"hi"}"#));
		assert!(json.contains(r#"{"role":"tool","tool_call_id":"call_1","content":"ok"}"#));
		// Crucial regression guard: no `cache_control` and no
		// blocks-array shape leaked when caching is off.
		assert!(!json.contains("cache_control"));
		assert!(!json.contains(r#""type":"text""#));
	}

	#[test]
	fn wire_messages_with_cache_emits_blocks_only_on_marked_indexes() {
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::user("hi"),
			ChatMessage::Tool {
				tool_call_id: "call_1".into(),
				content: "ok".into(),
			},
		];
		// Mark system + tool (typical 2-breakpoint placement for
		// an in-flight turn). User in between stays string-form.
		let wire = build_wire_messages(&messages, &[0, 2]);
		let json = serde_json::to_string(&wire).unwrap();
		assert!(json
			.contains(r#"{"role":"system","content":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}]}"#));
		// User remains string content (not in `cached_indexes`).
		assert!(json.contains(r#"{"role":"user","content":"hi"}"#));
		// Tool gets the blocks shape with cache_control on the
		// single text block; tool_call_id is preserved.
		assert!(json.contains(
			r#"{"role":"tool","tool_call_id":"call_1","content":[{"type":"text","text":"ok","cache_control":{"type":"ephemeral"}}]}"#
		));
	}

	#[test]
	fn wire_user_with_images_emits_blocks_with_image_url() {
		let messages = vec![
			ChatMessage::System { content: "sys".into() },
			ChatMessage::User {
				content: "what's in this".into(),
				images: vec![ImageAttachment {
					data_url: "data:image/png;base64,AAAA".into(),
					mime: "image/png".into(),
				}],
			},
		];
		let wire = build_wire_messages(&messages, &[]);
		let json = serde_json::to_string(&wire).unwrap();
		assert!(json.contains(r#"{"role":"system","content":"sys"}"#));
		assert!(json.contains(
			r#""role":"user","content":[{"type":"text","text":"what's in this"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]"#
		));
	}

	#[test]
	fn wire_user_with_images_and_cache_marks_text_block_only() {
		let messages = vec![ChatMessage::User {
			content: "hi".into(),
			images: vec![ImageAttachment {
				data_url: "data:image/jpeg;base64,BBBB".into(),
				mime: "image/jpeg".into(),
			}],
		}];
		let wire = build_wire_messages(&messages, &[0]);
		let json = serde_json::to_string(&wire).unwrap();
		assert!(json.contains(r#""type":"text","text":"hi","cache_control":{"type":"ephemeral"}"#));
		assert!(json.contains(r#""type":"image_url","image_url":{"url":"data:image/jpeg;base64,BBBB"}"#));
	}

	#[test]
	fn wire_user_no_images_no_cache_keeps_string_content() {
		// Regression guard: adding the images field must not
		// change the cheap path's wire shape — every router with
		// a prefix cache keyed on the literal request bytes
		// would otherwise miss on the next turn.
		let messages = vec![ChatMessage::User {
			content: "hi".into(),
			images: Vec::new(),
		}];
		let wire = build_wire_messages(&messages, &[]);
		let json = serde_json::to_string(&wire).unwrap();
		assert_eq!(json, r#"[{"role":"user","content":"hi"}]"#);
	}

	#[test]
	fn token_usage_accepts_anthropic_cache_fields() {
		// OpenRouter streams these alongside the usual prompt /
		// completion / total when the request had cache_control
		// markers. We have to parse them or the breakdown goes
		// silently to zero in the tooltip.
		let raw = r#"{"prompt_tokens":4000,"completion_tokens":200,"total_tokens":4200,"cache_read_input_tokens":3500,"cache_creation_input_tokens":480}"#;
		let usage: TokenUsage = serde_json::from_str(raw).unwrap();
		assert_eq!(usage.prompt_tokens, 4000);
		assert_eq!(usage.cache_read_input_tokens, 3500);
		assert_eq!(usage.cache_creation_input_tokens, 480);

		// Providers that don't emit the fields parse as zero;
		// the runner emits 0 to the panel, which the tooltip
		// then suppresses entirely.
		let raw = r#"{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}"#;
		let usage: TokenUsage = serde_json::from_str(raw).unwrap();
		assert_eq!(usage.cache_read_input_tokens, 0);
		assert_eq!(usage.cache_creation_input_tokens, 0);
	}

	#[test]
	fn find_event_boundary_handles_lf_and_crlf() {
		assert!(find_event_boundary(b"data: x\n\nrest").is_some());
		assert!(find_event_boundary(b"data: x\r\n\r\nrest").is_some());
		assert!(find_event_boundary(b"data: x\n").is_none());
	}
}
