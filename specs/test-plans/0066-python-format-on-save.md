# Test plan 0066: Python format-on-save (ruff, with `.venv` preference)

- **Date**: 2026-05-07
- **Phase**: 5.x (format-on-save). Adds a second row to the language-default fallback table; reuses the existing container-aware `ShellTarget` plumbing from test plan 0063.

## What shipped

- **`format::default_format_command` now returns a `DefaultFormatCommand { command, cwd }` struct** instead of a bare command string. The Rust path keeps the existing "file's parent dir" cwd; the Python path pins to the project root so a relative bin token resolves correctly (and so `ruff` finds the project's `[tool.ruff]` config). All callers updated; behaviour for Rust files is unchanged.
- **New Python row** in the language-default table: `.py` and `.pyi` route through `ruff format`. Walks parents from the file looking for `pyproject.toml`, `setup.py`, or `setup.cfg`; the first hit becomes the project root + subprocess cwd. When `<root>/.venv/bin/ruff` exists on the host filesystem we use `.venv/bin/ruff format`; otherwise we fall through to bare `ruff format` against `PATH`.
- **Container parity for free.** Because the project's `.venv` is bind-mounted into the container at `/workspace/<basename>/.venv`, the host-side `is_file()` probe is correct for both targets, and the relative `.venv/bin/ruff` bin token resolves under `docker exec -w /workspace/<basename> … .venv/bin/ruff format <translated_abs_path>` exactly like it does on the host. No bin-token translation needed.
- **Six new tests** in `crates/moon-core/src/format.rs` — `.venv` preference, no-`.venv` fallback, loose-file fallback (no project marker), `setup.py`-anchored project root, `.pyi` stub support, and the existing Rust tests rewritten against the new struct shape.
- **ADR 0013 updated** with the new table row, a "Python: ruff with venv preference" subsection covering host/container symmetry, and a "Why a struct, not just a string" note explaining the API change.

## How to test

Prerequisites: `bun install`, `cargo build`, `bun run tauri dev`. The interesting target is `~/code/huggingface_hub` — a real Python project the team uses, with `pyproject.toml`, `[tool.ruff]` config, and `.venv/bin/ruff` from `uv venv`.

### `huggingface_hub` (real project, container-bound)

1. Bind-mount `~/code/huggingface_hub` as a moon-ide workspace folder running inside the dev container (the existing container-shell wiring from ADR 0002 / test plan 0063).
2. Open any `.py` file under `src/`. Add a deliberate format defect — extra spaces, mis-aligned import, etc.:
   ```python
   import os,sys
   def f( x:int )->int:
       return  x +1
   ```
3. Save (`Ctrl+S` / `Cmd+S`).
4. Expected: the file gets reformatted to match `ruff format`'s output (single-statement imports, normalised spacing). The buffer reloads with the new bytes; the editor shows no "file changed on disk" prompt.
5. Open the dev terminal running `bun run tauri dev`. Confirm there's no `format-on-save: tool not found` warning. With `RUST_LOG=moon_core=debug` you can see the `ShellTarget::Container { … }` branch fire.

### `.pyi` stubs route the same way

6. Create `src/something.pyi` with `def f(x:int)->int:...` (no spaces). Save.
7. Expected: ruff reformats it to `def f(x: int) -> int: ...`. Same behaviour as `.py`.

### Loose `.py` outside any project

8. Create `~/scratch/foo.py` (no `pyproject.toml`, no `setup.py`, no `.venv`) inside a moon-ide workspace folder. Edit, save.
9. Expected: ruff (system / PATH) runs; if not installed, the `format-on-save: tool not found in node_modules/.bin or $PATH; skipping` warning fires once and the file keeps its editorconfig-normalised bytes (whitespace + EOL fixed but no syntactic reformat).

### `pyproject.toml` without a `.venv`

10. Pull a project with a top-level `pyproject.toml` but no `.venv/` (e.g. a fresh clone before `uv sync`). Edit a `.py` file, save.
11. Expected: bare `ruff format` runs (resolved from the host or container PATH). If ruff is system-installed, the file reformats. Otherwise the same "tool not found" warning fires.

### lint-staged still wins

12. In a Python project that ships `{"*.py": "black"}` in `package.json#lint-staged`, save a `.py` file.
13. Expected: `black` runs (lint-staged layer 1), not `ruff` (default layer 2). The default table is a fallback, never an override.

### Rust path unchanged

14. Save a `.rs` file in `~/code/workloads`. Expected: `rustfmt --edition 2024 <file>` (or whatever edition `Cargo.toml` declares) still runs and the file reformats. Same behaviour as test plan 0063 — the struct refactor is invisible to Rust users.

### Backend unit tests

15. `cargo test -p moon-core --lib -- format::tests::default_format_command_python_prefers_venv_ruff` — green (drops a fake `.venv/bin/ruff` shim, asserts the resolver returns `.venv/bin/ruff format` with cwd at the project root).
16. `cargo test -p moon-core --lib -- format::tests::default_format_command_python_falls_back_to_bare_ruff_without_venv` — green.
17. `cargo test -p moon-core --lib -- format::tests::default_format_command_python_loose_file_uses_parent_dir` — green.
18. `cargo test -p moon-core --lib -- format::tests::default_format_command_python_setup_py_anchors_project_root` — green.
19. `cargo test -p moon-core --lib -- format::tests::default_format_command_python_pyi_stubs_route_through_ruff` — green.
20. `cargo test -p moon-core --lib -- format::tests::default_format_command_rust` — three green (rust path tests rewritten against the new struct shape).

## What must keep working

- **Rust format-on-save** (`~/code/workloads`, every other `.rs` repo). Refactor returns the new struct; `cwd` is `abs.parent()`, command is `rustfmt --edition <e>`. Same as before from `run_formatter`'s perspective.
- **lint-staged matches** (`moon-landing`, `moon-ide` itself, every project with a `.lintstagedrc.json`). Layer 1 still runs first; the language-default layer only fires when lint-staged didn't match.
- **`run_formatter` host vs. container split** (test plan 0063). Unchanged. The container path translates `cwd` and the abs file path through the bind mount; the bin token is whatever the resolver returned (now potentially relative — `.venv/bin/ruff` — instead of absolute, but `docker exec -w <translated_cwd> … <bin_token>` resolves the same way `current_dir(…)` does on the host).
- **`bun run check`, `bun run lint`, `cargo check --workspace --exclude moon-desktop`, `cargo clippy --workspace --exclude moon-desktop --all-targets -- -D warnings`** all clean.

## Known limitations

- **`.venv` only.** `venv/` (no leading dot) is not probed. Some teams use that convention; the user's `huggingface_hub` setup uses `.venv` (the `uv venv` default) and AGENTS.md "hardcode first, configure later" applies.
- **`uv` / `poetry run` / `pipenv run` wrappers not detected.** If a project ships its toolchain via `poetry run ruff` rather than `.venv/bin/ruff` we'd need a separate detection path. None of the team's projects do this today.
- **No type-check on save.** Astral's `ty` (used by `huggingface_hub` for type checking) is invoked by the LSP, not by format-on-save. Conflating the two would mean blocking saves on typecheck failures, which is the wrong UX for "I'm in the middle of refactoring and want to save my work".
- **No `black` / `autopep8` row.** When a team project that needs them lands on the format-on-save path we add a row; until then ruff is the only Python default. lint-staged remains the override path for any project that wants something else.
- **`pyproject.toml` walk picks the first hit.** Nested Python packages (a workspace-style monorepo with multiple `pyproject.toml` files) get the closest one to the file. That matches what `ruff` itself would do walking up from cwd.
- **`is_file()` is sync.** The whole `default_format_command` resolver is synchronous; we run a single `metadata` call per save when the file is `.py` / `.pyi` / `.rs`. Sub-millisecond on every filesystem we care about; fine to keep on the save hot path. Async would gain nothing.

## Related

- ADRs:
  - [0002 — workspace host](../decisions/0002-workspace-host.md) (`ShellTarget` is one more `WorkspaceHost`-shaped abstraction).
  - [0013 — format on save (file-based)](../decisions/0013-format-on-save-file-based.md) (the home of the language-default table; updated as part of this change).
- Specs: [`containers.md`](../containers.md) (host vs. container shell routing the formatter rides on top of).
- Prior plans:
  - [0063 — format on save (file-based, container-aware)](0063-format-on-save-file-based.md) — same architecture, this plan adds the Python row.
