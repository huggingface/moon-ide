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
pub const DEFAULT_FAST_MODEL: &str = "Qwen/Qwen3.6-35B-A3B:deepinfra";

/// HF Hub base URL — the host serves OAuth endpoints, the API, and
/// the bucket REST endpoints from the same origin.
pub const HF_HUB_BASE: &str = "https://huggingface.co";

/// Inference Providers router base. OpenAI-compatible API surface.
pub const HF_ROUTER_BASE: &str = "https://router.huggingface.co/v1";

/// Conservative cap on how many LLM round-trips one user prompt can
/// trigger. Each iteration is "send messages → get tool calls → run
/// them → send results back". 32 is enough for any plausible
/// real-world refactor task while preventing a runaway loop from
/// burning credits on a single typo.
pub const MAX_TURN_ITERATIONS: usize = 32;

/// Phase-6.2 system prompt. A real version that pulls in `AGENTS.md`,
/// `<workspace>/.moon/SYSTEM.md`, and discovered `SKILL.md` files
/// lands in 6.6 — see `specs/coder.md` § "What the LLM sees as
/// system prompt". This stub establishes the shape and gives the
/// model a usable identity for the early test loops.
pub const PHASE_6_0_SYSTEM_PROMPT: &str = r#"You are moon-coder, the AI coding assistant inside the moon-ide editor.

The user is working in a single workspace folder. You can call tools to read files, list directories, search the workspace, run bash commands, and edit files. Use them whenever you need to inspect or change the codebase — never guess at file contents. Keep tool calls focused: prefer one targeted `grep` over scanning every file.

Editing rules:
- Use `edit_file` for surgical changes. `find` must match the file exactly and uniquely; if you get a "matched N times" error, retry with more surrounding context. To insert text, set `find` to a stable nearby line and include it in `replace`. To delete, set `replace` to "".
- Use `write_file` for new files or whole-file rewrites. Create parent directories with `bash` first if they don't exist.
- Read before you edit. Don't invent file paths; when unsure of the layout, call `list_dir` first.

Be concise. Do not narrate what each tool call is for; the UI already shows the call to the user.
"#;
