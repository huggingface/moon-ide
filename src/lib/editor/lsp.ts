// CodeMirror 6 adapters that bridge LSP data from `workspace` state
// into three editor surfaces:
//
// 1. **Diagnostics** — a `ViewPlugin` watches `workspace.diagnostics`
//    for the current path and pushes CM `Diagnostic[]` into
//    `@codemirror/lint`'s state field. CM renders gutter markers and
//    underlines; the panel stays closed (we surface counts in the
//    status bar instead).
// 2. **Hover tooltip** — CM's `hoverTooltip` extension fires on
//    mouse hover; we call `ipc.lsp.hover` and render the Markdown
//    response inside a popover styled with our theme.
// 3. **Completion source** — a source registered on the editor's
//    existing `autocompletion` extension. Returns completions from
//    `ipc.lsp.completion` on explicit invocation (Ctrl-Space); we
//    keep `activateOnTyping: false` at the top level so the
//    built-in buffer-identifier source stays quiet.
//
// Every adapter is keyed on the editor's current file path, passed
// through the `filePathFacet`. The path is the stable id we use on
// the wire for `didOpen` / `didChange` / `didClose`; the LSP broker
// resolves it to a `file://` URI against the workspace root.

import type { Completion, CompletionContext, CompletionResult, CompletionSource } from '@codemirror/autocomplete';
import { setDiagnostics, type Diagnostic as CmDiagnostic, lintGutter } from '@codemirror/lint';
import { Facet, type Extension } from '@codemirror/state';
import { EditorView, hoverTooltip, type Tooltip } from '@codemirror/view';
import { ipc } from '../ipc';
import type { LspDiagnostic, LspSeverity } from '../protocol';
import { lspLanguageFor } from './lspLanguage';

// Facet so every adapter (diagnostics, hover, completion) reads the
// *same* path for the editor view. The Editor component `.of()`s it
// at construction and `reconfigure`s a compartment when the active
// tab swaps.
export const filePathFacet = Facet.define<string | null, string | null>({
	combine: (values) => values[0] ?? null,
});

type SeverityMap = Record<LspSeverity, CmDiagnostic['severity']>;

const SEVERITY_MAP: SeverityMap = {
	error: 'error',
	warning: 'warning',
	info: 'info',
	hint: 'hint',
};

/**
 * Translate an LSP diagnostic list into CM's `Diagnostic[]`. The only
 * non-trivial step is the range: LSP uses line/column (0-based UTF-16
 * units), CM uses absolute offsets. We compute the offset by
 * consulting the current doc — ranges outside the doc are clamped
 * rather than dropped, so a stale diagnostic from before a paste
 * still paints on a best-effort position.
 */
function toCmDiagnostics(view: EditorView, diagnostics: readonly LspDiagnostic[]): CmDiagnostic[] {
	const doc = view.state.doc;
	return diagnostics.map((d) => {
		const from = offsetFor(doc, d.range.start.line, d.range.start.character);
		const to = Math.max(from, offsetFor(doc, d.range.end.line, d.range.end.character));
		const tags: string[] = [];
		if (d.source !== null) {
			tags.push(d.source);
		}
		if (d.code !== null) {
			tags.push(d.code);
		}
		const suffix = tags.length > 0 ? ` [${tags.join(' ')}]` : '';
		return {
			from,
			to: from === to ? to + 1 : to,
			severity: SEVERITY_MAP[d.severity],
			message: d.message + suffix,
		};
	});
}

function offsetFor(doc: EditorView['state']['doc'], line: number, character: number): number {
	if (line < 0) {
		return 0;
	}
	if (line >= doc.lines) {
		return doc.length;
	}
	const lineInfo = doc.line(line + 1);
	const column = Math.min(character, lineInfo.length);
	return lineInfo.from + column;
}

/**
 * A `ViewPlugin`-shaped extension that keeps the editor's lint state
 * in sync with `workspace.diagnostics[path]`. Subscribes via an
 * `$effect`-equivalent: the returned updater is invoked from
 * `Editor.svelte`'s `$effect` block whenever the diagnostic map or
 * the active path changes.
 *
 * We return both the update callback *and* the CM extension list so
 * Editor.svelte can wire the `$effect` in its normal shape.
 */
export function lspDiagnosticsExtension(): Extension[] {
	// `lintGutter()` is what actually paints severity markers next
	// to the gutter; `setDiagnostics` is the state effect we dispatch
	// per update. Both live in `@codemirror/lint`.
	return [lintGutter()];
}

/**
 * Apply the latest diagnostics for `path` to `view`. Called from
 * Editor.svelte's `$effect(() => workspace.diagnostics)` block.
 */
export function applyDiagnostics(view: EditorView, diagnostics: readonly LspDiagnostic[]): void {
	const cm = toCmDiagnostics(view, diagnostics);
	view.dispatch(setDiagnostics(view.state, cm));
}

