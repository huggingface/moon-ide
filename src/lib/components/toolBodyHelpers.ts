// Shared helpers for the per-tool body components in CoderPanel
// (`ToolBodyReadFile`, `ToolBodyGrep`, …). They all share the same
// shape: parse the tool's args / result JSON into something typed,
// fall back to a JSON view when the payload doesn't match, and —
// when there's source code involved — feed it through the same
// `@lezer/highlight` pipeline the markdown renderer uses so a `.ts`
// snippet shares colours with the live editor.
//
// Keep this module dependency-light: it must not import any Svelte
// runtime ($state / $effect / $derived) so the tool body components
// can use the helpers in regular `.svelte` script blocks without the
// helpers themselves becoming reactive.

import { highlightCode, loadHighlighters } from '../editor/highlightCode';
import { workspace } from '../state.svelte';

export { highlightCode, loadHighlighters };

/** Open `path` in the editor, optionally landing the caret at
 *  `line` (1-based, matching the grep / read_file convention).
 *
 *  Path routing:
 *  - **Relative path** → workspace-folder-relative against the
 *    active folder. The tool ran in *some* folder's context, but
 *    that's not threaded through to the panel; defaulting to
 *    active is correct ~all the time and acceptably wrong in the
 *    "user switched folders mid-turn" edge case (the file just
 *    won't be found, no crash).
 *  - **Absolute path under the active folder** → relativised and
 *    routed the same way, so the buffer gets the full LSP / git /
 *    editorconfig treatment.
 *  - **Absolute path outside any active-folder root** → opened as
 *    an external host file with no line landing. Better than
 *    silently dropping the click.
 *
 *  `line === null` opens at the file's current cursor (or 0 if
 *  fresh), the same shape as a tab-bar click. */
export async function openToolPath(path: string, line: number | null = null): Promise<void> {
	const trimmed = path.trim();
	if (trimmed.length === 0) {
		return;
	}
	const isAbsolute = trimmed.startsWith('/');
	const targetLine = line !== null && line >= 1 ? line - 1 : null;

	if (!isAbsolute) {
		if (targetLine !== null) {
			await workspace.jumpTo(trimmed, { line: targetLine, character: 0 });
			return;
		}
		await workspace.openFile(trimmed);
		return;
	}

	// Absolute path: try to relativise against the active folder
	// before falling back to the host-only loader. The host-only
	// path doesn't accept a target line — we'd need a separate
	// pending-jump mechanism keyed by absolute path, which is
	// more plumbing than this click is worth right now.
	const af = workspace.activeFolderPath;
	if (af !== null && af.length > 0) {
		const root = af.endsWith('/') ? af : `${af}/`;
		if (trimmed === af) {
			return;
		}
		if (trimmed.startsWith(root)) {
			const relative = trimmed.slice(root.length);
			if (targetLine !== null) {
				await workspace.jumpTo(relative, { line: targetLine, character: 0 });
			} else {
				await workspace.openFile(relative);
			}
			return;
		}
	}
	await workspace.openHostFile(trimmed);
}

