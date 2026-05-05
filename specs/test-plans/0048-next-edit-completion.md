# Test plan 0048: Next-edit completion (llama.cpp)

- **Date**: 2026-05-05
- **Phase**: Editor polish / innovation (not tied to a single roadmap phase)

## What shipped

- `moon-protocol` + `moon-core::next_edit`: Sweep-style prompt builder, `GET /health` probe, `POST /completion` client for llama.cpp.
- `moon-core::next_edit_server`: supervises a `llama-server` child (`--hf-repo` for Hugging Face download), log ring buffer, exit poller; `stop_all` kills it on IDE quit.
- Tauri: `next_edit_probe`, `next_edit_complete`, `next_edit_server_start` / `stop` / `status`; `AppState.next_edit` persists binary path, HF repo, listen host/port, optional `external_base_url` (manual server), and `server_autostart` (managed start → on, stop → off; relaunch auto-starts when on).
- Status bar **autocomplete** control: Start/Stop server, binary + HF repo + listen host/port; optional external URL under Advanced; derived `http://host:port` when external is empty; log tail; pip reflects probe (ready / warn / loading).
- CodeMirror: **LSP-only** completion (Ctrl+Space). Local autocomplete (**Ctrl+T** or palette) bypasses completion — it calls `next_edit_complete` and patches the document; status pip shows in-flight while `/completion` runs. Apply path **shrinks** the 21-line model output to a minimal line LCP/LCS middle (reduces duplicated tail), extends the replaced span through the **line break** after the last affected line (CodeMirror `line.to` stops before `\n`), restores a trailing newline when the deleted slice had one, and **maps the caret** from the pre-request anchor instead of jumping to the end of the insert.

## How to test

Prerequisites: `bun install`, `cargo build`, a `llama-server` on `PATH` (or absolute path), network for HF on first model pull.

1. Launch moon-ide, open a git-backed folder and a source file under that folder.
2. Status bar: click **autocomplete**. Expected: popover shows probe line (may be unreachable until server runs); pip grey/neutral until first probe settles, then warn/ready/loading per `/health`.
3. HF repo defaults to `sweepai/sweep-next-edit-1.5B`; adjust if needed, set binary if not `PATH`, click **Start server**. Expected: log lines appear; `/health` eventually ready (may stay 503 during download); **Stop server** kills the process (pip / probe reflect stopped). No separate HTTP base field in the main flow — URL is `http://{listen host}:{port}`.
4. Advanced: set external URL, Save — **Start server** disables; probes hit external URL. Clear external URL to use managed launch again.
5. With Ready: **Ctrl+T** (or palette **Editor: Autocomplete (Ctrl+T)**) calls the model and **replaces the returned line range** in the buffer (no completion popover). **Ctrl+Space** only shows LSP items. While the model request runs, the status bar shows in-flight / pip loading.
6. Quit moon-ide while server running. Expected: child is stopped (no orphan `llama-server` from moon-ide).
7. Restart app: settings restore; server does **not** auto-start (user clicks Start again).

## What must keep working

- LSP Ctrl+Space completion still works alone; local autocomplete never uses the completion popover (only Ctrl+T / palette).
- `app_state_save` merge still preserves `slack`, `right_panel`, `coder`; `next_edit` is written from the frontend payload.

## Known limitations

- No automatic download of weights; 503 is shown as “model loading” (covers HF download inside llama-server as well as mmap load).
- Untitled buffers and paths outside the active folder skip autocomplete (flash + no request).
- Training format extras (multi-file `file_sep` blocks, recent diff chunks) are not sent yet — only the 21-line original/current/updated window; see Sweep post for the full recipe.

## Related

- Specs: [protocol.md](../protocol.md), [architecture.md](../architecture.md)
- Blog: [Open sourcing a 1.5B Next-Edit Autocomplete Model](https://blog.sweep.dev/posts/oss-next-edit)