/**
 * Hover extension that resolves the current buffer path from the
 * facet. CM's built-in `hoverTooltip` already delays internally
 * until the pointer sits still for ~300ms, so the rate of LSP
 * requests tracks intentional hovers rather than mouse sweeps.
 */
export function lspHoverExtension(): Extension {
	return hoverTooltip(
		async (view, pos) => {
			const path = view.state.facet(filePathFacet);
			if (!path) {
				return null;
			}
			const languageId = lspLanguageFor(path);
			if (!languageId) {
				return null;
			}
			const position = positionFor(view, pos);
			if (!position) {
				return null;
			}
			let hover;
			try {
				hover = await ipc.lsp.hover(path, languageId, position);
			} catch {
				return null;
			}
			if (!hover) {
				return null;
			}
			const { from, to } = hoverRange(view, hover.range, pos);
			const tooltip: Tooltip = {
				pos: from,
				end: to,
				above: true,
				create: () => {
					const dom = document.createElement('div');
					dom.className = 'cm-lsp-hover';
					// `workspace.renderMarkdown` doesn't exist yet; the
					// backend has already normalised the contents to a
					// Markdown string, so plain text render is enough
					// for stage 1 (it preserves newlines and fenced
					// blocks the UI CSS styles). Swapping in markdown-it
					// is a one-liner when someone asks for rich hover.
					dom.textContent = hover.contents;
					return { dom };
				},
			};
			return tooltip;
		},
		{ hideOnChange: true },
	);
}

function positionFor(view: EditorView, offset: number) {
	const line = view.state.doc.lineAt(offset);
	return {
		line: line.number - 1,
		character: offset - line.from,
	};
}

function hoverRange(view: EditorView, range: LspDiagnostic['range'] | null, fallback: number) {
	if (!range) {
		return { from: fallback, to: fallback };
	}
	const from = offsetFor(view.state.doc, range.start.line, range.start.character);
	const to = offsetFor(view.state.doc, range.end.line, range.end.character);
	return { from, to };
}

/**
 * LSP-backed completion source, registered as an `override` on the
 * editor's existing `autocompletion()` extension. Fires only on
 * explicit invocation — `activateOnTyping: false` at the top level
 * keeps it off the typing path. The server decides what "relevant
 * here" means.
 */
export const lspCompletionSource: CompletionSource = async (
	context: CompletionContext,
): Promise<CompletionResult | null> => {
	const path = context.state.facet(filePathFacet);
	if (!path) {
		return null;
	}
	const languageId = lspLanguageFor(path);
	if (!languageId) {
		return null;
	}
	const line = context.state.doc.lineAt(context.pos);
	const position = {
		line: line.number - 1,
		character: context.pos - line.from,
	};
	let list;
	try {
		list = await ipc.lsp.completion(path, languageId, position);
	} catch {
		return null;
	}
	if (list.items.length === 0) {
		return null;
	}
	// `from` is the start of the word under the caret; CM uses it
	// to replace the prefix on accept. `$` is an identifier char
	// in JS; including it in the class avoids chopping the `$`
	// off a `$foo` completion.
	const word = context.matchBefore(/[\w$]+/);
	const from = word ? word.from : context.pos;
	// Build each option without the `undefined` branches so CM6's
	// `exactOptionalPropertyTypes` stays happy — CM wants the keys
	// absent rather than present-but-undefined. Writing each assign
	// explicitly (vs a spread pre-built object) keeps the compiler
	// narrow-tracking what's on the `Completion` type.
	return {
		from,
		to: context.pos,
		options: list.items.map((item) => {
			const option: Completion = {
				label: item.label,
				apply: item.insertText ?? item.label,
			};
			if (item.kind !== null) {
				option.type = item.kind;
			}
			if (item.detail !== null) {
				option.detail = item.detail;
			}
			if (item.documentation !== null) {
				option.info = item.documentation;
			}
			if (item.sortText !== null) {
				option.boost = sortScore(item.sortText);
			}
			return option;
		}),
		validFor: /^[\w$]*$/,
	};
};

/**
 * Convert an LSP `sortText` into a CM `boost`. LSP sort keys are
 * lexicographic (earlier is better); CM boost is numeric (higher is
 * better). We hash into a small negative range so the top-sorted
 * entries bubble up without drowning CM's own relevance scoring.
 */
function sortScore(sortText: string): number {
	// Convert the leading 4 chars into a descending numeric offset.
	let score = 0;
	for (let i = 0; i < Math.min(4, sortText.length); i++) {
		score = score * 256 + sortText.charCodeAt(i);
	}
	// Map to a gentle range (-99..0). Anything larger fights CM's
	// own ranking (label match quality, prefix length).
	return -(score % 100);
}
