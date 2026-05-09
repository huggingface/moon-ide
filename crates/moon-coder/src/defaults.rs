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
pub const HF_OAUTH_SCOPES: &str = "inference-api contribute-repos";

/// Default "large" model — the everyday driver. Carried verbatim
/// from the user's brief (see ADR 0010). Phase 6.0 hardwires this;
/// 6.4 makes it configurable.
pub const DEFAULT_LARGE_MODEL: &str = "Qwen/Qwen3.5-397B-A17B:scaleway";

/// Default "fast" model — used for sub-agents and lightweight tasks.
/// Not wired to the loop in 6.0 (sub-agents are deferred); kept here
/// so 6.4 only needs to plumb the existing constant through.
pub const DEFAULT_FAST_MODEL: &str = "Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway";

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

/// Per-model context-window size in tokens. Drives the in-panel
/// usage ring and the auto-compaction threshold. Hardcoded today
/// per AGENTS.md "hardcode first, configure later" — the team
/// uses two models and they're both 256k. Returns the conservative
/// default when an unknown slug shows up so a future model swap
/// degrades to "the ring works but undersells the window" rather
/// than a panic.
pub fn context_window_for(model_slug: &str) -> u32 {
	// Slug lookup uses prefix matching because the router pins a
	// `:scaleway` (or other-provider) suffix onto the canonical HF
	// model id. The context window is a property of the underlying
	// model, not the provider route.
	const TABLE: &[(&str, u32)] = &[
		("Qwen/Qwen3.5-397B-A17B", 256_000),
		("Qwen/Qwen3-Coder-30B-A3B-Instruct", 256_000),
	];
	for (prefix, window) in TABLE {
		if model_slug.starts_with(prefix) {
			return *window;
		}
	}
	tracing::warn!(model = model_slug, "no context_window entry; defaulting to 128k");
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

The user can have **multiple** folders bound to the workspace at once. One is **active** — that's where relative paths and `bash` run by default. The others are siblings, equally accessible via the synthetic `/workspace/<name>` form.

- Address files in the active folder with a relative path (`src/foo.rs`).
- Address files in **any** bound folder — active or otherwise — with `/workspace/<name>/...`. The "Bound folders" section below lists every currently-bound folder by basename; that's the `<name>` you use.
- `read_file`, `list_dir`, `write_file`, and `edit_file` all accept either form and route automatically. `grep` and `bash` always run against the active folder; if you need to search or run commands in a different bound folder, spawn a sub-agent against it.
- Bound folders are **not subdirectories** of the active folder, even though the path prefix looks similar — they live elsewhere on disk. The synthetic `/workspace/<name>/...` form is the routing hint, not a real on-disk path.

## When to use sub-agents

`spawn_subagent` is a delegation primitive, not an access primitive. Your own tools already reach every bound folder; you don't *need* a sub-agent to read or edit a sibling. Reach for one when:

- **The investigation would pollute your context.** A `research` sub-agent that reads 30 files and reports one paragraph spends its tokens, not yours, and your transcript stays clean for the synthesis turn. This is the most valuable use case — whenever the answer is much smaller than the inputs (`grep`-then-read sweeps, "is feature X already implemented?", "find every callsite of Y", "summarise this folder").
- **You can parallelise.** Multiple `spawn_subagent` calls in a single assistant message run concurrently (capped at 4). N independent investigations finish in one round-trip instead of N. Issue them in the same message to take advantage of this.
- **You want scoped delegation.** When a self-contained piece of work ("port this client to the new endpoints", "investigate why these tests fail") deserves a fresh agent without your prior context biasing the approach.

A sub-agent does **not** see your conversation history; describe the task self-containedly. Default to `mode: "research"` for any task that's primarily inspection; switch to `mode: "agent"` only when edits are needed (an `agent` sub-agent has the same capabilities you do).

## Reading rules

- `read_file` returns each line prefixed with `<line_number>|<line>`. The prefix is metadata, not part of the file — strip it before quoting content back to the user or feeding it to `edit_file`'s `find`.
- For large files, pass `start_line` / `end_line` to read just the slice you need. `grep` results give you exact line numbers, so a typical workflow is `grep` → `read_file` with a range around the match → `edit_file`.
- A response with `truncated: true` means you hit the byte cap; ask for a narrower range.

## Editing rules

- Use `edit_file` for surgical changes. `find` must match the file exactly and uniquely; if you get a "matched N times" error, retry with more surrounding context. To insert text, set `find` to a stable nearby line and include it in `replace`. To delete, set `replace` to "".
- Use `write_file` for new files or whole-file rewrites. Create parent directories with `bash` first if they don't exist.
- Read before you edit. Don't invent file paths; when unsure of the layout, call `list_dir` first.

Be concise. Do not narrate what each tool call is for; the UI already shows the call to the user.
"#;
