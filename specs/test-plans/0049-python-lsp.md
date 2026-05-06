# Test plan 0049: Python LSP via `ty`

- **Date**: 2026-05-06
- **Phase**: 4 (LSP)

## What shipped

- `moon-core::lsp::server`: new `PYTHON_SERVER` spec (`bin = "ty"`, `args = ["server"]`) and `DiscoveryStrategy::PythonVenv`. Discovery walks ancestors for `.venv/bin/<bin>` (Unix) / `.venv/Scripts/<bin>.exe` (Windows), then `$PATH`. Container-side resolution mirrors the TS path: walk host ancestors for `.venv/bin/<bin>` inside the bind mount, translate to the server-side absolute path via `HostMount`, return `None` for hoisted-out venvs (broker falls back to host).
- `moon-core::lsp::broker`: `spec_for("python")` → `PYTHON_SERVER`.
- `src/lib/editor/lspLanguage.ts`: `.py` / `.pyi` → `python`.
- `src/lib/editor/language.ts` + `highlightCode.ts`: bundle `@codemirror/lang-python` so `.py` files in the editor (and python-fenced markdown blocks) syntax-highlight; otherwise the diagnostic gutter floats on plain text.
- `specs/lsp.md`: documents the `ty`-as-LSP decision, adds `PythonVenv` to the discovery section, removes Python from the container LSP non-goals list.

`ty` is **not** baked into the `moon-base` image. Per-project install (via `uv add --dev ty` or `uv tool install ty`) matches the `tsgo` UX.

## How to test

Prerequisites: `bun install`, `cargo build`. Either install `ty` globally (`uv tool install ty`) or per project (see step 4).

### Host LSP (no container)

1. Open a folder containing a `.py` file. Drop a deliberate type error in it (e.g. `x: int = "hello"`).
2. With `ty` on `$PATH`:
   - Status bar shows `python` pill, transitioning `starting…` → `running`.
   - Diagnostic gutter lights up on the bad line; hover reads the `ty` message.
   - Ctrl-Space inside an identifier produces completions.
   - Ctrl-click on a symbol jumps to its definition (within the same file at minimum; cross-file when `ty`'s analysis covers it).
3. Without `ty` on `$PATH`: status bar pill reads `python: not available`; tooltip is `uv add --dev ty (or uv tool install ty)`.
4. Per-project install: in the workspace root, `uv venv && uv pip install ty`. Reopen the file. Discovery should pick `.venv/bin/ty` over a global `ty`; trace log line is `lsp: resolved via project-local .venv`. Bumping the global `ty` version doesn't change the buffer's behaviour until the project venv is updated.

### Container LSP

1. Workspace with `moon-base` configured + `Running`. The image does NOT pre-install `ty`.
2. Open a `.py` file: pill should read `python: not available` initially. The host fallback kicks in only if the host has `ty` on `$PATH`.
3. Inside the container terminal: `uv pip install ty` in the project's `.venv` (or `pip install ty` in any venv at the project root). Reopen the file: discovery via `container_binary_path` finds `/workspace/<basename>/.venv/bin/ty`, the broker spawns `docker exec -i <container> /workspace/.../.venv/bin/ty server`, status pill flips to `running`.
4. Hoisted-venv test: place `.venv/` at a parent of the bound folder. Container-side discovery returns `None`, broker falls back to host. Status pill reflects whichever route resolves; `tracing::debug!` line `lsp: .venv match sits outside the bind mount` confirms the fallback.

### Regression

- `.ts` / `.rs` LSPs still spawn and serve diagnostics — `spec_for` still routes them.
- `.py` syntax highlighting: keywords, strings, comments, decorators all colorize even when `ty` isn't running.
- `.pyi` stub files highlight + LSP-open identically to `.py`.
- Markdown code blocks fenced ` ```python ` highlight via `highlightCode.ts`.

## What must keep working

- Existing TS / Rust LSP behaviours (diagnostics, hover, goto-def, status pill).
- Container-vs-host fallback logic (now exercised by Python with hoisted venvs).
- `lsp:status` and `lsp:diagnostics` event flow — Python emits the same `LspServerEvent::Diagnostics` shape.

## Known limitations

- `ty` is in beta (`0.0.x` versions; Astral notes breaking changes between releases). If a feature gap blocks us, switching to `pyright-langserver` or `pylsp` is a one-string edit on `PYTHON_SERVER`.
- Project-relative `pyproject.toml` configuration (e.g. excluded paths, target Python version) is whatever `ty` reads from the workspace root by default; we don't proxy `workspace/configuration` requests yet.
- No `.python-version` / `pyenv` integration — discovery picks the first `.venv/bin/ty` it sees in the ancestor walk.

## Related

- Specs: [lsp.md](../lsp.md), [containers.md](../containers.md)
- ADR: [0008 — host-shared docker daemon](../decisions/0008-host-shared-daemon.md) (container LSP routing context)
- Upstream: <https://github.com/astral-sh/ty>
