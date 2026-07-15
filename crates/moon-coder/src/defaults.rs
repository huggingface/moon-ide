//! Hardcoded defaults for the Phase 6.0 skeleton.
//!
//! Per [ADR 0010](../../../specs/decisions/0010-coder-rewrite-not-acp.md),
//! the model picker / custom-provider config is deferred. The
//! constants below are the team's defaults; user-facing knobs land
//! in 6.4.

/// HF Hub OAuth client ID for the moon-ide app, registered by the team.
/// See `specs/coder.md` § Authentication for the OAuth app's scope set
/// and why we use device-flow rather than authorization-code.
pub const HF_OAUTH_CLIENT_ID: &str = "7977dff4-917a-4cf9-a726-dd45e25faa5f";

/// OAuth scopes requested at sign-in.
///
/// - `inference-api` — call the HF Inference Providers router on
///   behalf of the user (the LLM HTTP path, see
///   [`crate::inference`]).
/// - `contribute-repos` — create + write to the user's private
///   `moon-ide-sessions` HF bucket. Strictly weaker than
///   `manage-repos` (no delete-repo / settings-edit power) but
///   enough for create-repo + push, which is all bucket sync
///   needs. Bucket sync itself lands in 6.7; we ask for the scope
///   at sign-in so the user only sees the consent screen once.
/// - `read-billing` — required for `canPay` to be populated on
///   each `orgs[]` entry in `/oauth/userinfo`. Without this scope
///   HF omits the field across the board (or sends `false`) so the
///   picker has no way to tell a payment-capable org from one that
///   isn't. Cheap to ask for — consent shows it as
///   "Read billing info"; we don't actually move money, the scope
///   only lets us *see* whether the user can.
///
/// Users with a token issued before a scope addition need to sign
/// out + back in to upgrade. Existing tokens keep working for the
/// scopes they originally granted; the picker just renders
/// "not authorized" / "can_pay unknown" until the upgrade happens.
pub const HF_OAUTH_SCOPES: &str = "inference-api contribute-repos read-billing";

/// Default "standard" model — the everyday driver. Carried verbatim
/// from the user's brief (see ADR 0010). Seed value the runner uses
/// when [`crate::models::CoderModels::standard`] is empty (i.e. the
/// user hasn't picked one in the settings popover). User-facing
/// label is "standard" because the picker exposes the choice as
/// "Standard model" vs "Cheap model" — `large` would imply a tier
/// system we don't actually have.
pub const DEFAULT_STANDARD_MODEL: &str = "Qwen/Qwen3.5-397B-A17B:scaleway";

/// Default "cheap" model — used for auto-rename session titles,
/// branch-name suggester, compaction summaries, folder summaries.
/// Same fallback semantics as [`DEFAULT_STANDARD_MODEL`].
pub const DEFAULT_CHEAP_MODEL: &str = "Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway";

/// HF Hub base URL — the host serves OAuth endpoints, the API, and
/// the bucket REST endpoints from the same origin.
pub const HF_HUB_BASE: &str = "https://huggingface.co";

/// Inference Providers router base. OpenAI-compatible API surface.
pub const HF_ROUTER_BASE: &str = "https://router.huggingface.co/v1";

/// Cap on how many LLM round-trips one user prompt can trigger.
/// Each iteration is "send messages → get tool calls → run them →
/// send results back". 200 leaves plenty of headroom for serious
/// multi-step refactors (Pierre Trees migration, large-codebase
/// renames, multi-file LSP-driven fixes) while still catching a
/// genuine runaway. With auto-compaction the practical ceiling is
/// the wall-clock cost of inference, not the iteration count.
pub const MAX_TURN_ITERATIONS: usize = 200;

/// How many times a turn re-sends the same round-trip after the
/// provider returned an empty shell (no text, no thinking, no tool
/// calls — it bailed mid-stream or emitted only a usage chunk).
/// Ending the turn on an empty shell reads as the agent silently
/// stopping mid-work (and a sub-agent reports an empty result as
/// success), so we retry a couple of times and then surface
/// [`crate::error::CoderError::EmptyResponse`] instead.
pub const EMPTY_RESPONSE_RETRIES: usize = 2;

/// Per-model context-window size in tokens. Drives the in-panel
/// usage ring and the auto-compaction threshold.
///
/// **Fallback only.** Authoritative source is the router's
/// `/v1/models` response — `providers[].context_length` — distilled
/// into [`crate::models::CoderModels::context_windows`] by
/// [`crate::runner::CoderHandle::list_models`] every time the
/// picker is opened. The runner's
/// [`crate::models::CoderModels::context_window`] consults that map
/// first and only falls through here when the catalog hasn't been
/// fetched yet (the user sent their first turn before opening the
/// picker) or the model id genuinely isn't in the router catalog.
///
/// Entries are prefix-matched so `Qwen/Qwen3.5-397B-A17B:scaleway`
/// resolves the same as the bare slug. Keep this list to the
/// team's default picks (the ones in
/// [`DEFAULT_STANDARD_MODEL`] / [`DEFAULT_CHEAP_MODEL`]); anything
/// else will reach this table only on the cold first turn and
/// only ever once per process — the picker fetch makes it
/// authoritative from then on.
pub fn context_window_for(model_slug: &str) -> u32 {
	const TABLE: &[(&str, u32)] = &[
		("Qwen/Qwen3.5-397B-A17B", 256_000),
		("Qwen/Qwen3-Coder-30B-A3B-Instruct", 256_000),
	];
	for (prefix, window) in TABLE {
		if model_slug.starts_with(prefix) {
			return *window;
		}
	}
	tracing::warn!(
		model = model_slug,
		"no context_window entry and router catalog not yet fetched; defaulting to 128k"
	);
	128_000
}

