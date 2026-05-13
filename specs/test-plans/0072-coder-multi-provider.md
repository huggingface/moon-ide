# Test plan 0072: Coder multi-provider (OpenRouter / local LLM)

- **Date**: 2026-05-13
- **Phase**: Phase 6.x — coder follow-up

## What shipped

- Extended the coder model picker to support **user-added
  OpenAI-compatible providers** alongside the implicit Hugging Face
  default. The intended audience is users who want to route through
  OpenRouter (with their own API key) or hit a locally-hosted
  llama.cpp / vLLM / Ollama / LiteLLM without going through HF.
- `AppState.coder` gained `providers: Vec<CoderProviderConfig>` +
  `active_provider: Option<String>`. HF stays implicit (no entry,
  `None` active). Each provider carries its own
  `(standard_model, cheap_model)` — slugs aren't portable between
  hosts, so switching providers swaps which picks the runner uses.
- API keys live in the OS keyring under
  `service=moon-ide, account=coder-provider:<id>` — same pattern as
  the Tavily web-search key. The `CoderModelSettings` payload carries
  `has_api_key: bool` for the UI but never the key itself.
- The `InferenceClient` resolves `(base_url, auth, bill_to)` fresh
  off `CoderModels` for every request. HF path keeps the OAuth +
  refresh-on-401 behaviour; user providers get a plain
  `Bearer <api_key>` header (or no auth header for keyless local
  servers) and a 401 surfaces verbatim to the user — there's no
  refresh token to retry with.
- `X-HF-Bill-To` is now strictly HF-only: suppressed off the wire
  when a user provider is active. The "Bill to" field in the picker
  hides when the active route isn't HF.
- New Tauri commands wired through `ipc.coder.*`:
  `newProviderId`, `probeProvider`, `saveProvider`, `deleteProvider`,
  `setProviderApiKey`, `clearProviderApiKey`, `listProviderModels`.
  The existing `coder_list_models` stays HF-only and errors when a
  user provider is active (the picker dispatches per-route).
- `coder_status.signed_in` is now route-aware. HF needs OAuth; user
  providers need a keyring entry **or** a `localhost` / `*.local`
  base URL (mirrors of the runner's `is_local_base_url` heuristic on
  both ends — the picker hides the "no key" badge for local URLs).
- Picker UI: a provider switcher above the model fields with one
  tab per provider plus `+ Add provider`. The `Add / Edit` sub-form
  takes `(label, base_url, api_key?)`, has a **Verify** button that
  pings `/v1/models` (falling back to a 1-token completion on 404),
  and saves the provider atomically — so cancelling the outer modal
  doesn't lose a provider the user already saved. While the sub-form
  is open, the model picks + catalog underneath collapse out of the
  way (they belong to the about-to-be-replaced route and would
  otherwise compete for attention).
- The non-HF flat catalog parses the **richer fields** that
  OpenRouter / LiteLLM / vLLM emit alongside the minimal
  OpenAI-compat shape: human `name`, `context_length` (also accepts
  vLLM's `max_model_len`), and `pricing` (per-token strings from
  OpenRouter, per-million floats from LiteLLM — normalised to
  `$/M tokens` at the parse boundary). Each catalog row renders the
  long name, a context chip, and an in/out price chip when the
  server provides them; minimal Ollama responses gracefully
  degrade to just `id` + `owned_by`.

## How to test

Prerequisites: `bun install`, signed in to Hugging Face (HF stays
the default — users who don't sign in can't even pick a provider
until they set one). Build with `cargo build --release` or
`bun run dev`.

### HF stays the default and still works

1. Open the coder panel. Click the cog icon. Expected: the
   provider switcher shows a single **Hugging Face** tab (active)
   and a `+ Add provider` button. Bill-to field, HF rich catalog,
   tier-tab editor — all still present as before.
2. Send a prompt against the default pick. Expected: turn runs
   normally. Verify in network capture that `Authorization` carries
   the HF OAuth bearer and `X-HF-Bill-To` is set when configured.

### Add an OpenRouter provider with API key

3. Click the cog icon. Click `+ Add provider`. Expected: the model
   fields + HF catalog underneath collapse so only the add-provider
   form is visible. Fill in:
   - Label: `OpenRouter`
   - Base URL: `https://openrouter.ai/api/v1`
   - API key: paste a valid OpenRouter key.
     Click **Verify**. Expected: green message
     "OK — N models reachable" with a couple of sample ids.
4. Click **Save provider**. Expected: the sub-form closes, the
   model fields + catalog re-appear, and the switcher now shows
   three tabs: `Hugging Face`, `OpenRouter`, `+ Add provider`.
5. Click the `OpenRouter` tab. Expected:
   - The Bill-to field disappears.
   - The catalog header now reads "Filter by model id or owner…".
   - The catalog populates with rows showing the model id, the long
     human name (e.g. "Anthropic: Claude 3.5 Sonnet"), the context
     window (`200k ctx`), and the in/out price (`$3/$15 per M`) —
     OpenRouter publishes all of those at `/v1/models` and the
     picker lights them up automatically.
   - The standard / cheap model fields go blank (the OpenRouter
     entry has no picks yet).
