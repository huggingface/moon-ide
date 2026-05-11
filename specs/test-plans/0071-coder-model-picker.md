# Test plan 0071: Coder model picker

- **Date**: 2026-05-07
- **Phase**: Phase 6.x — coder follow-up

## What shipped

- `Standard model` + `Cheap model` picks for the coder, persisted in
  `AppState.coder` and live-applied to the next round-trip (no restart).
  Both default to the previous hardcoded constants when empty.
- Wire model ids are sent verbatim to `router.huggingface.co/v1/chat/completions`
  including any `:provider` suffix; runner reads via `CoderModels` snapshot at
  the top of each turn / sub-agent / cheap-helper call.
- Settings popover behind a new cog icon in the coder header. Search +
  filter against `https://router.huggingface.co/v1/models`, click-to-apply
  with optional auto-suffix from a `Default provider` hint field.
- `X-HF-Bill-To` header support: a "Bill to" dropdown lists the user's orgs
  (sourced from `/oauth/userinfo`, replaces the previously-considered
  `/api/whoami-v2` path); "Personal account" is the always-present default.
- Internal rename across the codebase: `DEFAULT_LARGE_MODEL` /
  `DEFAULT_FAST_MODEL` → `DEFAULT_STANDARD_MODEL` / `DEFAULT_CHEAP_MODEL`,
  matching the user-facing labels. No migration; AppState fields just gain
  new defaults.

## How to test

Prerequisites: `bun install`, signed in to Hugging Face (the picker needs
`/oauth/userinfo` and `/v1/models` access). Build with `cargo build --release`
or run `bun run dev`.

1. Open the coder panel. Sign in if not already. Click the new **cog**
   icon in the header. Expected: popover opens with four fields
   (Standard model, Cheap model, Default provider, Bill to), an empty-state
   "Loading models from `router.huggingface.co`…" hint, then a long list of
   models within ~1s.
2. In the search box at the top of the catalog, type `qwen3`. Expected:
   list narrows to Qwen3 variants. The list defaults to filtering to the
   Standard tier — every visible row supports tool calls (no orange
   `no tools` chip).
3. Click the **EDIT** chip next to "Cheap model" so the catalog now shows
   the unfiltered list. Expected: search returns Qwen3 _plus_ tool-less
   variants (e.g. some `gpt-oss-20b` provider rows).
4. Type `scaleway` into the "Default provider" field. Click any Qwen3 row
   in the catalog. Expected: the Standard model (or Cheap, depending on
   which tier is active) field gets populated with `<model>:scaleway`
   — but only when the model genuinely has a scaleway route. Otherwise
   it's the bare id (router falls back to its own selection policy).
5. Click **Save**. Expected: popover closes; no visible error.
6. Send a prompt in the coder ("hello, write a one-line answer"). Expected:
   the turn runs against the model you just picked. If you watch the
   network you should see the `model` field of the JSON body match
   what's in `Standard model`.
7. Reopen the cog popover. Expected: the saved settings re-populate
   the fields (i.e. they actually persisted, not just held in memory).
8. Pick a "Bill to" org (only orgs you're a member of show up). Save.
   Send another prompt. Expected: the request carries an
   `X-HF-Bill-To: <org>` header (verify via `tcpdump` /
   `mitmproxy` on the host, or by router-side billing showing up under
   the org). If the org can't actually bill, the request fails and the
   error surfaces in the panel verbatim.
9. Clear the Standard model field, save, send a prompt. Expected: the
   request goes to `Qwen/Qwen3.5-397B-A17B:scaleway`
   (the built-in default).
10. Bring up the cog popover with no internet (or revoke your token).
    Expected: the catalog area shows an error + Retry button; the
    save-form fields still work — you can still type a model id and save.

## What must keep working

Regression checks. If any of these break, the commit needs a follow-up.

- Send / abort / streaming still works with the default model picks
  (nobody has touched the settings yet).
- Auto-rename of fresh sessions still happens after the first turn,
  using the cheap model.
- Branch-name suggester (SCM panel "Commit to new branch…" form) still
  works.
- Compaction kicks in when the prompt crosses the threshold; the
  summary is generated against the **cheap** model.
- Folder summaries (the "Bound folders" panel-tooltip text) are still
  generated on the cheap model.
- Sub-agents inherit the parent's standard model — they don't randomly
  drop down to the cheap model.
- Signing out + back in still surfaces the user identity in the panel
  header; the new `orgs` field doesn't break the userinfo decode for
  users with no orgs (the field defaults to `[]`).

## Known limitations

Things we deliberately did not do, with one-line justification.

- No per-folder / per-session model override. The settings are global per
  signed-in user; agents in different folders share the same picks. Hardcode
  first; add per-scope overrides only if a real workflow needs it.
- No model deprecation handling — if a saved slug stops appearing in
  the router's `/v1/models` response, the runner still tries to send
  it; the router returns an error and we surface it. No silent fallback.
- The "Default provider" field accepts any string; we don't validate
  against the router's known set. Typos route through `:typo` and the
  router returns an error.
- Bill-to validation is server-side: we don't check `can_pay` before
  sending; the picker greys out the entry but a determined user could
  still type a bare org name. The router rejects, panel shows the error.

## Related

- Specs: `specs/coder.md` (model selection section), `specs/protocol.md`
  (`CoderModelSettings` / `RouterModel`).
- ADRs: ADR 0010 (coder rewrite — established the model defaults this
  picker now makes configurable).
- Prior test plans: 0039 (coder skeleton), 0042 (coder streaming),
  0054 (token usage + auto-compaction), 0057 (cross-folder sub-agent
  nudge), 0058 (cross-folder routing + drop tiers).
