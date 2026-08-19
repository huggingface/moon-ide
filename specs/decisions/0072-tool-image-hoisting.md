# ADR 0072 — Tool-result images: hoist into a user message on the OpenAI-compat wire

## Context

Tool results with images (playwright screenshots, `read_file` on a
PNG) went on the OpenAI-compat wire as a content-parts array inside
the `role: "tool"` message (`[{text}, {image_url}, …]`). OpenAI's
schema types `tool` content as string-only; most routers tolerate the
array, but strict backends enforce the schema — switching a session
with screenshots in history to deepinfra (behind the HF router)
rejected every request with a 422
(`messages.N…ChatCompletionToolMessage.content.str — Input should be
a valid string`), wedging the session on that provider until the
images aged out.

## Decision

On the OpenAI-compat wire (`build_wire_messages`), `tool` message
content is always a string (or a single cache-marked text block on
the OpenRouter-Anthropic caching path). Images collected from a
contiguous run of tool messages are hoisted into **one synthetic
user message emitted after the run** — a
`[image(s) attached from the preceding tool result(s)]` text block
followed by the usual `image_url` parts (or per-image
`[image omitted: …]` notes for non-vision models, ADR 0069). After
the whole run, not per message, because validators require tool
messages to directly follow the tool-calling assistant turn.

Unconditional, not per-provider: one wire shape that every backend
accepts beats sniffing which providers tolerate the array. The
Anthropic-native path is untouched — its `tool_result` blocks carry
images first-class. Session history is untouched too; this is
request-encode-time only.

## Rejected alternatives

- **Per-provider gating (keep the array where it worked).** The HF
  router multiplexes providers behind one endpoint and can't tell us
  which validate strictly; a capability map would be guesswork that
  rots. The hoisted shape costs nothing on tolerant providers.
- **Retry-on-422 with images hoisted.** Error-sniffing plus a
  duplicate request path for a deterministic, known-in-advance schema
  constraint.
- **Dropping tool images on strict providers.** Silent quality loss;
  the model asked for the screenshot for a reason.
