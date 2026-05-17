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
// errors. Markdown, Svelte, etc. get `null` until their servers
// land in a later stage.

const BY_EXTENSION: Record<string, string> = {
	ts: 'typescript',
	mts: 'typescript',
	cts: 'typescript',
	tsx: 'typescriptreact',
	js: 'javascript',
	mjs: 'javascript',
	cjs: 'javascript',
	jsx: 'javascriptreact',
	rs: 'rust',
	go: 'go',
	py: 'python',
	// `.pyi` is a type-stub file — same language id as a real
	// `.py`, ty consumes both. (We don't surface a separate
	// "python-stub" id; servers don't model that distinction.)
	pyi: 'python',
};

export function lspLanguageFor(path: string): string | null {
	// Strip anything past the last `.`; then match the known table.
	// Dotless files (`Dockerfile`, `.editorconfig`) never map to an
	// LSP here — the shipped language servers only care about
	// JS/TS, Rust, and Python, and the bootstrap files moon-ide
	// handles specially are not language-server-backed.
	const base = path.split('/').pop() ?? path;
	const dot = base.lastIndexOf('.');
	if (dot < 0) {
		return null;
	}
	const ext = base.slice(dot + 1).toLowerCase();
	return BY_EXTENSION[ext] ?? null;
}

// File-language ids that the JS/TS-aware LSP slots cover. `tsgo` and
// `oxlint` happen to cover the same four ids; mirrors
// `OXLINT_LANGUAGES` in `crates/moon-core/src/lsp/server.rs`, kept in
// lock-step by hand because there are only ever a handful of values
// and a generated bridge for a 4-entry list is more ceremony than
// it's worth. If oxlint ever grows Vue / Astro support upstream, this
// is the second place to update.
const JS_TS_FILE_LANGUAGES: ReadonlySet<string> = new Set([
	'typescript',
	'typescriptreact',
	'javascript',
	'javascriptreact',
]);

/**
 * Does the LSP slot identified by `slotLanguageId` (the broker's slot
 * key — `"typescript"`, `"rust"`, `"oxlint"`, …) emit diagnostics for
 * a file whose own language id is `fileLanguageId` (the value
 * `lspLanguageFor()` returns)?
 *
 * For language servers (`tsgo`, `rust-analyzer`, …) the slot's id and
 * the file's id align — `"typescript"` covers the four JS/TS file
 * ids, `"rust"` covers `"rust"`, etc. For the linter co-tenant the
 * slot is `"oxlint"` but it covers the JS/TS file ids. Callers that
 * need to "reopen every buffer this slot governs" (status-pill
 * Restart, crash-recovery) want this membership test, not a strict
 * id equality.
 */
export function lspSlotCoversFile(slotLanguageId: string, fileLanguageId: string): boolean {
	if (slotLanguageId === 'oxlint' || slotLanguageId === 'typescript') {
		return JS_TS_FILE_LANGUAGES.has(fileLanguageId);
	}
	return slotLanguageId === fileLanguageId;
}