6. Click a row in the catalog (e.g. `anthropic/claude-3.5-sonnet`).
   Expected: the standard-model field is set to that id verbatim
   (no `:provider` suffix — non-HF routes don't multiplex).
7. Click **Save** on the outer modal. Send a prompt. Expected:
   the request goes to `https://openrouter.ai/api/v1/chat/completions`
   with `Authorization: Bearer <openrouter-key>`, no
   `X-HF-Bill-To` header. The model and completion are
   OpenRouter-served.

### Switching providers preserves per-route picks

8. Reopen the cog. Switch back to **Hugging Face**. Expected:
   the standard / cheap fields restore the HF picks you saw before.
   The Bill-to dropdown reappears.
9. Switch back to **OpenRouter**. Expected: the fields show
   `anthropic/claude-3.5-sonnet` (whatever you picked).

### Local llama.cpp / Ollama (keyless)

10. Cog → `+ Add provider`. Fill in:
    - Label: `Local llama`
    - Base URL: `http://localhost:11434/v1` (or wherever your local
      OpenAI-compat server runs; this works for Ollama out of the
      box on its default port — note that as of Ollama 0.x the
      endpoint expects no API key).
    - Leave API key empty.
      Click **Verify**. Expected: if the local server is running,
      green "OK — N models reachable" (Ollama exposes `/v1/models`);
      if not, a transport error. Save anyway.
11. Switch to `Local llama` in the switcher. Expected: the tab does
    **not** show a `no key` badge (local URLs are keyless-by-default).
    Pick a model. Send a prompt. Expected: it routes to localhost.

### Verify rejection on bad key

12. Cog → `+ Add provider`. Label: `Bad key`, Base URL:
    `https://openrouter.ai/api/v1`, API key: `sk-nonsense`. Click
    Verify. Expected: red error showing the upstream 401 verbatim.
    You can still Save anyway (we don't gate on Verify).

### `has_api_key` flag survives a relaunch

13. With at least one user-provider configured (with key), quit the
    app and relaunch. Open the cog. Expected: the provider switcher
    shows the same tabs; the configured provider does **not** show
    a `no key` badge (the runner re-warms the keyring cache at
    startup). The `state.json` on disk does **not** contain the
    actual key — only the metadata.

### Delete a provider

14. Cog → switch to a user provider → click **Edit**. Click
    **Delete provider**. Expected: confirmation-less delete
    (idempotent; if the user actively misclicks they can re-Add).
    The switcher tab disappears; if this was the active route, the
    runner falls back to HF and the cog re-renders with HF picks.

## What must keep working

Regression checks. If any of these break, the commit needs a follow-up.

- HF-only users (no provider configured) see no UI change beyond
  the new provider switcher row. Send / abort / streaming /
  sub-agents / auto-rename / compaction / branch-suggestion all
  unchanged.
- The runner's 401-refresh path on HF still kicks in when the
  access token expires mid-session. Confirm by hand-clearing the
  keyring slot for HF and sending a prompt — first 401 triggers a
  refresh, second 401 (when refresh also fails) surfaces "not
  signed in" to the panel.
- `coder_status.signed_in` returns true the moment a provider is
  set up (key configured, or local URL). Verify by toggling between
  providers — the composer disables / enables in lock-step.
- Switching the active provider mid-session (without restarting a
  turn) takes effect on the **next** request, not the current one.
  An in-flight turn against HF stays on HF until it ends.
- The model-settings popover Save is atomic: a partial failure
  (network issue inside a `set_provider_api_key`) leaves the
  modal open with an inline error and the previous picks intact.
- Old `state.json` files without `providers` / `active_provider`
  fields still parse (existing serde `#[serde(default)]` plus the
  `app_state_tolerates_obsolete_fields` regression test in
  `crates/moon-protocol/src/app_state.rs`).

## Known limitations

Things we deliberately did not do, with one-line justification.

- **No per-folder active provider.** The active provider is global per
  signed-in user. Hardcode first; per-folder overrides land if a real
  workflow needs it.
- **No model-deprecation handling for non-HF endpoints.** OpenRouter
  occasionally retires a model id; we surface the upstream error
  verbatim and the user reopens the picker. No silent fallback.
- **No throughput / TTFT chips for non-HF routes.** The flat
  catalog renders context + pricing (when the server provides them,
  which OpenRouter and most LiteLLM deployments do) but skips
  throughput / time-to-first-token. Those are HF-router-specific
  measurements and don't have a uniform equivalent across
  OpenAI-compat servers.
- **No OAuth for Anthropic / OpenAI / Google.** Those remain
  parked. API-key auth covers the immediate need; if subscription
  billing matters later, see "Later: Anthropic OAuth" in
  `specs/coder.md`.
- **No multi-account-per-provider.** One key per provider id; users
  who need two OpenRouter accounts add two providers with different
  labels and base URLs (the dedup is on `id`, not `base_url`).
- **No probe-on-save gate.** The picker can save a provider that
  failed Verify — local servers might not be running at save time,
  and forcing a successful probe before save would block legit
  flows.

## Related

- Specs: `specs/coder.md` ("Custom OpenAI-compatible endpoints" —
  the previous "Later:" section is now what landed),
  `specs/protocol.md` (`CoderProviderConfig`,
  `ProviderProbeResult`, `ProviderModelSummary`).
- Prior test plans: 0071 (coder model picker — the HF-only baseline
  this extends).