export function escapeHtml(s: string): string {
	return s
		.replace(/&/g, '&amp;')
		.replace(/</g, '&lt;')
		.replace(/>/g, '&gt;')
		.replace(/"/g, '&quot;')
		.replace(/'/g, '&#39;');
}

/** `highlightCode` returns a full `<pre><code>…</code></pre>`
 *  wrapper because that's what markdown-it's `highlight` hook
 *  expects to splice in. The tool body components render their own
 *  `<pre>` for layout reasons (sticky line-number columns, diff
 *  styling, etc.), so this peels off the wrapper and returns the
 *  inner spans-and-text to be `{@html}`'d into the caller's markup. */
export function extractInnerHtml(html: string): string {
	const codeStart = html.indexOf('<code');
	if (codeStart < 0) {
		return html;
	}
	const tagEnd = html.indexOf('>', codeStart);
	if (tagEnd < 0) {
		return html;
	}
	const codeEnd = html.lastIndexOf('</code>');
	if (codeEnd < 0) {
		return html;
	}
	return html.slice(tagEnd + 1, codeEnd);
}

/** Detect the coder runner's tool-error envelope and return the
 *  message string. The runner emits exactly
 *  `{ "error": "<message>" }` (with `is_error: true`) for any
 *  `CoderError::ToolFailed` / `InvalidToolArgs` an individual tool
 *  raises — see `crates/moon-coder/src/runner.rs:finish_tool_call`.
 *
 *  Returns `null` when `result` doesn't match the envelope shape so
 *  callers can fall through to their normal success rendering.
 *  Tool-body components use this to surface "find matched 3 times,
 *  pass occurrence" / "find not found in foo.rs" / "binary file"
 *  cleanly inline rather than collapsing into the generic JSON
 *  fallback. */
export function parseToolError(result: unknown): string | null {
	if (typeof result !== 'object' || result === null) {
		return null;
	}
	// Same shape-narrowing pattern the per-tool `parseArgs` /
	// `parseResult` helpers use: cast to a partial type with only
	// the field we care about so the assertion stays a strict
	// subset of `object` (the type the runtime check already
	// narrowed to).
	const o = result as { error?: unknown };
	if (typeof o.error !== 'string') {
		return null;
	}
	const msg = o.error.trim();
	return msg.length > 0 ? msg : null;
}

/** Map a workspace-relative path to a fence id understood by
 *  `highlightCode`. Mirrors the canonical / alias lists in
 *  `src/lib/editor/highlightCode.ts`; returns `null` for any
 *  extension we don't ship a grammar for so the renderer
 *  silently degrades to plain (escaped) text rather than
 *  mis-highlighting. */
export function fenceLangFromPath(path: string | null): string | null {
	if (path === null) {
		return null;
	}
	const baseName = path.split('/').pop() ?? path;
	if (baseName === 'Cargo.lock') {
		return 'toml';
	}
	if (baseName === 'Dockerfile' || baseName.startsWith('Dockerfile.') || baseName.endsWith('.Dockerfile')) {
		return 'dockerfile';
	}
	if (baseName === '.editorconfig' || baseName === '.npmrc') {
		return 'properties';
	}
	const dot = baseName.lastIndexOf('.');
	if (dot < 0) {
		return null;
	}
	const ext = baseName.slice(dot + 1).toLowerCase();
	const map: Record<string, string> = {
		ts: 'ts',
		mts: 'ts',
		cts: 'ts',
		tsx: 'tsx',
		js: 'js',
		mjs: 'js',
		cjs: 'js',
		jsx: 'jsx',
		json: 'json',
		jsonc: 'json',
		css: 'css',
		scss: 'css',
		less: 'css',
		html: 'html',
		htm: 'html',
		svelte: 'svelte',
		md: 'markdown',
		rs: 'rust',
		go: 'go',
		py: 'python',
		pyi: 'python',
		sh: 'shell',
		bash: 'shell',
		zsh: 'shell',
		yml: 'yaml',
		yaml: 'yaml',
		toml: 'toml',
	};
	return map[ext] ?? null;
}

/** Build a right-aligned, newline-joined column of line numbers
 *  from `start` (1-based, inclusive) to `start + count - 1`. The
 *  width is sized to the largest number, so a 1–9 column stays
 *  one char wide and a 1–999 column three. Used by the read /
 *  write / edit body components to render a gutter that vertically
 *  aligns with the highlighted code column next to it. */
export function buildLineNumberColumn(start: number, count: number): string {
	if (count <= 0) {
		return '';
	}
	const last = start + count - 1;
	const width = String(last).length;
	const lines: string[] = [];
	for (let i = 0; i < count; i += 1) {
		lines.push(String(start + i).padStart(width, ' '));
	}
	return lines.join('\n');
}

/** Render `value` as pretty JSON for the JSON-fallback view. Only
 *  the tool-body components use this — the panel's existing
 *  `fmtArgs` is identical, but exporting one shared helper here
 *  keeps the fallback consistent across every component. */
export function fmtJson(value: unknown): string {
	if (value === null || value === undefined) {
		return '';
	}
	try {
		return JSON.stringify(value, null, 2);
	} catch {
		// `JSON.stringify` only throws on circular references or
		// BigInt values — neither shape we'd ever expect from a
		// well-typed tool payload, but worth a sane fallback so
		// the panel doesn't crash on a malformed trace. We avoid
		// `String(value)` here because oxlint's `no-base-to-string`
		// flags it for `unknown` (the unknown could be `{}`, which
		// would stringify to the unhelpful `[object Object]`).
		if (typeof value === 'string') {
			return value;
		}
		if (value instanceof Error) {
			return value.message;
		}
		return '[unrepresentable]';
	}
}
