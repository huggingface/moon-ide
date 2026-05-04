# Test plan 0030: Rust LSP (rust-analyzer)

- **Date**: 2026-05-04
- **Phase**: Phase 4 (LSP)

## What shipped

- Rust files (`.rs`) now route through the LSP broker using `rust-analyzer` — diagnostics, hover, completion, and go-to-definition all work the same way they already did for TypeScript.
- Binary discovery is per-language: a new `DiscoveryStrategy::CargoHome` variant checks `$CARGO_HOME/bin/<bin>` (falling back to `$HOME/.cargo/bin/` or `$USERPROFILE/.cargo/`) before `$PATH`. Covers `rustup component add rust-analyzer` reliably even when the Tauri process's inherited `PATH` is the desktop launcher's stripped-down one rather than a login shell's.
- The LSP language mapping table (`lspLanguageFor`) learned `rs → rust`. Everything else (hover UI, goto-def underlining, nav history, status pill) composes unchanged.
- Docs / spec updates in `specs/lsp.md` describe both discovery strategies and the Rust install hint shown in the status pill when the binary is missing.

## How to test

Prerequisites: `bun install`, a working moon-ide dev build (`bun run tauri dev`), and `rust-analyzer` installed on the host (`rustup component add rust-analyzer` or a cargo-install build). The moon-ide repo itself is a great test workspace — it has a Cargo workspace with real cross-crate references.

### Startup / status pill

1. Open the moon-ide folder. Open any `.rs` file (e.g. `crates/moon-core/src/host.rs`).
2. Status bar should briefly show a "rust: starting" pill, then drop it within a few seconds when `rust-analyzer` finishes its first `cargo metadata` pass. No pill = running happily.
3. Uninstall `rust-analyzer` (`rustup component remove rust-analyzer` in another terminal) and reopen the IDE. Opening any `.rs` file should leave a persistent pill with tooltip `rustup component add rust-analyzer`. TypeScript LSP should still work — the two are independent servers.
4. Reinstall (`rustup component add rust-analyzer`). Restart the IDE. Pill goes away on first `.rs` open.

### Diagnostics

1. Open `crates/moon-core/src/host.rs`. After rust-analyzer warms up (visible as the status pill disappearing), the gutter should be clean.
2. Introduce a syntax error — e.g. remove a trailing `}` from a function — and save. A red gutter marker appears on the offending line; hovering the gutter icon shows the error message (`expected item, found end of input`, etc.).
3. Undo. Diagnostic clears within a couple of seconds of the edit being reflected in the saved buffer.
4. Introduce a type error — e.g. assign a `String` to a `&str` binding. Same behaviour, with the actual rustc-style message.

### Hover

1. With the buffer clean, hover a symbol (function name, type). Tooltip appears after ~300 ms with the symbol's signature, doc comment, and often the defining module path.
2. Fenced code blocks in the tooltip render with Rust syntax highlighting (same pipeline as the Markdown hover fix in `0025-markdown-syntax-highlighting`).
3. Hover a literal / whitespace: no tooltip. No errors.

### Completion

1. Inside a function body, type `std::collections::` and press `Ctrl+Space`. The completion popover lists `HashMap`, `BTreeMap`, `HashSet`, etc. Selecting one inserts the identifier.
2. On an imported type, type `.` and press `Ctrl+Space`. Methods + fields appear. Kind icons (method / field / trait-method) match the TypeScript popover's conventions.
3. Escape dismisses the popover; the "do not auto-open" rule from TS still holds — typing alone without `Ctrl+Space` never spawns the popover.

### Go-to-definition

1. Hold `Ctrl` (Linux/Windows) or `Cmd` (macOS), hover a function call inside a moon-ide crate. The identifier underlines in accent colour within ~300 ms.
2. Click while still holding the modifier. The editor jumps to the function's definition. Caret lands on the name, not inside the body.
3. Use `Alt+Left` to go back. History works across the jump.
4. Cross-crate jumps in the Cargo workspace (e.g. from `moon-desktop` into `moon-core`) should work; both crates share one `rust-analyzer` instance per workspace.
5. `Ctrl`-clicking a symbol whose definition is in an external crate (e.g. `tokio::sync::Mutex`) should show a toast explaining the target is outside the workspace — same degradation path as TS `node_modules` targets.

### Nav history interop

1. Jump between three `.rs` files via goto-definition + tab clicks.
2. `Alt+Left` / `Alt+Right` walks the history with correct caret positions, same as test plan `0028`.
3. Mixed-language nav: jump from a `.ts` file into a `.rs` file (via the file tree) and back. History entries are per-file and language-agnostic.

### Performance sanity

1. First `.rs` open in a cold repo should reach "diagnostics rendered" inside the time rust-analyzer's own `cargo metadata` takes (10-30 s on moon-ide on a warm machine, more on a cold disk). The editor stays responsive during this window — other tabs, terminal, command palette all keep working.
2. Subsequent opens inside the same session are fast (< 500 ms to first diagnostic); the server is already warm.
3. Memory: `rust-analyzer` typically sits at a few hundred MB for moon-ide. Not alarming. Killing the IDE should reap the child (verify with `pgrep rust-analyzer` after closing the window).

## What must keep working

- TypeScript LSP from test plans `0024` / `0027` / `0028` — same diagnostics, hover, completion, goto-def, and nav history.
- Markdown rendering (`0025`) — fenced code blocks inside Rust hover tooltips should use the same pipeline.
- Inline git blame (`0029`) — active-line annotation in `.rs` files matches `.ts` behaviour.
- File tree, editor splits, bracket QoL (`0023`), bottom panel / terminal (`0026`).

## Known limitations

- LSP runs on the host only. Rust files inside a devcontainer still spawn `rust-analyzer` from the developer's host toolchain, not the container's. Routing LSP stdio through `WorkspaceHost` (so a container-provided `rust-analyzer` could serve a container-bound folder) is a deliberate future scope.
- No `workspace/didChangeConfiguration` plumbing yet: rust-analyzer runs with defaults. `checkOnSave`, `cargo.features`, proc-macro toggles, diagnostic customisation — all at their defaults. Fine for moon-ide's repo; revisit if a team needs project-specific LSP tuning.
- `rust-analyzer` doesn't ship as a per-project npm/cargo dep, so there's no moon-ide-pinned version. You get whatever's in `$CARGO_HOME/bin/` or on `$PATH`. This matches how every other Rust editor tool does it.
- No rename / find-references / code-actions surface for Rust (or TS). Scoped out until someone asks.
- Proc-macro expansion, flycheck, and other rust-analyzer-only bells are available to the server but not surfaced in any moon-ide-specific UI.

## Related

- Spec: `specs/lsp.md` — updated with the new discovery strategy and Rust server section.
- Prior test plans: `0024-lsp-typescript-stage-1.md`, `0027-lsp-goto-definition-nav-history.md`, `0028-nav-history-positions-cross-folder.md`.
