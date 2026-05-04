// LSP language-id mapping.
//
// The LSP spec names are the canonical strings servers expect in
// `textDocument/didOpen` — not the CodeMirror grammar names. Both
// ends of moon-ide's LSP plumbing (frontend → backend `ipc.lsp.*`
// calls → `moon_core::lsp::broker::spec_for`) use the same string,
// so this table is effectively the LSP feature-flag per file type.
//
// Return `null` for file types that have no wired-up LSP server —
// callers skip LSP calls entirely rather than surface "no server"
// errors. Markdown, Rust, Svelte, etc. get `null` until their
// servers land in a later stage.

const BY_EXTENSION: Record<string, string> = {
	ts: 'typescript',
	mts: 'typescript',
	cts: 'typescript',
	tsx: 'typescriptreact',
	js: 'javascript',
	mjs: 'javascript',
	cjs: 'javascript',
	jsx: 'javascriptreact',
};

export function lspLanguageFor(path: string): string | null {
	// Strip anything past the last `.`; then match the known table.
	// Dotless files (`Dockerfile`, `.editorconfig`) never map to an
	// LSP here — the two shipped language servers only care about
	// JS/TS, and the bootstrap files moon-ide handles specially are
	// not language-server-backed.
	const base = path.split('/').pop() ?? path;
	const dot = base.lastIndexOf('.');
	if (dot < 0) {
		return null;
	}
	const ext = base.slice(dot + 1).toLowerCase();
	return BY_EXTENSION[ext] ?? null;
}