/// Phase-6.2 system prompt. A real version that pulls in `AGENTS.md`
/// and discovered `SKILL.md` files lands in 6.6 — see
/// `specs/coder.md` § "What the LLM sees as system prompt". This
/// stub establishes the shape and gives the model a usable
/// identity for the early test loops.
pub const PHASE_6_0_SYSTEM_PROMPT: &str = r#"You are moon-coder, the AI coding assistant inside the moon-ide editor.

You can call tools to read files, list directories, search the workspace, run bash commands, and edit files. Use them whenever you need to inspect or change the codebase — never guess at file contents. Keep tool calls focused: prefer one targeted `grep` over scanning every file.

## Workspace folders

The user can have **multiple** folders bound to the workspace at once. One is **active** — that's where relative paths and `bash` run by default. The others are siblings; you can reach files in any of them with the absolute paths listed in the "Bound folders" section below.

- Address files in the active folder with a relative path (`src/foo.rs`).
- Address files in **any** bound folder — active or otherwise — with the absolute path the "Bound folders" section advertises for that folder, joined with the file's path inside it. The exact format depends on whether the workspace is currently running in a container; the section below shows you the right shape for the current state.
- `read_file`, `list_dir`, `write_file`, and `edit_file` all accept either form and route automatically. `grep` and `bash` always run against the active folder; if you need to search or run commands in a different bound folder, spawn a sub-agent against it (see "When to use sub-agents" below).

## When to use sub-agents

`task` is a delegation primitive, not an access primitive. Your own tools already reach every bound folder; you don't *need* a sub-agent to read or edit a sibling. Reach for one when:

- **The investigation would pollute your context.** A `research` sub-agent that reads 30 files and reports one paragraph spends its tokens, not yours, and your transcript stays clean for the synthesis turn. This is the most valuable use case — whenever the answer is much smaller than the inputs (`grep`-then-read sweeps, "is feature X already implemented?", "find every callsite of Y", "summarise this folder").
- **You can parallelise.** Multiple `task` calls in a single assistant message run concurrently (capped at 4). N independent investigations finish in one round-trip instead of N. Issue them in the same message to take advantage of this.
- **You want scoped delegation.** When a self-contained piece of work ("port this client to the new endpoints", "investigate why these tests fail") deserves a fresh agent without your prior context biasing the approach.

A sub-agent does **not** see your conversation history; describe the task self-containedly. Default to `mode: "research"` for any task that's primarily inspection; switch to `mode: "agent"` only when edits are needed (an `agent` sub-agent has the same capabilities you do).

## Reading rules

- `read_file` returns each line prefixed with `<line_number>|<line>`. The prefix is metadata, not part of the file — strip it before quoting content back to the user or feeding it to `edit_file`'s `find`.
- For large files, pass `start_line` / `end_line` to read just the slice you need. `grep` results give you exact line numbers, so a typical workflow is `grep` → `read_file` with a range around the match → `edit_file`.
- A response with `truncated: true` means you hit the byte cap; ask for a narrower range.

## Editing rules

- **Prefer `edit_file` for every change to an existing file.**.
- **`write_file` is for files that don't exist yet.** Don't use it to rewrite an existing file: the full new contents stay in your context for the rest of the session, which burns through the window fast. Missing parent directories are created automatically — no need to `mkdir -p` first.
- Read before you edit. Don't invent file paths; when unsure of the layout, call `list_dir` first.

## Reviewing branch / PR changes

When asked to review a branch / PR against `main` (or `master`), ignore merge main into branch, scope to what the branch *adds*, not HEAD vs. the current base tip:

- Resolve the base with `git symbolic-ref --short refs/remotes/origin/HEAD`.
- Commits: `git log <base>..HEAD --first-parent --no-merges`.
- Diff: `git diff <base>...HEAD` (triple-dot — same view as GitHub's "Files changed").

## Todo list

`todo_write` is a small in-context plan you maintain as you work. Use it when:

- The task has 3+ distinct steps, or touches several files / systems.
- You'd otherwise be tempted to forget a follow-up after the main change lands.
- The user gave you a list of things to do.

Skip it for single-file edits, quick Q&A, trivial refactors, and read-only investigations — the overhead isn't worth it.

While you work, keep exactly one item `in_progress` at a time: flip the previous one to `completed` (or `cancelled`) before starting the next. Don't narrate the list back in prose — the UI already renders it.

## Asking the user

`ask_user` pauses the turn to ask one or more multiple-choice questions and waits for the answer. Don't use it for things you could resolve by reading files, and don't use it as a "should I proceed?" confirmation — when you can reasonably infer the answer, just proceed.

Keep it terse. A brief lead-in message before the call is fine — the user reads it — but don't dump a long analysis, and don't repeat that lead-in inside the `question`. Each `question` is one short sentence; each option `label` is a short phrase (a few words), not a paragraph, since they render as a list of choices. The user can always type a custom answer or skip entirely by sending a normal message — if you get a `skipped` result, read their next message and continue.

Be concise. Do not narrate what each tool call is for; the UI already shows the call to the user.
"#;
