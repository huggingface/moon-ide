# Coder — the in-process AI coding agent

The right-side coder panel is moon-ide's own coding agent. Unlike
the [Slack chat panel](slack-chat.md) (a thin chat client over
somebody else's bot), the coder owns its loop end-to-end: it talks
to LLMs directly, dispatches its own tool calls, and routes every
tool call through the active [`WorkspaceHost`](architecture.md#workspacehost-phase-2)
so a containerized workspace gets a containerized agent without
extra plumbing.

[ADR 0010](decisions/0010-coder-rewrite-not-acp.md) explains why
we don't host ACP. [ADR 0011](decisions/0011-rename-moon-agent-to-moon-remote.md)
explains why the new crate is `moon-coder` and the existing
remote-host stub becomes `moon-remote`. Sub-phase work breakdown:
[`roadmaps/phase-06-coder.md`](roadmaps/phase-06-coder.md).

## Why we own the loop

The Phase 6 reasons in ADR 0010, in one paragraph: ACP is a
protocol for driving somebody else's agent binary. The agents the
team would actually pick (Claude Code, Cursor, opencode) aren't ACP
native, and the ones that are (pi-coding-agent) are TS-first.
Either we adopt a Node sidecar and inherit pi's release cadence, or
we put fs/bash tools in JS land and break the architecture
invariant. Owning the loop in Rust is a few hundred lines around
`reqwest` + an SSE parser + a tool dispatcher, and it lets every
tool be a moon-core method that already respects `WorkspaceHost`.

## Loop shape

The conceptual vocabulary borrows from
[pi-agent-core](https://github.com/badlogic/pi-mono/tree/main/packages/agent)
but the wire shape we ship is flatter — pi's `message_start` /
`message_end` framing was redundant once we picked stable IDs and
delta accumulation:

```
prompt(msg)
├─ user_message               { id, text }
├─ assistant_message_start    { id }                        // fires on first content OR thinking delta
├─ assistant_thinking_delta   { id, delta }*                // optional, before/interleaved with content
├─ assistant_message_delta    { id, delta }*                // SSE chunks of the answer
├─ assistant_message_end      { id, text, thinking? }       // canonical full content + reasoning
├─ tool_call                  { id, name, args }*           // one per call (assembled)
├─ tool_result                { id, result, is_error }*     // when each tool finishes
│
├─ assistant_message_start    { id' }                       // next iteration if tools fired
├─ …
└─ turn_complete | aborted | error                          // exactly one of the three
```

Mirrored 1:1 in `src/lib/protocol.ts:CoderEvent`. Stable IDs let
the frontend reconcile a stream of deltas onto one bubble without
buffering the order; `assistant_message_end.text` /
`assistant_message_end.thinking` are authoritative in case the
concatenated deltas drift (e.g. a mid-stream provider retry).

Reasoning traces stream as `assistant_thinking_delta`. We accept
both `reasoning_content` (DeepSeek, Qwen) and `reasoning` (other
providers) on the wire; the frontend renders the accumulated
trace in a collapsible block above the answer that auto-collapses
when `assistant_message_end` fires. Models that don't expose a
reasoning trace simply never emit thinking deltas — no special
case in the runner or UI.

Tool calls **stream incrementally** off the SSE wire (chunks carry
partial JSON for `function.arguments`), but the loop buffers them
in `inference.rs` and only fires `tool_call` once the call is fully
assembled — partial JSON is not useful to render. Tool calls in a
single assistant turn dispatch sequentially today; parallel
dispatch lands when a real workload needs it (no measured benefit
yet on the team's prompt patterns).

States the panel needs to render:

| State       | Where it shows                                                                |
| ----------- | ----------------------------------------------------------------------------- |
| `idle`      | composer is the active focus, send button enabled                             |
| `streaming` | message_update events landing on the current assistant message; abort enabled |
| `tools`     | one or more `tool_execution_*` blocks expanding inline; abort enabled         |
| `error`     | error banner above composer, "retry" / "continue" affordances                 |

Three control surfaces:

- **Abort** (`Esc`): cancels the in-flight HTTP / SSE / tool-call.
  The session keeps the partial assistant message and any completed
  tool calls; the loop stops cleanly.
- **Steer** (Enter while `streaming` / `tools`): the composer stays
  editable mid-turn. Pressing Enter fires `coder_send` like a
  regular message; the runner sees a turn already in flight,
  appends the text to `Session.pending_steers`, and emits a
  `user_message` event so the bubble lands in the transcript
  immediately. The running `run_turn` drains the queue at its next
  iteration top — after the current iteration's tool results have
  settled, before the next LLM call — so the model sees the steer
  on its next round-trip. Persistence happens at drain time, never
  at queue time, because the OpenAI / Anthropic chat shape forbids
  a user message between an `assistant.tool_calls` and its tool
  result rows; persisting then would corrupt session reload.
  Aborts (`Esc`) drop undrained steers — pressing Esc throws away
  in-flight intent, including queued context.
- **Follow-up** (Alt+Enter while idle-but-just-finished): future —
  not implemented. For now, sending while idle just starts a fresh
  turn the moment the user hits Enter.

These match pi's defaults. They also match the Slack panel's
keymap (Enter sends, Shift+Enter newline) so a user moving between
the two doesn't relearn anything.

## Authentication

### HF OAuth Device Authorization Grant (RFC 8628)

The IDE registers a public OAuth app on Hugging Face Hub with
client ID **`7977dff4-917a-4cf9-a726-dd45e25faa5f`**. No client
secret. Scopes asked upfront, just like
[Phase 11's Slack flow](slack-chat.md#required-scopes-user-token):

| Scope              | Used by     | What it gets us                                                                                                                                                                                                                                                |
| ------------------ | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `inference-api`    | LLM calls   | Call HF Inference Providers' router endpoint                                                                                                                                                                                                                   |
| `contribute-repos` | Bucket sync | Create + write to the `<user>/moon-ide-sessions` private bucket. Strictly weaker than `manage-repos` (which would also grant repo deletion and settings edits); `contribute-repos` is the minimum that lets us `create-repo` + `push`, which is all sync needs |

The `read-billing` and `email` scopes are deliberately **not**
asked for: we don't need to read the user's email and we don't
surface billing in the IDE. Likewise we explicitly avoid
`manage-repos` — granting delete-repo on the user's whole
namespace would be way more power than session sync warrants.

### Flow

1. User clicks "Sign in with Hugging Face" in the coder panel
   header (or in the connect modal on first open). `moon-coder`
   `POST`s `https://huggingface.co/oauth/device` with
   `client_id=7977dff4-…`, `scope=inference-api contribute-repos`.
2. Response carries `device_code`, `user_code`, `verification_uri`,
   `expires_in`, `interval`. The IDE renders a modal showing the
   user code in a big monospace box and a "Open in browser" button
   that opens `verification_uri` (typically
   `https://huggingface.co/login/device`) via
   `tauri-plugin-opener`.
3. Background poll on `interval` seconds (typically 5 s) against
   `https://huggingface.co/oauth/token` with
   `grant_type=urn:ietf:params:oauth:grant-type:device_code`,
   `device_code`, `client_id`. Returns `access_token`,
   `refresh_token`, `expires_in`, `token_type=bearer`.
4. Persist `(access_token, refresh_token, expires_at)` to the OS
   keyring as a single JSON blob under
   `service=moon-ide`, `account=hf-oauth`. Same keyring backend the
   Slack panel already uses (`apple-native + windows-native +
sync-secret-service + crypto-rust` features in `keyring` 3.x).
5. The IDE pulls the user's profile (`GET https://huggingface.co/api/whoami-v2`)
   for the panel header (avatar, username, namespace). Cached in
   `AppState.coder.identity`.

### Refresh

The HTTP middleware on the inference + bucket clients refreshes the
access token when **`expires_at - now < 60 s`** (covers clock
skew) and on a 401 response (one retry). Refresh is `POST` to
`https://huggingface.co/oauth/token` with
`grant_type=refresh_token`, `refresh_token`, `client_id`. A
successful refresh writes the new triple back to the keyring.

A failed refresh (`invalid_grant`, network outage, …) drops the
keyring entry, fires `coder:disconnected` to the UI, and the panel
returns to the "Sign in with Hugging Face" empty state. The user
re-authorizes; everything else (sessions, bucket pointer) stays.

### Disconnect

Explicit "Disconnect" in the panel header confirms (modal), then:

- Drops the keyring entry.
- Clears `AppState.coder.identity` and any in-memory token.
- Does **not** revoke server-side. HF's revoke endpoint isn't
  reliably documented for device-flow tokens; revocation is the
  user's job from their HF account settings (we link it from the
  confirm dialog).

Token rejection mid-session (HF returns `invalid_token` /
`expired_token` and refresh fails) follows the same path as an
explicit disconnect, plus a toast.

## Providers

### Day-1: HF Inference Providers (only)

One HTTP client, OpenAI-compatible schema, against
`https://router.huggingface.co/v1`:

- `POST /chat/completions` (streaming via SSE when `stream: true`)
- `Authorization: Bearer <access_token>`

The model field carries HF's provider-routing slug verbatim:
`Qwen/Qwen3.5-397B-A17B:scaleway`,
`Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway`. The IDE doesn't parse it; the
router does.

#### Default models

Hardcoded in `crates/moon-coder/src/defaults.rs`:

- **`standard`** → `Qwen/Qwen3.5-397B-A17B:scaleway` — the
  day-to-day default for chat / refactor / multi-step tasks. The
  agent loop and every sub-agent run against this model.
- **`cheap`** → `Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway` —
  used for everything that doesn't need tool calls: auto-rename
  session titles, branch-name suggester, commit-message suggester,
  compaction summaries, folder-summary onboarding.

These are _defaults_. The user picks freely from the catalog at
runtime — see [Model picker](#model-picker). Internally we never
read the constants except as fallbacks when the user's pick is the
empty string.

Per-session model override is stored in the session JSONL header
purely as informational metadata; the runner reads the active pick
from `CoderModels` (panel-global, user-scoped), not from the
session header.

### Model picker

User-facing surface: a cog icon in the coder panel header opens a
popover with four fields:

- **Standard model** and **Cheap model** — wire model ids (e.g.
  `Qwen/Qwen3.5-397B-A17B:scaleway`) sent verbatim to the router.
  Empty string falls back to the hardcoded default. The popover's
  catalog list lets the user click-to-fill against the live
  `https://router.huggingface.co/v1/models` response. The Standard
  list is pre-filtered to models with at least one tool-capable
  provider; the Cheap list is unfiltered.
- **Default provider** — UI hint only, used to auto-suffix the
  picked model with `:provider`. Accepts real provider slugs
  (`scaleway`, `novita`, `together`, `fireworks-ai`, …) and the
  router-side synthetic policies (`fastest`, `cheapest`,
  `preferred`). The picker only applies the suffix when the
  catalog row actually has that provider route, so users don't
  accidentally pin a model to a provider that doesn't serve it.
- **Bill to** — sent as `X-HF-Bill-To` on every inference request.
  Dropdown sourced from `identity.orgs` (which itself comes from
  `/oauth/userinfo` — same call we already make at sign-in time,
  no extra scope / endpoint involved). "Personal account" is the
  always-present default.

The picks live in `AppState.coder.{standard_model, cheap_model,
default_provider, bill_to}` and are hot-swapped into the runner's
`CoderModels` snapshot on save. Mid-turn changes apply to the next
round-trip; in-flight requests are not aborted.

Internal architecture:

```text
                    ┌─────────────────────────────────────┐
                    │  CoderHandle::set_models(...)        │
                    │  (called from coder_set_model_       │
                    │  settings Tauri cmd)                 │
                    └────────────────┬─────────────────────┘
                                     │
                                     ▼
                ┌────────────────────────────────────────┐
                │  Arc<RwLock<CoderModels>>               │
                │   { standard, cheap, bill_to }         │
                └────┬───────────────────────────────────┘
                     │                  │
              read at every             │
              chat-completions          │
              site (turn-start          │
              snapshot)                 │
                     │                  ▼
                     │     ┌────────────────────────────────┐
                     │     │  InferenceClient                │
                     │     │   reads bill_to                 │
                     │     │   per-request → X-HF-Bill-To    │
                     │     └────────────────────────────────┘
                     ▼
            runner / subagent / compaction / folder_summary
```

`default_provider` lives only in `AppState`; the runner doesn't
read it because the suffix has already been baked into the saved
model ids by the time the picker writes back.

#### Why HF as primary

- The team's home turf (every model the team cares about is
  already on HF, including provider-routed access to Scaleway,
  Together, Fireworks, …).
- One auth flow, one HTTP client, no per-provider integration.
- Bills via the user's HF account — we don't ship API keys.
- Adding more providers later is additive: a per-provider config
  array on `AppState.coder.providers[]`. The HF flow stays
  canonical.

### Custom OpenAI-compatible endpoints (OpenRouter, local)

`AppState.coder.providers` is a `Vec<CoderProviderConfig>`:

```rust
struct CoderProviderConfig {
    id: String,           // opaque "prov-<unix-ms>-<rand>"
    label: String,        // user-typed, shown in the picker
    kind: ProviderKind,   // custom | open_router | anthropic
    base_url: String,     // OpenAI-compat /v1 root for custom/open_router;
                          // API host (e.g. https://api.anthropic.com) for anthropic
    standard_model: String,
    cheap_model: String,
    has_api_key: bool,    // server-set off the keyring
}
```

The HF route stays implicit and always available — it has no entry
in this list and is selected via `active_provider: None`. Switching
to a user provider sets `active_provider = Some(id)` and the runner
reads the picks off that entry instead of the HF fields. The
[`X-HF-Bill-To`](#bill-to-org-vs-personal) header is suppressed off
the wire when a user provider is active.

### Per-workspace provider lock

`active_provider` lives in the global `state.json`, so a flip in
one workspace's modal would naturally bleed into every other
workspace on the next launch. That's the right default — most users
have a single preferred provider — but some repos (e.g. one that
relies on Anthropic's prompt-cache for quality, while others happily
flip between HF and OpenRouter) want to opt out.

The opt-out is a per-workspace pin stored on
[`WorkspaceSession::coder_provider_lock`](../crates/moon-protocol/src/session.rs)
in the workspace's `session.json`. Two-variant tagged enum:

```rust
enum CoderProviderLock {
    Hf,                    // pinned to the implicit HF route
    User { id: String },   // pinned to a user-added provider
}
```

Resolution rule: the runner's effective active provider is
`session.coder_provider_lock.unwrap_or(state.coder.active_provider)`.
The picker's `coder_get_model_settings` returns the **effective**
value on `active_provider` plus the lock annotation, so the modal
shows what's actually running with a "Locked to X" label when the
pin is set.

Writes from the picker:

- **Lock on**: persist the picked provider into
  `session.coder_provider_lock`. Don't touch `state.coder.active_provider`.
  Sibling workspaces are unaffected.
- **Lock off**: persist the picked provider into
  `state.coder.active_provider` (the previous behaviour). Clear the
  workspace's lock so the global default takes over.

A stale pin (the locked user-provider id no longer exists in
`state.coder.providers`) falls back to HF the same way a stale
global `active_provider` does, with a `tracing::warn!` noting the
orphan.

`kind` is the wire-shape discriminator:

- `custom` — free-form OpenAI-compatible endpoint. Default for
  back-compat with entries persisted before the field existed; what
  every locally-hosted server (vLLM, Ollama, llama.cpp, LiteLLM,
  …) lands under.
- `open_router` — a built-in preset for OpenRouter. Identical wire
  path to `custom`, but the picker recognises it for the URL
  preset and the API-key dashboard link, and the prompt-cache
  marker code fires on `anthropic/*` slugs without sniffing the
  base URL.
- `anthropic` — Anthropic native (`/v1/messages`). The runner
  takes a separate code path through
  [`crates/moon-coder/src/anthropic.rs`](../crates/moon-coder/src/anthropic.rs):
  auth via `x-api-key` + `anthropic-version`, system prompt as a
  top-level field, tool calls as `tool_use` / `tool_result`
  content blocks, images as base64 `image` blocks, native
  `cache_control: {type: "ephemeral"}` markers, and a different
  streaming SSE event grammar
  (`message_start` / `content_block_*` / `message_delta` /
  `message_stop`). The translator merges adjacent
  `tool` / `user` messages into a single user-role Anthropic
  message because the API rejects two consecutive same-role turns.

For the built-in presets the picker locks `base_url`
(`https://openrouter.ai/api/v1` for OpenRouter,
`https://api.anthropic.com` for Anthropic) and disables the outer
**Save** button until a `standard_model` is picked from the
catalog — the runner has no per-preset hardcoded default and a
blank slug would 404 on the first turn. `cheap_model` stays
optional and falls back to `standard_model` for the same provider
when blank (the previous fallback to the HF cheap default leaked
an HF-only slug onto every non-HF route).

API keys live in the keyring under `service=moon-ide`,
`account=coder-provider:<id>`. The Tauri commands shipping this:

- `coder_new_provider_id` — allocates the opaque id (keyring slot is
  addressable before the config lands in `AppState`).
- `coder_probe_provider { base_url, api_key, kind }` —
  `GET <base_url>/models` for OpenAI-compat kinds (fallback to a
  1-token `chat/completions` ping on 404), or `GET /v1/models`
  with the Anthropic auth headers for `kind=anthropic`. Lets the
  picker verify before saving without committing a half-broken
  config.
- `coder_save_provider` / `coder_delete_provider` — atomic
  per-provider commits straight into `AppState`.
- `coder_set_provider_api_key` / `coder_clear_provider_api_key` —
  keyring-only; secrets never round-trip through the model-settings
  read.
- `coder_list_provider_models { id }` — flat catalog for the
  picker; the runner picks the right wire shape based on the
  provider's `kind` (OpenAI-compat `/v1/models` for
  `custom`/`open_router`, Anthropic native for `anthropic`).
  Differs from `coder_list_models`, which returns the rich HF
  catalog with per-route pricing.

The picker hides the "Bill to" field when a user provider is
active, renders a flat catalog (since pricing / throughput aren't
uniform across OpenAI-compat servers), and treats `localhost` /
`*.local` base URLs as keyless-by-default (Ollama / llama.cpp run
keyless conventionally).

The 401-refresh behaviour stays HF-only: a user provider that
returns 401 surfaces the error to the user — there's no refresh
token, just an API key the user has to fix in the picker.

#### Prompt caching (Anthropic, native or via OpenRouter)

Anthropic prompt caching is opt-in (unlike DeepSeek / Gemini Flash / GPT‑4o, which auto-cache on prefix match), so by default zero caching happens on a Claude model regardless of how repetitive the request is. The IDE enables it automatically on every Anthropic-bound request, regardless of how it's routed:

- **`kind=anthropic`** (native `/v1/messages`): the translator marks the last block of the system prompt and the last block of the most recent user-role message with `cache_control: {type: "ephemeral"}` directly on the Anthropic-native content blocks.
- **`kind=open_router`** (OpenAI-compat through OpenRouter to an `anthropic/*` slug): the inference layer flips selected messages onto the blocks-array shape `{role, content: [{type: "text", text: "...", cache_control: {type: "ephemeral"}}]}` for those messages only — OpenRouter normalises the `cache_control` marker into Anthropic's native shape on the way through. Other messages keep the string form, so the wire shape stays byte-for-byte identical to the no-caching path for every other provider.

Non-Anthropic providers see no `cache_control` at all because `cache_breakpoint_indexes` returns an empty list and `build_wire_messages` then degenerates to the original `String` content. The same is true for `kind=custom` providers regardless of slug — the marker only fires when the route's `kind` says caching is meaningful.

Breakpoint placement (`cache_breakpoint_indexes` in `crates/moon-coder/src/inference.rs`) uses two of Anthropic's four allowed markers per request:

1. **End of system prompt** (always message index 0 in our agent loop). The biggest static piece in every request — base prompt, hardcoded editing rules, the bound-folders block, the folder-summary preamble — adds up to ~6–8 K tokens of identical content across every turn. One marker, one cache write on the very first call, every subsequent call within the 5-min TTL reads the whole system off cache at the 90 % discount.

2. **End of the last non-assistant message in the list** (the most recent user prompt or tool result). Anthropic caches the entire prefix up to and including the marker; the next round-trip's prefix is exactly "this prefix plus the new assistant turn plus any new tool results", so its longest-matching-prefix lookup at start-of-call comes back as a hit covering everything stable. Assistant turns get skipped because our wire layer keeps them on string-content form — assistant content is `None` when the model emitted tool calls only, and there's no text block we could attach `cache_control` to in that case. Walking back one step lands on a tool or user message that always has non-empty string content.

The 5-minute ephemeral TTL is plenty for an interactive agent loop (turns land seconds apart); the 1-hour TTL costs 5× more per cache write and buys nothing for an active session. Anthropic silently ignores cache breakpoints on spans below the 1024-token minimum, so short conversations pay no cache-write surcharge and get no hits — no special-case in our code.

The usage breakdown comes back on the streaming `usage` chunk alongside `prompt_tokens`. We extend `TokenUsage` with `cache_read_input_tokens` and `cache_creation_input_tokens` (default `0` for every provider that doesn't emit them — DeepSeek's own `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` shape uses different field names, so its caching shows up as zero here for now; revisit when we ship a DeepSeek-specific path) and forward them through `CoderEvent::TokenUsage` as `cache_read_tokens` / `cache_creation_tokens`. The panel's `ContextRing` tooltip surfaces them in a `cache: 3.5k read (87 %, -90 %) · 480 written (+25 %)` line whenever either side is non-zero; the line is suppressed entirely when both are zero so non-caching paths don't see clutter. The compaction trigger still keys off `prompt_tokens` because what eats context-window space is the full input regardless of how it was billed.

### Later: Anthropic OAuth (Claude Pro / Max)

A second OAuth flow, parallel to HF's, persisted in its own keyring
slot. Lands when somebody wants subscription billing instead of
API-key / HF-routed billing. Not in the initial sub-phases.

## Tool surface

Every tool is a moon-core method, dispatched through the active
`WorkspaceHost`. A workspace running in a Phase 2 container gets
container-bound tools without the agent loop knowing.

The schema is JSON-Schema in the request to the LLM; the
implementations are typed Rust:

| Tool         | Signature                                                                                                           | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| ------------ | ------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `read_file`  | `(path, start_line?, end_line?) -> { content, start_line, end_line, total_lines, truncated, mtime_ms }`             | `content` is line-numbered: every line is prefixed with `<line_no>\|<line>` (right-aligned, width sized to the largest visible number). The prefix is metadata, not part of the file. `start_line` / `end_line` are 1-based and inclusive; `end_line` is clamped to EOF and the response echoes the _effective_ range so the model can detect short reads. Path may be active-folder-relative or synthetic `/workspace/<name>/...` to address any bound folder (see [Synthetic `/workspace/<name>` paths](#synthetic-workspacename-paths-and-cross-folder-routing)).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `write_file` | `(path, content) -> { path, bytes_written, mtime_ms }`                                                              | Creates parents only if they exist; agent does `bash mkdir -p` first when it needs to. Same path routing as `read_file`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `edit_file`  | `(path, find, replace, occurrence?) -> { path, bytes_written, mtime_ms, occurrence, total_matches, match_mode }`    | `find` is matched against the file in three stages: (1) exact byte substring; (2) once more after unescaping literal `\n` / `\t` in `find` (escape-leakage from the model's tool-call JSON); (3) line-aligned indent-tolerant — strip per-line leading whitespace from both sides, find a line-aligned window, splice the file's original byte range and re-indent `replace` to match the file's indent at that point. `match_mode` in the result is `"exact"` / `"fuzzy_unescape"` / `"fuzzy_indent"` so the model can tell which path took. Empty `find` rejected. Non-unique match without `occurrence` throws (the error lists matching line numbers so the model can disambiguate). Same path routing as `read_file`. Fuzzy paths trust format-on-save to normalise the spliced bytes — the model still aims for byte-exact, fuzzy is the safety net.                                                                                                                                                                                                                             |
| `list_dir`   | `(path) -> DirEntry[]`                                                                                              | Honours the same gitignore-aware walk the file tree uses. Same path routing as `read_file`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `grep`       | `(pattern, case_sensitive?, max_matches?) -> { pattern, matches, count, truncated }`                                | `matches` is one hit per line in `path:line: text` form (line is 1-based). Lines longer than 500 chars are capped with a `… [line truncated, N chars total]` marker so a single hit on an inlined base64 image / minified bundle can't blow the context window — the path + line are intact, so `read_file` with `start_line` / `end_line` is the recovery path. The exact line numbers feed back into `read_file`'s `start_line` / `end_line` so the typical loop is `grep` → narrow `read_file` → `edit_file`. Backed by the existing `ignore`/ripgrep dep.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `bash`       | `(cmd, timeout_ms?) -> { cmd, target, stdout, stderr, exit_code }`                                                  | Routes to `docker exec -w <container_cwd> <name> bash -c <cmd>` when the workspace shell container's lifecycle status is `Running`, else `bash -lc <cmd>` rooted at the active folder. The decision is made by `tools::resolve_bash_target`, which calls the same `moon_container::Workspace::status()` query `lsp.rs` already uses — so terminals, LSP, and the coder agree on the routing target. `target` field echoes `"host"` / `"container"` so the panel pip and the tool result can't drift. **Container:** `bash -c` inherits moon-base Dockerfile `ENV PATH` (fnm, Cargo, Bun, …). A login shell resets `PATH` via Debian `/etc/profile` and skips fnm because non-interactive `bash` exits `~/.bashrc` early, while IDE terminals are interactive and load fnm — `-lc` therefore hid Node from the tool even though the image ships it. **Host:** `bash -lc` (not `sh -lc`) because `/bin/sh` is usually `dash`, whose login PATH ignores typical `~/.bashrc` toolchains; login bash pulls those in. Requires `bash` in the container — already assumed by `moon-terminal`. |
| `task`       | `(task, folder?, mode?, system_prompt?) -> { result, sub_session_id, tokens_used_estimate, mode, iterations_used }` | Delegates a self-contained task to a sub-agent and gets back a single summarised string. `folder` defaults to the parent's active folder. `mode` is `"research"` (read-only intent) or `"agent"` (default; full toolkit, same capabilities as the parent). The sub-agent inherits the parent's everyday driver model — there is no per-call model selector. Multiple `task` calls in one assistant message run in parallel (cap: 4 via `Semaphore`). Sub-agents cannot spawn sub-sub-agents — depth=1 cap is enforced by the parent's tool list including `task` while the sub-agent's does not. Available **only** to the top-level parent turn. (Internal Rust types and the `subagent.rs` module keep the `subagent` naming; only the wire/tool name surfaces as `task`.)                                                                                                                                                                                                                                                                                                           |
| `web_search` | `(query, max_results?) -> { query, results, count }`                                                                | Open-web search via Tavily. `results` is a list of `{ title, url, snippet, published_date? }` entries sorted by Tavily's relevance ranking. `max_results` defaults to 8 and is capped at 20. Only advertised to the model when a Tavily API key is configured (model-settings popover → Web search); without a key the tool is hidden from the definitions list so the model never sees an unusable tool. See [§ Web search](#web-search).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `web_fetch`  | `(url) -> { url, markdown, truncated, bytes }`                                                                      | Fetch a single page and return Jina Reader's markdown extraction. `http`/`https` only; other schemes rejected at the boundary. Body capped at 200 kB — past that, `truncated: true` and the tail is dropped. Always available (Jina's free tier needs no key). Sub-agents in both modes can call it — fetching docs is read-only, no mutation risk. See [§ Web search](#web-search).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |

Tools that arrive **as separate commits when proven needed**, not
in the initial slice:

- `goto_definition`, `find_references`, `hover` — wrappers around
  the existing LSP broker. Cheap and high-value once the LLM
  starts navigating code structurally.
- `git_status`, `git_diff`, `git_blame`, `git_log` — read-only
  wrappers around the existing git layer.
- `apply_diff` — unified-diff applier so the model can emit a
  single patch instead of N `edit_file` calls. Token-efficient on
  multi-file edits.
- `editor_open` — focus a file at line/col in the running IDE.
  Useful so the agent can drop the user at the right place after
  a refactor.
- `todo_write` — see [§ Todo list](#todo-list-tool).
- `ask_user` — see [§ Asking the user](#ask-user-tool).

### Todo list tool

`todo_write` lets the agent keep a small in-context to-do list as
it works through multi-step tasks. Same shape as the Cursor /
pi-mono convention so prompts can be carried over verbatim:

| Field    | Type                                                       | Notes                                                                                                                                   |
| -------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `todos`  | `[{ id, content, status }]`                                | Full list (with `merge=false`) or partial updates (with `merge=true`).                                                                  |
| `merge`  | `bool`                                                     | When `true`, items are matched by `id` and merged; new ids are appended. When `false`, the list is replaced wholesale. Default `false`. |
| `status` | `"pending" \| "in_progress" \| "completed" \| "cancelled"` | Same vocabulary as Cursor's `TodoWrite`.                                                                                                |

Backend semantics:

- The list is **session-scoped**. The loop keeps it on
  `Session.todos` alongside `messages` / `last_usage`. Each
  `todo_write` call appends one `SessionRecord::TodosUpdate`
  carrying the **full** post-merge list to the session JSONL;
  replay walks the records and seeds the in-memory list from
  the **last** `TodosUpdate` (intermediate ones are read but
  discarded). Reset when the user starts a new session; not
  persisted across users (it's per-session, not per-workspace).
- The list **survives compaction**. Compaction folds the older
  prefix of `messages` into a synthetic system summary; the plan
  is orthogonal to that and stays untouched.
- The tool result is the _current full list_ after the update so
  the model always sees its own bookkeeping. The frontend mirrors
  the same payload into a per-folder `coder.todos` bucket.
- Lifecycle (the spec doesn't model "complete the list" or "clear
  the list" as separate operations — both happen via state
  transitions of the same `todo_write` call):
  - **Complete an item** → `merge: true` with that item's id and
    `status: "completed"` (or `"cancelled"`). It stays in the
    list with the new status; the row UI strikes it through.
  - **Restart with a fresh plan** → `merge: false` with a new
    `todos` array. Wholesale replacement.
  - **Discard everything** → `merge: false` with `todos: []`.
  - **New session** → empty list.
- `merge: true` requires all three fields (`id`, `content`,
  `status`) on each item. Unknown ids are appended; existing ids
  are updated in place; items not mentioned are left untouched.
  No field-level partial updates — keeps the schema simple, and
  the model has the full list in context anyway.
- Only one item should be `in_progress` at a time. The tool does
  **not** enforce this — the prompt does, the same way Cursor's
  prompt does. Enforcing it would make benign races (the model
  flips two items in one call) into errors for no benefit.
- The tool is available in **agent** and **research** sub-agents
  too. Sub-agents maintain their own scratchpads (separate from
  the parent's plan); each sub-agent's list lives on its own
  `Session.todos` and persists into its own per-parent JSONL.

Frontend rendering:

- A compact pill in the panel header (next to the context ring)
  shows the dominant status glyph and a `done / total` count.
  Hidden when the list is empty. Clicking the pill expands a
  popover with the full list — status glyphs, in-progress accent,
  strikethrough on completed / cancelled. Closes on outside click
  or Escape.
- Each `todo_write` call renders a per-row body in the transcript
  via `ToolBodyTodoWrite.svelte`, so scrolling back through the
  session shows how the plan evolved.
- The collapsed tool-row hint chip prefers `→ <in-progress
content>` when there's an item in flight, falling back to
  `M / N done` at rest.
- Sub-agent transcripts render the same per-row body for the
  sub-agent's own `todo_write` calls; the **header pill** is
  parent-only — sub-agent plans don't bubble into the parent's
  pill, since they're separate scopes.
- The agent owns the list. The user can't tick items by hand —
  this is the agent's scratchpad, not a shared task tracker.
  Editing-by-user is a separate question that we'd answer by
  having the user _ask_ the agent rather than by mutating the
  list directly.

### Ask user tool

`ask_user` lets the agent pause its turn and ask the user a
multiple-choice question. The natural use case is the moment
where the agent legitimately doesn't know which of two valid
paths to take and asking the human is cheaper than guessing
wrong twice. Inspired by Cursor's `AskQuestion` shape.

| Field            | Type              | Notes                                                                                 |
| ---------------- | ----------------- | ------------------------------------------------------------------------------------- |
| `question`       | `string`          | What the agent wants to know. Rendered as the prompt text.                            |
| `options`        | `[{ id, label }]` | Buttons the user can click. At least 2.                                               |
| `allow_other`    | `bool`            | When `true`, an "Other…" textarea is shown alongside the buttons. Default `true`.     |
| `allow_multiple` | `bool`            | Multi-select with a confirm button. Default `false` (single-select submits on click). |

Wire shape: this is the first tool that needs **bi-directional**
flow between the loop and the panel. The runner blocks the tool
call on a `oneshot::Receiver<UserChoice>`:

1. Loop emits a normal `tool_call` event with `name: "ask_user"`.
2. Frontend renders the prompt block with buttons (and the
   "Other" textarea when allowed).
3. User clicks an option → the panel calls a new Tauri command
   `coder_respond_to_prompt(call_id, response)`.
4. The backend resolves the oneshot; the tool returns
   `{ choice: { id, label, free_text? } }`; the loop emits the
   matching `tool_result` event and continues.
5. If the user aborts the turn (or closes the panel) before
   responding, the cancellation token short-circuits the
   oneshot and the tool returns `Aborted` — no half-state for
   the model to puzzle over.

Concurrency: only one `ask_user` call is in flight at a time
(the loop is single-turn-at-a-time and `ask_user` blocks the
turn). A second call before the first resolves is a loop bug;
the runner asserts on it rather than queueing.

System-prompt guidance: the base prompt tells the agent **not**
to use `ask_user` for clarification it could resolve by reading
files. The tool is for genuine forks where the user's intent is
the missing input — "rename it across the codebase or just
locally?", "delete the old file or keep it for compat?". The
prompt explicitly discourages "should I proceed?"-style polling
because that turns the agent into a confirmation maze.

### Web search

Two tools, both pure outbound HTTP from the IDE process — no
`WorkspaceHost` involvement (there's no workspace to touch).

**`web_search(query, max_results?)`** routes through Tavily's
SERP API and returns `{ title, url, snippet, published_date? }`
entries. We picked Tavily over Brave / Serper / Exa because (a)
its JSON shape is clean and stable, (b) the free tier covers 1k
searches / month per user — enough for an interactive editor
agent — and (c) it's the option that consistently shows up in
LLM-agent benchmarks, so future models will already know how to
use it well. Per-user key, stored in the OS keyring at
`service=moon-ide, account=coder-web-search:tavily`. The tool is
**only advertised to the model when a key is configured** — no
point telling the agent about a tool that's guaranteed to error.

**`web_fetch(url)`** routes through
[Jina Reader](https://jina.ai/reader) (`https://r.jina.ai/<url>`)
and returns clean markdown extracted from the page. No key
needed for the free tier (60 RPM, ample for the agent's actual
fetch rate). Picked over an in-process HTML→markdown extractor
because (a) it's literally one `reqwest::get` and zero deps, and
(b) extraction quality is consistently good across SPAs / doc
sites / blogs that an embedded `readability` port would handle
badly. `http`/`https` only; other schemes rejected at the entry
point. Body capped at 200 kB to keep a huge page from monopolising
the agent's context window — past that, `truncated: true` and
the model knows to fetch a more specific sub-page rather than
re-fetch the same URL.

**Why two tools, not one.** The agent decides for itself whether
the snippets in the SERP answered the question or whether a full
read is worth the tokens; we don't preemptively expand every
result. This is closer to how a human Googles than to a "search
and synthesise" black box, and the auto-compaction machine already
handles the "context too big after a few `web_fetch`es" failure
mode for free. We deliberately do **not** insert a cheap-model
summarisation step between Jina and the agent — that introduces
non-determinism (the summariser drops nuance) and extra latency
for a problem we don't actually have.

**Mode gating.** Neither tool is mode-gated. A Research sub-agent
reading the open web is exactly the kind of read-only inspection
the mode exists for. The mutating-tool gate (`write_file` /
`edit_file`) still applies — `web_search` and `web_fetch` are
strictly read-only against the world, just like `read_file` is
strictly read-only against the workspace.

**Failure shape.** Errors map to `CoderError::ToolFailed` and
flow back to the model as `is_error: true` results. Tavily's
verbatim error body (`{"detail": "Invalid API key"}`,
`{"detail": "quota exceeded"}`) is preserved in the surfaced
message so the user sees what to fix when they look at the
tool-call card in the transcript.

**UI.** The Tavily key is set / cleared in the model-settings
popover ("Web search" section). The key itself never round-trips
back from the keyring; the popover just knows whether one is
configured. Tool results render through dedicated tool-body
components (`ToolBodyWebSearch`, `ToolBodyWebFetch`): the SERP
shows one card per hit with clickable title → system browser
via `tauri-plugin-opener`; the fetch result renders the page's
markdown through the same `CoderMarkdown` pipeline an assistant
reply uses, with a clickable URL header.

### Error model

Tools **throw** on failure (`Result::Err` becomes
`isError: true` in the tool result message back to the LLM).
Returning a string like "ERROR: file not found" as content is
banned — it confuses the model and is the
[explicit pi convention](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md#error-handling).

### Permissions

No popups. The user's safety boundary is the workspace container
(Phase 2), not a per-call confirm dialog. If they ran `bun run dev`
moon-ide outside a container, their bash tool runs on the host —
that's their choice. The panel header surfaces a small "running on
host" / "running in container" indicator so the boundary is
visible, not hidden.

The one exception is **explicitly destructive built-ins** (none in
the initial set, listed here so we don't drift): `git push --force`,
`rm -rf` outside the workspace folder, etc. If we add such a tool,
it gets a one-time confirm. We do not gate `bash` itself; bash
inside a container is the safety story.

### What the LLM sees as system prompt

Concatenated, in this order, with `\n\n` between sections:

1. Hardcoded base prompt (workspace-aware): "You are moon-coder,
   the coding agent built into moon-ide. The user is working on
   …; here are the tools you have …".
2. **`AGENTS.md`** from the active workspace root, with
   **`CLAUDE.md`** as a fallback for projects that came from the
   Claude / Anthropic ecosystem. Both are matched case-insensitively
   against the folder's top-level entries; AGENTS.md wins when
   both are present so a project that ships both has one
   canonical source. Verbatim contents up to a 20 KB cap, then a
   `... (truncated)` sentinel so the model knows it didn't see
   the tail. Walked-up parent dirs (a la `.editorconfig` / `git`)
   is on the 6.6 work list — today the read is "active folder
   root only" since that's where every team we've seen actually
   keeps these files.
3. **Skills** discovered from these directories under the active
   workspace folder (or any parent — same walk-up convention as
   `AGENTS.md`):
   - `skills/<name>/SKILL.md` — the project-local convention used
     by `agentskills.io` and the `pi`/`claude` agent ecosystem.
   - `.claude/skills/<name>/SKILL.md`
   - `.cursor/skills/<name>/SKILL.md` and `.cursor/skills-*/<name>/SKILL.md`
   - `.agents/skills/<name>/SKILL.md`

   Each skill is a `SKILL.md` whose frontmatter declares `name` +
   `description` (per the [agent-skills standard](https://agentskills.io)).
   All discovered skills' descriptions are listed in the system
   prompt as a "you may invoke skill X by reading file Y" index;
   the body of a skill is loaded via `read_file` only when the
   agent decides to use it. This keeps the system prompt small.

We do **not** ship a skill installer / skill package manager.
Skills are file conventions; the user drops `SKILL.md` into one of
the supported directories.

## Sessions

### Multi-session per project

Every bound workspace folder has its own in-memory session, its own running-turn cancel handle, and its own sessions-list cache. Switching the active workspace folder doesn't touch other folders' sessions, so an agent running in folder X keeps streaming events while the user is browsing folder Y. When the user switches back to X, the panel re-renders against X's bucket — the transcript, the sub-agent cards, the composer draft and attachments are all where they were left.

The backend keeps the per-folder runtime state in `CoderState.sessions_by_folder: HashMap<Utf8PathBuf, Arc<FolderSession>>`. Lazy creation: a `FolderSession` materialises the first time a command needs one for that folder. Currently we never garbage-collect entries on folder unbind — they sit in the map until process exit. Cheap (a `Mutex<Session>` and a `Mutex<TurnState>` per folder; nothing per-folder allocates more than that until tools actually run), and rebinding the same folder gets the same in-memory state back.

Tools captured by a running turn close over the **session's bound folder**, not the live `WorkspaceRegistry::active_folder()`. That's how "agent in folder X stays bound to folder X" actually works at the tool layer: the spawned `run_turn` task carries an `Arc<FolderSession>` plus the folder string, and its `ToolContext` uses that folder for every dispatch. Switching active folder mid-turn cannot redirect tool calls.

`abort` operates on the **active folder's** turn only. Stopping a background turn requires switching to that folder first. Sign-out is the one global exception — it cancels every running turn since the auth identity backing them just went away.

Per-folder hydration on launch: `AppState.coder.last_session_by_folder: Map<folder, sessionId>` records the last-opened session per project; the panel restores the active folder's entry on first mount, and again whenever the user switches back to a folder it hasn't visited yet in this session. The pointer is updated by both `coder_open_session` (explicit click in the sessions list) **and** `coder_send` (so a fresh `new_session` + first message refreshes the pointer — without that nudge the entry would still point at whichever transcript the user had open before clicking "new"). If the pointer goes stale (session JSONL deleted out-of-band, data dir wiped), `#hydrateSession` swallows the missing-file error and falls through to the sessions list rather than mounting an inline error row; the first subsequent `open_session` or `send` overwrites the stale entry, so the failure mode self-heals.

Hydration is gated on a workspace-ready signal. The cold-start critical path is: (1) `coder.wireRuntime` binds the `coder:event` listener early so an in-flight turn (e.g. resumed across HMR) keeps streaming into the right bucket; (2) `coder.setActiveFolder` runs from `adoptWorkspaceSnapshot` and **queues** a hydrate but doesn't execute it; (3) `restoreAppState` walks `session.folders`, temporarily switching the **backend's** active folder (`workspace_set_active_folder`) for each one to load its tabs through the right host; (4) once the loop finishes and the active-folder pointer is finalised, `state.svelte.ts` calls `coder.markWorkspaceReady()`, which fires the deferred hydrate. Without this gate, step (3) and the hydrate's `coder_list_sessions` / `coder_active_session` / `coder_open_session` calls (all of which read through the backend's mutable active-folder pointer) race the loop and the panel paints another folder's session list under the active folder's bucket. Folder switches after launch don't have this problem — the loop only runs at hydration time — and `setActiveFolder`'s queued hydrate fires immediately for any not-yet-visited folder.

The wire format reflects all of this: `coder:event` payloads are wrapped in a `CoderEventEnvelope { folder, event }` so the frontend can route updates to the right per-folder UI bucket. Sub-agent events arrive tagged with the **parent's** folder (sub-agents belong to whichever project originated them), so a parent in folder X with a sub-agent operating against folder Y still shows the sub-agent's collapsed card under X's transcript.

### On disk

Append-only JSONL at
`<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/<session-id>.jsonl`.
The slug is `<basename>-<8-char FNV-1a hex>` derived
deterministically from the workspace folder's absolute path —
two folders that share a basename get distinct slugs, and the
same folder always maps to the same slug across launches.

The first line is a header; every subsequent line is one
[`SessionRecord`](../crates/moon-coder/src/sessions.rs):

```jsonl
{"schema":1,"id":"sess-1746440000123-9e3779b1","title":"implement bucket sync","created_at_ms":1746440000123,"updated_at_ms":1746440045871,"model":"Qwen/Qwen3.5-397B-A17B:scaleway"}
{"kind":"user","text":"do the thing"}
{"kind":"assistant","content":"sure…","thinking":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{...}"}}]}
{"kind":"tool","tool_call_id":"call_1","content":"…"}
{"kind":"usage","prompt_tokens":1234,"completion_tokens":56,"total_tokens":1290}
{"kind":"assistant","content":"done"}
{"kind":"usage","prompt_tokens":1340,"completion_tokens":18,"total_tokens":1358}
{"kind":"title_update","title":"add bucket sync upload task"}
```

The system prompt isn't persisted: re-opening a session re-adds
the current default at load time, so prompt updates between
releases apply retroactively. The header carries metadata
(`schema`, `id`, `title`, `created_at_ms`, `updated_at_ms`,
`model`); a `title_update` record overrides the header's title on
load (auto-rename uses this — see below).

Provider-supplied token usage gets persisted as a `usage` record after every parent-loop round-trip whose response carried a `usage` chunk. The fields mirror [`TokenUsage`](../crates/moon-coder/src/inference.rs) (`prompt_tokens`, `completion_tokens`, `total_tokens`, plus `cache_read_input_tokens` / `cache_creation_input_tokens` when caching kicked in — those last two skip-serialise when zero, which is most providers). On reopen, the runner walks every record, remembers the **last** `Usage`, and uses it as the seed for both the in-memory `last_usage` (so the auto-compaction trigger has a real number to compare against on the very next prompt) and the synthetic restore-time `TokenUsage` event the panel turns into the context-usage ring. Sessions written before this variant shipped, or sessions whose final round-trip didn't yield a `usage` chunk, fall back to a bytes/4 estimate of the rebuilt history — same number the panel used to render at restore time before persistence landed. Sub-agent JSONLs follow the same shape; `open_session` only reloads top-level transcripts today, so persisted sub-agent usage is forensic value plus future-proofing for whenever sub-agent restore lands. Bytes/4 estimates aren't persisted: they're recomputable from the messages, so storing them would just bloat the file with redundant approximations.

Sessions explicitly **don't** live inside the project tree.
They're personal scratch / history rather than project
artefacts: putting them under VCS would either litter
`git status` or force a `.gitignore` entry every team adds
blind, and tying them to the on-disk path rather than the
user's account would make them follow a `git mv` of the repo
in confusing ways. The shared `moon-ide/` data dir under
`XDG_DATA_HOME` (next to compose state) is the right home.

Lazy persistence: an empty session has no file on disk. The
header is written on the first `append_record` call so spamming
the `+` button doesn't litter the directory with empty sessions.

A crash loses at most the in-flight event.

### Auto-rename

After the first turn of a fresh session **finishes** — successfully, aborted, or errored — the runner fires a one-shot fast-model call asking for a 4-6 word title against whatever made it into the transcript so far. That title:

- Replaces the truncated-prompt fallback in the header (in memory).
- Gets persisted as a [`TitleUpdate`] record so re-opening sees it.
- Emits `session_title_updated` so the sticky header + sessions list update without a re-fetch.

The "any first-turn outcome triggers it" rule matters because long tool-heavy turns are routinely Esc'd by the user before the assistant produces its final wrap-up text. Under an "Ok-only" rule those sessions kept the truncated-prompt fallback forever; firing the rename on any outcome means even a session that was Esc'd seconds in still gets a real title from the user prompt + whatever assistant content + tool results landed.

Concurrency: the `auto_rename_pending` flag is captured-and-cleared inside the same critical section that sets the truncated-prompt title, so a second `send` racing with the spawned rename task can't double-spawn it. Failures (model down, response empty / over-long, session switched mid-flight) keep the truncated-prompt title — it's a serviceable fallback. The pass only runs once per session.

### Sidebar UI

The panel has two views — they share the existing right-side
slot via `rightPanel.kind === 'coder'`:

- **Session view** (`coder.view === 'session'`). The transcript
  - composer, plus a sticky `← Sessions | <title> | +` strip
    above. Default view on panel mount: if the runner already has
    a session in memory or `AppState.coder.last_session_id` points
    at an existing one, the session view opens to that.
- **Sessions list** (`coder.view === 'list'`). Sticky
  `Sessions | +` header; a row per persisted session showing the
  title plus a relative `updated_at_ms`. Hovering a row reveals
  two icon buttons on the right: an "open trace" `</>` button and
  a trash icon (with a confirm dialog on click). Clicking the
  body of a row opens the session. The row whose id matches the
  current folder bucket's `activeSession` and whose bucket is
  `busy` paints a pulsing accent dot left of the title and a
  `running…` label in the meta row, so a user juggling background
  agents can see at a glance which session is mid-turn from the
  list view (the busy state is per-folder; only one session per
  project can be running at a time, so at most one row pulses).

`+` from either view drops into a fresh empty session and lands
focus in the composer. Empty sessions don't persist until the
first user message lands; that message creates the JSONL file
and seeds the title from the prompt.

### Open the raw trace in the editor

Each session row, the active-session header, and the sub-agent
pop-out header all expose a `</>` icon that opens the session's
on-disk JSONL as an editor tab. The button calls
`coder_session_jsonl_path(id)` to resolve the absolute host path,
then routes through `workspace.openHostFile` — the same
host-direct file mechanism `Ctrl+O` uses for paths outside the
active folder (see [test plan 0051](test-plans/0051-open-host-file.md)).
This means traces open identically whether the project is local
or running in a container: the JSONL always lives on the host's
`XDG_DATA_HOME`, never inside the container, so there's no
docker-exec round-trip and no path translation.

The trace is editable by default — we don't lock it because the
tab is for power-user inspection, and the cost of an edit guard
isn't worth the value. A corrupted line at worst makes a future
`coder_open_session` log a `tracing::warn!` and skip the record.
Empty sessions (created with `+ new` and never sent to) have no
file on disk, so the button surfaces a flash toast instead of
opening a phantom tab. The buffer doesn't auto-tail: to see
appended turns, close and re-open via the same button.

### Composer attachments

The user can attach an editor selection to the composer via
`Ctrl+L` (mirrors Cursor's "add to chat" gesture). Mechanics:

- The active editor publishes its non-empty selection to a
  workspace-level `activeSelection` snapshot (path + 1-based
  inclusive line range + the selected text captured at the
  moment of update). Empty selections clear the snapshot.
- The editor pane shows a small floating `Ctrl+L Add selection
to Coder` pill in its top-right corner while the snapshot
  belongs to that pane's file. The pill is keyboard-only — its
  job is to remind the user the gesture exists, not to
  duplicate it as a click target.
- `Ctrl+L` reads the snapshot and (a) inserts an inline
  `@path:start-end` token at the textarea's caret, (b) adds a
  matching `ComposerAttachment` to `coder.attachments`, (c)
  opens the panel, (d) pulls focus to the composer. The chip
  list dedupes by `(path, range)` so a hammered Ctrl+L only
  adds one chip — but every press inserts a fresh inline
  token, matching Cursor's "you can reference the same
  selection at multiple spots in the prose" behaviour.
- Each attachment renders as a chip above the textarea:
  `[doc-icon] basename:start-end [×]`. Click the chip body to
  jump to the captured range (`workspace.jumpTo`); click `×`
  to drop the chip _and_ strip every inline token (`@token`
  with at most one trailing whitespace) out of the draft so
  the chip and the inline references stay in sync.

#### Wire shape (matches Cursor)

The user prose stays intact, with `@`-tokens inline at the
positions the user picked. The captured snippet contents and
the implicit "active file" hint both land in a trailing
`<context>` block:

```
explain the difference between @src/lib/foo.ts:48-50 and
@src/lib/foo.ts:63-67

<context>
<active_file path="src/lib/foo.ts" />
<code_selection path="src/lib/foo.ts" lines="48-50">
[lines 48-50 verbatim]
</code_selection>
<code_selection path="src/lib/foo.ts" lines="63-67">
[lines 63-67 verbatim]
</code_selection>
</context>
```

Splitting the two means a multi-attachment prompt reads
naturally instead of inflating the prose with a wall of code
headers, and the `<code_selection path lines>` element is the
same wire shape Cursor's composer ships, which the model has
already seen plenty of in training. The wrapper element is a
sufficient delimiter — no need to fence the body — so a
snippet that contains its own triple-backticks rides through
unmangled.

`<active_file path="…" />` is self-closing — no body, no
selection range, just the workspace-relative path of whichever
file the user has focused in the editor at send time. Ships on
every turn the user has a routable file open (skipped for
untitled buffers, external host-direct buffers, and
working-tree-deleted buffers — none of them are addressable by
the agent's tools). The model uses it as a "current focus"
hint: questions like "explain this" or "add a test for the
function I'm looking at" route correctly without the user
needing to `Ctrl+L`. Contents are deliberately _not_ shipped
implicitly — that stays `Ctrl+L`'s job; the model can
`read_file` on its own when it actually wants the bytes. The
hint lives in the user message (per-turn) rather than the
system prompt because tab switches would otherwise bust the
inference router's prefix cache. The `parseUserPrompt`
renderer in `CoderPanel.svelte` ignores the element (no chip
rendered) — the user already knows what they have open, and
the raw value is still visible in the on-disk session JSONL
for anyone debugging.

Empty draft + non-empty attachments is a valid send (the
context block ships on its own); empty + empty is a no-op even
when an active file is focused — the implicit hint never
auto-fires a turn on its own.

The text snapshot lives with the chip — a follow-up edit to
the file does not change what the agent sees. That matches
Cursor's behaviour and is the safer interpretation: the user
asked about the code as it stood when they attached it, not as
it stands at send time.

### Image attachments

The user can paste images (e.g. screenshots from the system
clipboard) directly into the composer textarea. Mechanics:

- Paste handler on the composer reads `ClipboardEvent.clipboardData.items`,
  pulls out anything with `kind === 'file' && type.startsWith('image/')`,
  and routes each blob through `coder.addImageAttachment(blob)`.
  Mixed payloads (image + text representation, common when
  copying from screenshot apps) attach the image and let the
  text portion paste.
- Each image becomes an `ImageComposerAttachment` chip in the
  same chip strip as selection chips, with a thumbnail in place
  of the file icon. The × removes it; sending clears the strip.
  Cap: 4 MB per image (decoded), 10 images per send — the user
  gets a friendly inline error if they try to exceed either.
- On send, image attachments are split out of the chip list and
  shipped to `coder_send` as a separate `images` argument
  alongside the text+context payload. The Rust side carries them
  on the `ChatMessage::User` variant and emits OpenAI-compatible
  `{"type": "image_url", "image_url": {"url": "data:..."}}` blocks
  on the wire (OpenRouter normalises this into Anthropic's
  `image` block on the way through).
- Persisted in JSONL via `SessionRecord::User { text, images }`
  with `skip_serializing_if = "Vec::is_empty"`, so existing
  no-image transcripts keep their old line shape and replay on
  any build.
- Replayed on session reopen as a `CoderEvent::UserMessage` that
  carries the images through to the user's bubble, which renders
  them as clickable thumbnails (click → open full-size in a new
  tab via the data URL).

### Compaction

Long sessions exceed the model's context window. moon-coder ships
a **single compaction strategy** — "summarize older turns" — and
calls it automatically when the next turn would exceed the model's
context limit (provider-reported), or manually via a "Compact"
command. The summary turn is a fast-model call against the older
prefix; the result becomes a synthetic `system` message that
replaces the prefix in the LLM context. The full JSONL on disk is
untouched (so a future "view raw history" affordance can still
show the original).

## Bucket sync (HF buckets)

Buckets are HF Hub's S3-like object storage backed by Xet — see
[the official guide](https://huggingface.co/docs/huggingface_hub/guides/buckets)
and [the Buckets API PR](https://github.com/huggingface/huggingface_hub/pull/3673).
They're a different `repo_type="bucket"` from models / datasets /
spaces. moon-coder uses one bucket per user to keep the team's
session history in one place.

### Bucket layout

- **One per-user private bucket**, name `moon-ide-sessions`. Created
  on first sign-in with the API equivalent of
  `create_bucket("moon-ide-sessions", private=true, exist_ok=true)`.
- Key prefix per workspace: `<workspace-slug>/<session-id>.jsonl`
  where `<workspace-slug>` is a stable short hash of the
  workspace's canonical absolute path (e.g.
  `<sha256(path)[:16]>`). Renaming a folder doesn't fork its
  history; moving a checkout to another machine still lines up.
- A small `<workspace-slug>/manifest.json` lists the workspace's
  human-readable name, last-seen path, and session-id → first-line
  preview. Useful for a future "browse remote sessions" UI.

### Sync cadence

A debounced background task per session:

- Local writes are append-only, so the sync task knows the new
  byte offset. Every **N seconds of quiescence** (default 5 s)
  after the last local write, the task uploads the **whole file**
  again — Xet dedup makes this nearly free, since the unchanged
  prefix re-uploads as a hash-only reference.
- Force-flushes on session close (panel hidden / app exit).
- The task uses `hf-xet`'s `XetSession` for the byte transfer and
  the Hub REST API (`/api/buckets/...` endpoints) for create /
  list / delete operations. We don't shell out to the Python
  `huggingface_hub` CLI.

### Privacy / opt-out

Default: **on for every workspace once signed in**. The first time
a fresh workspace's session uploads, the panel shows a one-time
banner:

> Syncing this workspace's coder sessions to your private HF bucket
> `moon-ide-sessions/<workspace-name>`.

The banner has an inline toggle next to that text — flipping it off
sets `AppState.coder.sync_disabled_workspaces[]` for the workspace
and dismisses the banner. The same toggle is reachable from the
panel header after the banner is gone, so the off-switch is always
one click away.

A failed upload is a `tracing::warn!` plus a small status-bar pip
("coder sync delayed"). Local JSONL stays the source of truth.

### What never leaves the host

- HF access / refresh tokens (keyring only).
- Provider API keys for any custom provider (keyring only).
- File contents that the agent _reads_ during a session **do**
  end up in the JSONL (they're part of the tool result message),
  and therefore in the bucket. NDA workspaces should toggle sync
  off; the banner exists exactly for this case.

## Token accounting and auto-compaction

Long sessions end up larger than any model's context window if the loop is just left to run. We solve that with two layered mechanisms: a per-turn token-usage report (so the user can _see_ what's happening) and an auto-compaction pass (so the loop _keeps working_ when the prompt would otherwise exceed the window).

### Token usage report

The runner emits [`CoderEvent::TokenUsage`](../crates/moon-coder/src/event.rs) at three points per round-trip so the panel ring and the auto-compaction trigger see the closest-to-current number at every moment:

1. **Pre-call** (Estimate). Right before sending the prompt to the provider, the runner emits a `TokenUsage` with `prompt_tokens` set to the bytes/4 estimate of the message array and `completion_tokens = 0`. The ring jumps the moment the user hits send (or a tool result lands), instead of waiting for the SSE stream to finish.
2. **Mid-stream** (Estimate, throttled). While content / thinking deltas stream back, the runner accumulates byte length and re-emits `TokenUsage` at most every 500 ms with the growing `completion_tokens`. Long generations visibly fill the ring.
3. **Post-call** (Provider when available, Estimate otherwise). The exact provider numbers from the streaming `usage` chunk replace the estimate the moment the round-trip finishes.

```ts
{
  kind: 'token_usage',
  prompt_tokens: number,       // size of the prompt the model just saw
  completion_tokens: number,   // size of the response we just got back
  total_tokens: number,
  context_window: number,      // hardcoded per model in `defaults::context_window_for`
  source: 'provider' | 'estimate'
}
```

The numbers come from the OpenAI-compatible streaming `usage` chunk that providers emit when we set `stream_options: { include_usage: true }` on the request. `source: 'provider'` means those figures are exact; `source: 'estimate'` means the provider didn't emit a `usage` chunk and we fell back to a `bytes / 4` approximation (the conventional ratio for English text + tool JSON across the Qwen / Llama / DeepSeek families). The frontend tints the panel ring identically in both cases and adds a `≈` to the tooltip when source is `estimate`.

`context_window` is sourced from each provider's `/v1/models` catalog (HF router, any user-added OpenAI-compatible endpoint, or Anthropic's native `/v1/models` which exposes the per-model context as `max_input_tokens`) and cached in `CoderModels::context_windows`. The cache is primed in the background at startup and on every active-provider change, refreshed as a side-effect of every picker fetch (`coder_list_models` for HF, `coder_list_provider_models` for user providers), and merged across routes — switching providers in the picker doesn't blow the previous route's entries away. Lookups consult the cache first (with and without the `:provider` suffix the user may have pinned onto the slug), then fall through to a tiny static table in [`defaults::context_window_for`](../crates/moon-coder/src/defaults.rs) for the team's HF defaults, and finally to a 128k fallback with a `tracing::warn!`. The static table is the cold-start safety net for the very first turn after a fresh launch when the prime hasn't landed yet; in steady state every slug the user can actually pick has an authoritative entry in the cache.

**User-set caps.** The picker also exposes a per-tier "Cap context to … tokens" input next to each model field. Values land in `CoderModelSettings.context_window_overrides` (a `slug → tokens` map persisted on `CoderAppState`) and clamp the catalog-derived window with `min(catalog, cap)` at every `CoderModels::context_window` call site. Use case: a model that advertises a 1M-token window but degrades past ~250k — capping arms auto-compaction earlier and keeps the usage ring honest without forcing the user to remember to switch models. Lookup mirrors the catalog: full slug first (so `Qwen/...:scaleway` can carry a route-specific cap), then the suffix-stripped base id (so a bare-id cap covers every routed flavour). A `0` value is treated as "no cap" defensively, and saved entries with `0` are dropped on persist so a `Clear` gesture doesn't litter `state.json` with inert rows. Caps survive a model swap inside the same provider — the map is keyed by slug, not by tier, so flipping the standard slug back to a previously-capped one picks the cap up again.

The runner re-emits `TokenUsage` for every folder session whenever the cache or the active model changes — that way the ring's denominator repaints the moment the user flips models or the prime fetch lands, instead of waiting for the next round-trip. Token counts (`prompt_tokens`, `completion_tokens`, cache fields) carry over from the session's cached `last_usage`; only the `context_window` denominator changes.

The frontend's `ContextRing` component lives in the panel header — a small circular progress arc filled to `prompt_tokens / context_window`. Tone: muted under 60 %, warning under 80 %, danger at or above 80 %. Pulses while a compaction summary is being written (see below).

### Auto-compaction

When a [`TokenUsage`](../crates/moon-coder/src/event.rs) event reports `prompt_tokens / context_window ≥ 0.80`, the next iteration of the loop runs an auto-compaction pass before sending. The pass:

1. Walks the in-memory message history backwards, counts the most recent **6 user turns**, and uses the oldest of those as the **cut point**.
2. Calls the **cheap model** (see [Model picker](#model-picker)) with a fixed system prompt that asks it to summarise the prefix (`messages[1..cut]`) — covering user intent, decisions, files touched, current state, and what to do next. Output is plain markdown; the call is non-streaming and has no tools.
3. Replaces the prefix with a single synthetic [`ChatMessage::System`](../crates/moon-coder/src/inference.rs) carrying the summary (with a header that distinguishes it from `messages[0]`, the composed system prompt). The leading system prompt is **not** reinjected — `runner::refresh_system_prompt` runs at the top of every turn anyway and recomposes it from `AGENTS.md` + bound-folder summaries + folder-summary cache, so the compaction summary at `messages[1]` rides under whatever the next turn's fresh system prompt produces.

```text
before:
  [system: composed]                               messages[0]
  user … assistant … tool … assistant … user …    long history
  user (most recent)

after:
  [system: composed]                               messages[0]   (rewritten next turn)
  [system: <summary of older middle>]              messages[1]   (new)
  user … assistant … tool …                       last 6 user turns kept
  user (most recent)
```

The on-disk JSONL transcript is **not** rewritten — the full history stays on disk so pop-out / debug / audit see everything; only the in-memory prompt the next round-trip sends gets compacted. As a consequence the on-disk file can be longer than what the agent currently has in front of it; that's intentional. To keep replay reaching the same compacted shape, the runner also appends a [`SessionRecord::Compaction { summary, messages_compacted }`](../crates/moon-coder/src/sessions.rs) record at the point the live drain happened. On reload, replay rebuilds messages linearly until it hits the `Compaction` record, drops everything since `messages[0]` (the composed system prompt), inserts the synthetic summary system message, and keeps replaying newer records on top. Without this record, reopening a long compacted session would re-inflate the entire pre-compaction transcript and the next turn would instantly trip the provider's context-length cap.

Two events fire around the pass:

```ts
{ kind: 'compaction_started', messages_compacted: number }
{ kind: 'compaction_complete', summary: string, prompt_tokens_after: number }
```

The frontend renders these as a single full-width row at the bottom of the transcript: a "compacting…" pip while the fast-model call is in flight, flipping to a `<details>` showing the summary once `compaction_complete` lands. The ring also pulses while compaction is running.

If the fast-model call fails or returns an empty summary, compaction is skipped (the agent keeps going with the uncompacted prompt; a warn-level log gets dropped) and `compaction_complete` fires with an empty summary so the UI's "compacting…" pip clears.

Sub-agents run the same compaction pass at the same threshold against their own message list. That's why the previous byte-budget cap was removed — auto-compaction handles the "context too big" failure mode that the byte cap was approximating, without forcing the sub-agent to bail with a partial result.

Threshold ([`COMPACT_THRESHOLD`](../crates/moon-coder/src/compaction.rs) = `0.80`), retained recent turns (`RECENT_USER_TURNS_KEPT` = `6`), and the summary system prompt are hardcoded today. They become user-tweakable when a real workload demonstrates a need for it; until then "hardcode first, configure later".

### Iteration cap and final wrap-up

The parent loop is capped at [`MAX_TURN_ITERATIONS`](../crates/moon-coder/src/defaults.rs) = `200` tool-call roundtrips per user prompt. Hitting the cap does **not** bail with a bare error banner — the runner appends a sentinel user message (`[Tool-call budget exhausted: …]`) and runs one final tools-disabled round-trip with `tools = []` so the model can write its best answer using what it's gathered. The sentinel is persisted in the JSONL and rendered in the panel like any other user message, so it's obvious in the transcript why the assistant suddenly stopped using tools. Same for sub-agents (50 iterations).

This is distinct from per-tool errors: those continue to be shipped back as `tool_result.is_error = true` JSON the model sees on the next iteration, so it can retry, choose a different tool, or recover. Only the loop-level "out of iterations" failure mode triggers the wrap-up; cancellation (`Esc` / `CoderError::Aborted`) and inference errors still bail without a wrap-up since the loop is already torn down.

## Sub-agents

The parent's loop exposes the `task` tool (see the [tool surface](#tool-surface) above for the schema). One call dispatches one sub-agent; the parent's tool call awaits the sub-agent's report, then the model continues with that text in its context. Multiple `task` calls in a single assistant message dispatch concurrently, bounded by a 4-permit semaphore so a stampede on the inference router stays well-behaved.

The wire-level tool name is `task` because that's what every other agent product the team has used calls this primitive, and the model picked up on it without prompting; we kept the Rust-internal naming (`Subagent`, `subagent.rs`, `SubagentSpawned`) since internally that's what gets spawned, but the surface the model sees and the user reads is `task`.

```jsonschema
task(
  task: string,                    // self-contained description; sub-agent doesn't see the parent's transcript
  folder?: string,                 // basename of a bound folder, default = parent's active folder
  mode?: "research" | "agent",     // default = "agent"
  system_prompt?: string           // overrides the mode-default prompt
) -> {
  result: string,                  // the only string the parent's model sees
  sub_session_id: string,          // the UI's pop-out lookup key, persists across IDE restarts
  tokens_used_estimate: number,    // provider-supplied total when available, falls back to bytes/4
  mode: "research" | "agent",      // echoes the mode the sub-agent actually ran under
  iterations_used: number          // tool-call roundtrips consumed
}
```

There used to be a `model: "fast" | "large"` selector on this tool. We dropped it — the model picker had two effects neither of which justified the surface area: (a) it implied sub-agents were second-class workers, which biased the parent toward refusing to delegate non-trivial tasks; (b) it duplicated a tier choice the team doesn't actually exercise. Sub-agents now inherit the parent's **standard** model (see [Model picker](#model-picker)). The cheap model is still used internally (auto-rename title generator, branch-name suggester, commit-message suggester, compaction summaries, folder summaries) but is not reachable from any tool surface or sub-agent argument.

### Modes

Two operational modes — the model picks per spawn, defaults to `agent`:

- **`research`** — read-only intent. Tools: `read_file`, `list_dir`, `grep`, `bash`. The shell stays available so sub-agents can run inspection commands (`git log`, `git diff --stat`, `cargo check`, `pytest --collect-only`, …) without us having to whitelist every read-only shell idiom as a separate tool. The "no mutation via `bash`" half of the constraint is **behavioural** and lives in the sub-agent's system prompt — `write_file` and `edit_file` are blocked at the [`ToolRegistry::dispatch`](../crates/moon-coder/src/tools.rs) boundary regardless of what the prompt says.
- **`agent`** — full toolkit. Adds `write_file` and `edit_file`. Same capabilities as the parent. Use for "do this scoped task and report back" workflows.

Top-level parent sessions are always `agent` — there is no parent-side toggle. Mode is a sub-agent-level concept.

The `agent` variant used to be called `coder`. We renamed it because the parent model (which writes `mode: "..."` arguments based on the schema) consistently treated a `coder` sub-agent as less capable than itself and hesitated to delegate non-trivial work to one. `agent` reads as "another instance of you" in the model's vocabulary and lifted that hesitation in dogfooding.

### Folder targeting

Sub-agents target one already-bound workspace folder. Defaults to the parent's active folder; explicit `folder` argument takes any other bound folder by basename (or by absolute path as a fallback). Targeting an unbound path errors with `Err(CoderError::ToolFailed)` so the parent's model can recover.

The parent's path-taking tools (`read_file`, `list_dir`, `write_file`, `edit_file`) reach **any** bound folder via the synthetic `/workspace/<name>/...` form, so sub-agents are no longer required for cross-folder file access. They are still useful (and prompt-recommended) when:

- The investigation would pollute the parent's context (read 30 files, report one paragraph).
- Independent investigations can run in parallel (one assistant message issues N `task` calls at once, capped at 4 by [`tokio::sync::Semaphore`](../crates/moon-coder/src/runner.rs)).
- A self-contained piece of work deserves a fresh agent free of the parent's prior context.

In other words: cross-folder _access_ is the parent's own tools; cross-folder _delegation_ is `task`. The previous design rejected cross-folder paths and forced delegation; that ended up pushing the model into multi-step rummaging when a single targeted edit on a sibling folder was the right move. The current design lets the parent take the targeted route when it has the answer, and reserves sub-agents for the cases where the answer-vs-input ratio actually justifies their overhead.

### Budget

Each sub-agent shares the parent's [`MAX_TURN_ITERATIONS`](../crates/moon-coder/src/defaults.rs) cap on tool-call roundtrips. Sub-agents previously ran a tighter 50-iteration ceiling on the assumption they were always small scoped tasks, but in practice the team delegates real refactors to them and a tighter cap just bailed mid-flight; we dropped the separate constant. Hitting the cap doesn't bail with a stub — the sub-agent runs one final tools-disabled round-trip (`tools = []`, sentinel user message asking for a wrap-up) so the parent gets a real summary back. The wrap-up is prefixed with a `[Sub-agent reached the N-iteration cap; final wrap-up follows.]` note so the parent's model knows the budget was exhausted; if the wrap-up call itself fails or returns empty, the sub-agent falls back to the historical canned `Sub-agent stopped after N iterations …` string. The previous byte-budget cap was removed when [auto-compaction](#auto-compaction) shipped — sub-agents now compact their own history at the same threshold the parent uses, which handles the "context too big" failure mode the byte cap was approximating without forcing the sub-agent to bail mid-flight.

### Persistence

Sub-agent transcripts persist as JSONL at `<XDG_DATA_HOME>/moon-ide/coder-sessions/<parent-folder-slug>/<parent-session-id>/<sub-id>.jsonl`. The slug directory uses the **parent** folder's slug — sub-agents belong to whichever project originated them, not whichever folder they happened to operate against — and the per-parent-session subdirectory groups every sub-agent that ran during that conversation. The subdirectory is created lazily on first sub-agent spawn (so a session that never spawns one doesn't leave an empty directory behind) and is removed wholesale by [`sessions::delete`](../crates/moon-coder/src/sessions.rs) when the user deletes the parent.

[`sessions::list_sessions`](../crates/moon-coder/src/sessions.rs) reads the slug directory flat and filters by `*.jsonl` extension, so the per-parent subdirectories naturally fall through and only top-level sessions land in the picker. The "open trace" IPC takes a single id and routes by id prefix: `sess-...` resolves directly via `session_path`, `sub-...` falls back to [`sessions::find_subagent_session`](../crates/moon-coder/src/sessions.rs) which scans parent subdirs (cheap — bounded by the project's session count). The pop-out card on a parent's transcript is the in-UI route to a sub-agent's transcript.

The header carries `parent_session_id` + `parent_tool_call_id` + `subagent_mode` so the UI's "pop out" affordance can resolve a transcript across IDE restarts; `subagent_target_folder` is populated only when the sub-agent's tools operated against a different folder than its parent (otherwise omitted from the on-disk header).

### UI

The frontend renders sub-agents as collapsed cards inline under the parent's `task` tool row: target-folder basename, mode badge (`research` quiet-neutral, `agent` accent-tinted), status pip, two-line result preview, token-cost footer. Click pops out into a dedicated sub-agent view (`coder.view = 'subagent'`) with a back-arrow to the parent's session. The pop-out reuses the parent transcript's row markup — same components, just a different rows source.

Persisted across reloads: the parent's JSONL gains a
`SubagentSpawned { tool_call_id, subagent_id, target_folder, mode }`
record at spawn time and a
`SubagentFinished { subagent_id, tokens_used_estimate, was_error, result_preview }`
record at finish time, so [`Coder::open_session`](../crates/moon-coder/src/runner.rs)
re-emits the matching `SubagentSpawned` / `SubagentFinished` events
on reload — the parent's collapsed cards rebuild without a special
case in the frontend reducer. For each `SubagentSpawned` the open-
path also reads the sub-agent's own JSONL under
`<sessions_dir>/<parent_session_id>/<subagent_id>.jsonl` and replays
its records as `SubagentEvent`s wrapping the same shape an
`open_session` would emit for a top-level session, so the popped-
out transcript shows the full sub-agent conversation, not just the
final preview. A missing sub-agent JSONL (manual deletion, partial
write, sessions written before this landed) logs a `tracing::warn!`
and falls through to the card-only restoration path.

### Bound folders system-prompt section

The parent's system prompt (rebuilt on every turn in [`runner::refresh_system_prompt`](../crates/moon-coder/src/runner.rs)) gains a "Bound folders" section listing every bound folder with the absolute path the model should use to address it and a 2–3 sentence description. The advertised path shape depends on the workspace's current shell mode: when the workspace shell container is `Running`, folders are listed under `/workspace/<name>` (the actual mount inside the container); otherwise (host mode) folders are listed by their real absolute host paths (`/home/eliheros/code/moon-ide`). The mode probe reuses [`ToolRegistry::bash_target_is_container`](../crates/moon-coder/src/tools.rs), so the bash target the section describes can't drift from how `bash` actually routes commands. Descriptions are generated by the `fast` model from each folder's manifest files in canonical order (`AGENTS.md`, `README.md`, `Cargo.toml`, `package.json`, `pyproject.toml`), cached at `<XDG_DATA_HOME>/moon-ide/folder-summaries/<slug>.json` keyed on a 64-bit FNV-1a of the inputs, and invalidated when any of those inputs change. AGENTS.md leads the bundle because it's literally written for agents — when both AGENTS.md and a README exist, the agent guidance anchors the prompt before the user-facing prose. Casing is matched case-insensitively against the folder's top-level entries, so `Readme.md` / `AGENTS.MD` / `Agents.md` all resolve without a hardcoded variant list.

Generation kicks off as a detached `tokio::spawn` from `runner::kick_off_summary_refresh` for any bound folder whose summary cache is missing or stale. The runner never blocks a turn waiting for one — if a summary isn't ready, the system-prompt builder emits `(summary still generating)` for that folder, and the next turn picks up whichever summaries finished in between. A `folder_summary_ready` event fires when one lands, so the project bar (follow-up plan) can refresh tooltips without polling.

The parent's path-taking tools accept any of the path shapes the resolver knows about (see below), so the parent can read or edit any bound folder directly regardless of which shape the model emits. Bound-folder summaries are still context (description text the model uses to decide _what_ to do), not access (the routing happens at the path layer regardless of whether the summary has loaded).

### Path resolution and cross-folder routing

[`tools::ToolRegistry::resolve_workspace_path`](../crates/moon-coder/src/tools.rs) classifies each path argument and returns the `(target_folder, relative_path)` pair the tool dispatches against:

1. **Absolute path under a bound folder's root**: routes to that folder. This is the everyday host-mode form — the system prompt's "Bound folders" section advertises each folder by its absolute host path, and the model joins file-relative paths onto it. When multiple bound folders' roots match (a strict-ancestor case the file tree allows), the longest match wins so an inner folder's relative addressing stays correct.
2. **Synthetic active or sibling** (`/workspace/<name>/...`): looks up the basename in the workspace registry and routes to that folder's [`WorkspaceHost`](architecture.md#workspacehost-phase-2). This is the container-mode form the prompt advertises while the workspace shell container is `Running`. An unbound `<name>` errors with `CoderError::ToolFailed` carrying the list of currently-bound folders so the model can self-correct without another guess turn. Kept available in host mode too (no behaviour gate by mode at the resolver), but the prompt only advertises it when the container is up.
3. **Bare-basename relative** (`<sibling-name>/foo.rs`): the same routing rule as the synthetic form. The model often produces this lower-friction form once it knows another folder's basename.
4. **Disambiguation opt-out** (`./<sibling-name>/foo.rs`): a directory inside the active folder might legitimately share a sibling's basename. The leading `./` skips the cross-folder routing; the path resolves against the active folder's host like any other relative path.
5. **Anything else** (relative paths, absolute paths outside every bound folder's root): resolved against the active folder; the host's `resolve` validates the bounds the way it always has — an absolute path outside every bound folder fails with a clear "escapes workspace root" error, which is what we want for paths like `/etc/passwd`.

Wiring the same routing into `read_file`, `list_dir`, `write_file`, and `edit_file` means the model can address any bound folder via whichever of those tools the task calls for. `grep` and `bash` aren't path-routed: `grep` always searches the active folder and `bash` runs commands in the active folder's working directory — both stay scoped to `cx.folder`, and a sub-agent against the target folder is the way to do searches or commands there.

Sub-agents share the parent's `ToolRegistry` so technically the same routing applies to them too; the sub-agent system prompts ask them to stay focused on the assigned folder. Sub-agents cannot dispatch `task` themselves — depth=1 is enforced by tool-list filtering at the parent's `run_turn`.

### Project-bar git status: surgical refresh

Cross-folder edits from the parent need to reach the project bar so the per-folder `+N ~N -N` badges stay accurate without the user having to activate the sibling. Two design points:

1. **The local fs-watcher only sees the active folder.** That covers in-active-folder edits and the user's own typing, but a parent agent that writes to `/workspace/<other>/...` flies under its radar.
2. **`git status` is cheap, but only one folder at a time.** Refreshing every bound folder on every tool turn is wasteful when most turns touch one folder.

The frontend's `bindCoderRefresh` listener parses every parent `tool_call` event's `args.path` with the same `/workspace/<name>` rule the backend uses (see [`WorkspaceState.resolveCoderEventTargetFolder`](../src/lib/state.svelte.ts)) and adds the resolved folder to a debounce-window pending set. On `tool_result` / `turn_complete` / `subagent_finished` it schedules a 200 ms flush that refreshes only the folders in the set. Anything ambiguous — `bash`, `grep`, sub-agent activity (the wrapper event doesn't carry the sub-agent's bound folder), or a missing `path` arg — flips a fan-out bit so the flush refreshes every bound folder. Correctness over cleverness; the worst case is the old behavior.

## UI placement

A right-side panel docked to the editor area. Chat and coder are
**mutually exclusive tenants** of a single right-side slot — the
two panels share one width and the user toggles between them; we
don't stack two narrow columns on top of each other. Which surface
is mounted (or `null` for closed) lives on `AppState.right_panel`
and is restored across launches via the dedicated
`ui_set_right_panel` Tauri command. Resizable horizontal splitter
(uses the same hand-rolled splitter the Slack panel currently
uses; will switch to `paneforge` when that lands repo-wide).

Toggleable from:

- Status-bar button (robot/wand icon, active when signed in and a
  session is open).
- Command palette: `Coder: Toggle Panel`.
- Keyboard: `F6` / `Shift+F6` cycle (the chat panel is in this
  rotation today; whichever surface is mounted in the right slot
  takes that focus stop).

Top-to-bottom layout:

```
┌──────────────────────────────────────┐
│ HF identity card | sign out          │  ← only when signed in
├──────────────────────────────────────┤
│ Sessions ▾  | + New session          │  ← session list (collapsible)
│ "implement bucket sync" · 2m         │
│ "rename moon-agent → moon-remote" · 1h│
├──────────────────────────────────────┤
│                                      │
│  [user] you said …                   │
│  [assistant] thinking… (collapsed)   │
│  [assistant] streamed reply …        │
│  [tool] read_file src/lib/state…     │
│         (expandable: input | output) │
│  [assistant] continues …             │
│                                      │
├──────────────────────────────────────┤
│ Model: large ▾  | sync: ●            │  ← session controls
├──────────────────────────────────────┤
│ ┌──────────────────────────────────┐ │
│ │ Type a message — Enter to send   │ │
│ └──────────────────────────────────┘ │
└──────────────────────────────────────┘
```

States the panel renders at the top:

- **Not signed in**: empty state with a "Sign in with Hugging
  Face" primary button.
- **Signed in, no session**: identity card + session list + a
  big "+ New session" button if the list is empty.
- **Active session**: full layout above.

## Frontend ↔ backend boundary

Tauri commands in `src-tauri/src/commands/coder.rs`:

| Command                                                    | Purpose                                                                                                                                                                                                                                                  |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `coder_start_device_flow()`                                | Returns `{ user_code, verification_uri, expires_in, interval }`. Background poll runs in `moon-coder`                                                                                                                                                    |
| `coder_status()`                                           | `{ signed_in, identity?, has_session, sync_enabled }`                                                                                                                                                                                                    |
| `coder_sign_out()`                                         | Drops keyring + identity                                                                                                                                                                                                                                 |
| `coder_list_sessions()`                                    | List of `{ id, first_line, latest_event_ts }`                                                                                                                                                                                                            |
| `coder_open_session(id?)`                                  | If `id` given, load it; else create a new one. Returns the new active id                                                                                                                                                                                 |
| `coder_delete_session(id)`                                 | Removes JSONL + tombstones for sync                                                                                                                                                                                                                      |
| `coder_session_jsonl_path(id)`                             | Resolves a session id (parent or sub-agent) to its absolute on-disk JSONL path; powers the panel's `</>` "open trace" affordance, which then opens the file via the host-direct file mechanism (see [test plan 0051](test-plans/0051-open-host-file.md)) |
| `coder_send(text, mode: "send" \| "steer" \| "follow_up")` | Routes to the loop                                                                                                                                                                                                                                       |
| `coder_abort()`                                            | Cancels the in-flight loop                                                                                                                                                                                                                               |
| `coder_set_model(slug)`                                    | Override on the active session                                                                                                                                                                                                                           |
| `coder_set_sync_enabled(enabled)`                          | Per-workspace bucket-sync toggle                                                                                                                                                                                                                         |

Push events from backend → frontend (Tauri event channel):

- `coder:event` — every loop event (`agent_start`, `turn_start`,
  `message_start/update/end`, `tool_execution_*`, `turn_end`,
  `agent_end`, `error`). Single channel; the UI dispatches by
  `event.type`.
- `coder:signed_out` — token went bad / explicit sign-out.
- `coder:sync_state` — `{ session_id, status: "uploading" | "ok" |
"delayed", error? }` for the status-bar pip.

## Failure modes

| Scenario                                     | UI behaviour                                                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| User has no HF token                         | "Sign in with Hugging Face" empty state                                                          |
| Device-flow polling expires                  | Modal shows "code expired, retry"                                                                |
| Token rejected by `/whoami-v2` after refresh | Toast + return to empty state; keyring cleared                                                   |
| `router.huggingface.co` 5xx                  | Streaming surfaces an error; "Retry" button performs a `continue()` against the existing context |
| Tool throws                                  | LLM gets `isError: true` + the message; loop continues                                           |
| Bucket creation 4xx (e.g. quota)             | `coder:sync_state delayed` pip; sessions stay local; banner offers "show details"                |
| `hf-xet` upload partial-fail                 | Retry on next quiescence; if still failing after 3 attempts, mark `delayed`                      |
| Network down                                 | Streaming aborts; same retry surface as 5xx; sync stays in `delayed` until network returns       |

## Frontend module layout

New files (all per [`frontend.md`](frontend.md) conventions):

```
src/lib/
├── components/
│   ├── CoderPanel.svelte
│   ├── CoderConnectModal.svelte    // device-flow user code display
│   ├── CoderSessionList.svelte
│   ├── CoderMessage.svelte
│   ├── CoderToolCall.svelte
│   └── CoderComposer.svelte
├── coder.svelte.ts                 // panel state ($state runes)
└── util/coderStream.ts             // event-channel parser
```

The panel reuses the Slack panel's sticky-bottom scroll and
auto-grow textarea verbatim — that's a five-line lift each time;
no shared component. ADR 0003 ("no adapter layer") still applies.

## Out of scope (explicitly)

- **Pluggable agent binaries** (ACP) — superseded by ADR 0010.
- **Plan mode** — the team can write plans into `AGENTS.md`.
  Reconsider when somebody asks.
- **Permission popups** — see "Permissions" above.
- **MCP** — same posture as pi.
- **Per-sub-agent abort UI** — parent abort cascades to all live sub-agents via child `CancellationToken`s; individual cancel buttons are deferred until a real workload calls for them.
- **Background detached sub-agents** (Cursor / Devin-style "agents that keep working across IDE restarts") — sub-agents are synchronous-blocking; the parent's tool call awaits their report.
- **Depth ≥ 2 sub-sub-agents** — hardcoded depth=1 cap. The sub-agent's tool list omits `task`, so the model literally cannot describe a sub-sub-agent.
- **Skill packages / installable skills** — file conventions only.
- **Custom providers** — schema lives in `AppState.coder.providers`
  but no UI / wiring in the initial sub-phases.
- **Bucket browser** — "import session from bucket" UI for picking
  a chat up on a different machine. Bucket is backup-only at first.
- **Multi-account** — one HF account per moon-ide install.
- **Background agent runs** — the loop only runs while the panel
  is active. A "run this overnight" mode is a Phase 12 problem.

## Cross-spec touch-points

- [`architecture.md`](architecture.md) — the agent loop lives in
  moon-core; the UI never touches LLMs directly. Cross-cutting
  invariant updated.
- [`protocol.md`](protocol.md) — `coder.*` methods replace the
  sketch of `acp.*`.
- [`AGENTS.md`](../AGENTS.md) — invariant 1 updated to mention
  coder instead of ACP.
- [`slack-chat.md`](slack-chat.md) — the "real agent in the IDE
  with context" pointer in the lead paragraph re-points at this
  spec.
- [`containers.md`](containers.md) — the "what runs on the host
  vs the container" table flips the ACP row to coder. The agent
  itself is a moon-core component, so it lives in the Tauri shell;
  its bash / fs tools cross into the container exactly the way
  the terminal does.
