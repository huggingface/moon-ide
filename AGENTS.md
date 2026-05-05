# Agent instructions

This file is read by AI coding agents (Cursor, Claude Code, opencode, etc.) when working in this repo. Humans read it too.

## Read-before-touching

1. Always read [specs/](specs/) before changing anything non-trivial. Specs describe intent; code is the implementation.
2. Skim [specs/architecture.md](specs/architecture.md) and [specs/protocol.md](specs/protocol.md) first.
3. Phased plan lives in [specs/roadmap.md](specs/roadmap.md).
4. Architecture decisions are in [specs/decisions/](specs/decisions/) (numbered ADR-style).

## Update-as-you-go

- If a spec is wrong or incomplete and you fix the underlying code, **update the spec in the same change**.
- New non-trivial decisions get a new ADR in `specs/decisions/NNNN-short-title.md`. Don't rewrite old ADRs; supersede them with a new one.
- Keep specs short, opinionated, and current. They are not aspirational marketing.
- Non-trivial commits get a **test plan** in `specs/test-plans/NNNN-short-slug.md` written before the commit (or while the human is testing). See [specs/test-plans/README.md](specs/test-plans/README.md) for what counts as non-trivial and the required headers.

## House rules

- **Early return** is preferred over nested conditionals.
- **No global try/catch in HTTP-style endpoints or Tauri commands**; let an error middleware / `tauri::Result` boundary handle it.
- **MB/kB use 1000-multiples**, not 1024.
- **Comments explain non-obvious intent only**, never narrate what the code does.
- **No emoji in code, docs, or commit messages** unless explicitly asked.
- **No one-line `if` / `else` / `for` / `while` without braces** — always use a block. Enforced by `oxlint`'s `curly` rule for JS/TS; Rust gets the same treatment by reviewer taste.
- **Tabs, not spaces** for indentation in every file we author. Editor display width is in `.editorconfig`. See [ADR 0004](specs/decisions/0004-code-style.md).
- Match existing code style; don't reformat unrelated code.
- **No pre-existing warnings.** If `cargo build`, `cargo clippy`, `vite build`, `tsgo`, `svelte-check`, `oxlint`, or any tool we run in CI prints a warning, treat it as a bug and fix it — even if it isn't your fault and even if it isn't what you were asked to do. The repo stays clean. The only exception is when the warning genuinely cannot be fixed without a wider refactor; in that case, suppress it locally with a comment explaining why.

## Tooling

- Format: `bun run fmt` (oxfmt + prettier-svelte) and `cargo fmt --all`.
- Lint: `bun run lint` (oxlint, type-aware) and `cargo clippy --all-targets -- -D warnings`.
- Type-check: `bun run check` (`tsgo --noEmit` + `svelte-check`).
- Full details and rationale: [ADR 0004 — code style](specs/decisions/0004-code-style.md).
- The IDE has to be able to develop itself. See [ADR 0005 — bootstrap](specs/decisions/0005-bootstrap.md).

## Phased delivery

This project is built in numbered phases (see [specs/roadmap.md](specs/roadmap.md)). **Stop at the end of each phase and wait for human review** before starting the next. The completion checklist at the bottom of each phase in the roadmap is the gate; do not auto-proceed even if every box is ticked.

## Scope discipline

Moon IDE serves one specific team. It is **not** a generic product, and the roadmap is **not** a wishlist.

- Don't pad phases with "nice-to-have" features just because other IDEs have them. If a feature isn't actively requested or blocking real work, leave it out — the team will surface real needs through testing and feedback.
- Hardcode first, configure later. If the team needs exactly one keybinding / one theme / one shortcut, hardcode it. Add user configuration when there's a second concrete need, not preemptively.
- Speculative ideas belong in prose ("later we might want X") in the relevant spec, **not** as checklist items in a phase. A checklist item is a commitment.
- When in doubt, ask the human reviewer rather than expanding scope.

### The bootstrap exception

If moon-ide's own source tree contains a file of some format, supporting that format is bootstrap, not speculation — see [ADR 0005](specs/decisions/0005-bootstrap.md). Concretely: the moon-ide repo has `Cargo.lock`, `bun.lock`, `.editorconfig`, `.npmrc`, etc., so syntax highlighting, formatting, and any tooling those files imply are in scope by default. Likewise for any language toolchain we use to develop moon-ide itself (Rust, TypeScript, Svelte). The "is anyone asking?" test still applies for everything else.

### No premature migrations

Until the roadmap's last phase ships, there is no "user installed base" worth keeping compatible with. Schemas (settings files, persisted app state, the JSON-RPC protocol, the `.moon/` directory layout) can be renamed, restructured, or deleted freely. **Don't write migration code, aliases, or backward-compat shims** for these — the cost is dead code that hides the real schema. Acceptable failure modes when a schema changes:

- The dev (whoever's running moon-ide on their machine) loses their last session / open tabs / persisted app state once. They reopen the folder; life goes on.
- A best-effort `tracing::warn!` on a parse failure is fine; falling back to defaults is fine; crashing on startup is **not**.

When the final roadmap phase lands and we declare a stable surface, this rule flips: schema changes get explicit migration paths. Until then, optimize for cleanliness of the current schema.

## Cross-cutting invariants

These are enforced by reviewers and CI; breaking them is a real bug:

1. The UI never directly calls git, LSP, fs, the coder / any LLM, or the terminal. Everything goes through the workspace core.
2. Anything that does I/O on the workspace must go through the active `WorkspaceHost` so it works the same locally and inside a devcontainer.
3. Container port forwarding is **explicit** — never auto-forward all listening ports.
4. `crates/moon-protocol/` is the single source of truth for the JSON-RPC schema. UI types (in TS) are generated/synced from it; do not hand-edit divergent copies.

## Adding a dependency

- Prefer the latest stable version (look up via `cargo search`, `npm view <pkg> version`).
- Add to `[workspace.dependencies]` for shared Rust deps; only add to a crate's own `[dependencies]` if it's truly local.
- For frontend: prefer small, focused packages over framework lock-in.
- Document significant adds in the relevant spec.

## Commit/PR hygiene

- Commits should be atomic and tell a story; no "wip" or "fix" alone.
- A change that touches both Rust and TS for the same feature lands in one commit.
- Reference the spec/ADR you're implementing in the commit body when relevant.
- Non-trivial commits also reference their test plan: `Test plan: specs/test-plans/NNNN-...md` in the commit body. The test plan file is part of the same commit.
