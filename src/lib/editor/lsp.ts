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
import { setDiagnostics, type Action as CmAction, type Diagnostic as CmDiagnostic, lintGutter } from '@codemirror/lint';
import { Facet, type Extension } from '@codemirror/state';
import { EditorView, hoverTooltip, type Tooltip } from '@codemirror/view';
import type { Text } from '@codemirror/state';
import { coder } from '../coder.svelte';
import { ipc } from '../ipc';
import { frontendLog } from '../logs.svelte';
import { openExternalMarkdownLink, renderMarkdown } from '../markdown';
import {
	formatError,
	type LspCodeAction,
	type LspCompletionItem,
	type LspDiagnostic,
	type LspSeverity,
	type LspTextEdit,
} from '../protocol';
import { workspace } from '../state.svelte';
import { lspLanguageFor } from './lspLanguage';
import { applyWorkspaceEdit } from './lspWorkspaceEdit';

/** Diagnostic-logs source for everything autocomplete-related.
 * The user explicitly asked to see Ctrl+Space breadcrumbs here so
 * "did the keybinding fire?" vs. "did the LSP answer empty?" is
 * one click away. */
const COMPLETION_LOG_SOURCE = 'editor.completion';

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

type ProducerDiagnostic = {
	d: LspDiagnostic;
	producer: string;
};

/** Per-(path, diagnostic) cached `LspCodeAction[]`. The lint
 * tooltip needs `Diagnostic.actions` populated synchronously when
 * it opens, but the LSP fetch is async; we prime this map on each
 * `applyDiagnostics` call (one IPC per diagnostic, in parallel)
 * and re-dispatch with full actions once it lands. The cache
 * survives across re-dispatches so a fresh
 * `applyDiagnostics(view, samePath)` after, say, a status-pill
 * refresh doesn't repaint with empty action lists for one frame.
 *
 * Keys are derived from the diagnostic itself rather than its
 * memory identity — diagnostics are full-replaced on every server
 * publish, so the same logical "while(1) is constant" entry is a
 * fresh object on every keystroke, but its `(producer, range,
 * code)` triple stays stable until the user actually edits the
 * line. That triple is what we key on.
 *
 * Module-level (not per-view) so the diff-view editor and the
 * main editor share the cache when looking at the same file.
 */
const codeActionCache = new Map<string, Map<string, LspCodeAction[]>>();

/** Generation counter that lets late prefetches drop their result
 * when a newer `applyDiagnostics` has already fired. Each call
 * bumps it; the in-flight prefetch reads it once at start and
 * discards everything if it doesn't match at completion. Avoids
 * the "switched files mid-fetch, old file's actions painted on
 * new file's diagnostics" race. */
let activeFetchGen = 0;

function diagnosticKey(d: LspDiagnostic, producer: string): string {
	const code = d.code ?? '';
	// First line of the message disambiguates two diagnostics
	// that share a range + code (rare, but oxlint can emit
	// e.g. two `typescript-eslint(no-unused-vars)` for two
	// adjacent identifiers on the same line). 80 chars is plenty
	// — anything past that is rule-chatter the tooltip already
	// truncates.
	const msg = d.message.split('\n')[0]?.slice(0, 80) ?? '';
	return `${producer}|${d.range.start.line}:${d.range.start.character}|${code}|${msg}`;
}

/**
 * Translate an LSP diagnostic list into CM's `Diagnostic[]`. The only
 * non-trivial step is the range: LSP uses line/column (0-based UTF-16
 * units), CM uses absolute offsets. We compute the offset by
 * consulting the current doc — ranges outside the doc are clamped
 * rather than dropped, so a stale diagnostic from before a paste
 * still paints on a best-effort position.
 */
function toCmDiagnostics(
	view: EditorView,
	diagnostics: readonly ProducerDiagnostic[],
	path: string | null,
): CmDiagnostic[] {
	const doc = view.state.doc;
	const cache = path === null ? null : codeActionCache.get(path);
	return diagnostics.map(({ d, producer }) => {
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
		const cachedActions = cache === null ? undefined : cache?.get(diagnosticKey(d, producer));
		return {
			from,
			to: from === to ? to + 1 : to,
			severity: SEVERITY_MAP[d.severity],
			message: d.message + suffix,
			actions: buildActions(d, producer, path, cachedActions ?? []),
		};
	});
}

/** Build the action list shown in the lint tooltip for one
 * diagnostic. LSP-provided quickfixes (cached or just-fetched)
 * come first — autofix, "disable rule on this line", "disable
 * rule for this file" — and our always-on "Fix in coder" entry
 * caps the list so the user has an out even when the linter has
 * no programmatic fix to offer. */
function buildActions(
	d: LspDiagnostic,
	producer: string,
	path: string | null,
	cached: readonly LspCodeAction[],
): readonly CmAction[] {
	const actions: CmAction[] = [];
	for (const ca of cached) {
		actions.push({
			name: ca.title,
			apply: () => {
				void runQuickFix(ca);
			},
		});
	}
	if (path !== null) {
		actions.push({
			name: 'Fix in coder',
			apply: (view, from, to) => {
				const text = view.state.doc.sliceString(from, to);
				coder.fixDiagnosticInCoder({
					path,
					startLine: d.range.start.line + 1,
					endLine: d.range.end.line + 1,
					text,
					code: d.code,
					source: d.source,
					message: d.message,
				});
			},
		});
	}
	return actions;
}

async function runQuickFix(ca: LspCodeAction): Promise<void> {
	try {
		const result = await applyWorkspaceEdit(ca.edit);
		for (const f of result.failures) {
			workspace.flash(`Quick fix: failed to update ${f.path}: ${f.error}`);
		}
		const total = result.openCount + result.closedCount;
		if (total === 0 && result.failures.length === 0) {
			// Server returned an edit that targeted only files
			// we couldn't reach (URIs outside the workspace
			// root, dropped by the translation layer). Surface
			// quietly rather than silently no-oping; the user
			// just clicked something and deserves a hint that
			// it didn't take.
			workspace.flash(`Quick fix: nothing to apply for "${ca.title}"`);
		}
	} catch (err) {
		const msg = err instanceof Error ? err.message : String(err);
		workspace.flash(`Quick fix failed: ${msg}`);
	}
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
 * LSP position → CM document offset against `view`'s current doc.
 * Clamps `line` past EOF to the doc length and `character` past
 * line end to that line's end, so a goto-def that points into a
 * since-shrunken buffer lands at the closest valid position instead
 * of crashing CM with an out-of-bounds dispatch. Exported because
 * both the editor and the diff view consume `pendingJumps` and
 * need the same conversion.
 */
export function offsetForLspPosition(view: EditorView, position: { line: number; character: number }): number {
	return offsetFor(view.state.doc, position.line, position.character);
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
 * Apply the latest diagnostics for the editor's current path to
 * `view`. Called from `Editor.svelte` and `DiffView.svelte` from a
 * reactive `$effect(() => workspace.diagnosticsByProducer)` block;
 * `perProducer` is the per-file slice of that map (so each
 * diagnostic carries its origin server's slot key, which the
 * lint tooltip needs to ask the right server for quickfixes).
 *
 * Two-phase apply for the action list: the synchronous dispatch
 * uses whatever quick-fixes are already cached (typically empty
 * on first apply), then a background prefetch fans out one
 * `lsp_code_action` IPC per diagnostic and re-dispatches with the
 * fetched actions populated. A generation counter guards against
 * stale prefetches landing after a newer apply (e.g. user
 * switched files mid-fetch). The `Fix in coder` action is always
 * present client-side regardless — that's the user's escape
 * hatch when the linter has no programmatic fix to offer.
 */
export function applyDiagnostics(
	view: EditorView,
	perProducer: ReadonlyMap<string, readonly LspDiagnostic[]> | null,
): void {
	const path = view.state.facet(filePathFacet);
	const flat: ProducerDiagnostic[] = [];
	if (perProducer !== null) {
		for (const [producer, list] of perProducer) {
			for (const d of list) {
				flat.push({ d, producer });
			}
		}
	}
	const cm = toCmDiagnostics(view, flat, path);
	view.dispatch(setDiagnostics(view.state, cm));
	if (path !== null && flat.length > 0) {
		activeFetchGen += 1;
		const myGen = activeFetchGen;
		void prefetchCodeActions(view, flat, path, myGen);
	}
}

async function prefetchCodeActions(
	view: EditorView,
	flat: readonly ProducerDiagnostic[],
	path: string,
	gen: number,
): Promise<void> {
	const cache = codeActionCache.get(path) ?? new Map<string, LspCodeAction[]>();
	let changed = false;
	const tasks = flat.map(async ({ d, producer }) => {
		const key = diagnosticKey(d, producer);
		if (cache.has(key)) {
			return;
		}
		try {
			const actions = await ipc.lsp.codeAction(path, producer, d.range, d);
			if (gen !== activeFetchGen) {
				return;
			}
			cache.set(key, actions);
			changed = true;
		} catch (err) {
			// Cache the empty list so we don't retry on every
			// re-render. A fresh server publish (next edit) will
			// produce new keys naturally; transient failures
			// during a server restart land here and the user just
			// sees the always-on `Fix in coder` action until then.
			frontendLog('editor.diagnostics', 'warn', `code-action prefetch failed: ${formatError(err)}`);
			cache.set(key, []);
		}
	});
	await Promise.all(tasks);
	if (gen !== activeFetchGen) {
		return;
	}
	codeActionCache.set(path, cache);
	if (!changed) {
		return;
	}
	if (view.state.facet(filePathFacet) !== path) {
		// User swapped files between prefetch start and
		// completion — the editor's diagnostics state now belongs
		// to a different path. Drop without dispatch; the new
		// path's `applyDiagnostics` already fired on the swap.
		return;
	}
	const cm = toCmDiagnostics(view, flat, path);
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
			// Pre-render here (async) so the synchronous `create`
			// callback can hand CM the fully-baked DOM. Rendering
			// inside `create` would leave a visible empty popover
			// for a frame while grammars load on first use.
			let html: string;
			try {
				html = await renderMarkdown(hover.contents);
			} catch {
				return null;
			}
			const { from, to } = hoverRange(view, hover.range, pos);
			const tooltip: Tooltip = {
				pos: from,
				end: to,
				above: true,
				create: () => {
					const dom = document.createElement('div');
					// `markdown-body` picks up the shared Markdown CSS
					// from `src/styles.css` (headings, lists, tables,
					// blockquote, `<pre>` code-block chrome). The
					// `cm-lsp-hover` class adds the tooltip-specific
					// caps (max-width, max-height, padding) from
					// `editor/theme.ts`.
					dom.className = 'cm-lsp-hover markdown-body';
					dom.innerHTML = html;
					// Intercept every anchor click so MDN refs, doc
					// links, `@link` crossrefs etc. open in the OS
					// browser via the Tauri opener plugin instead of
					// navigating the IDE window — a raw anchor click
					// inside the webview would replace the whole IDE
					// with the target page. Delegated on the tooltip
					// root so new anchors added later (none today,
					// but cheap insurance) still get caught.
					dom.addEventListener('click', (ev) => {
						const t = ev.target;
						if (!(t instanceof Element)) {
							return;
						}
						const anchor = t.closest('a');
						if (!anchor) {
							return;
						}
						ev.preventDefault();
						const href = anchor.getAttribute('href');
						if (!href) {
							return;
						}
						// Hover popovers have no meaningful "current
						// file" to resolve relative links against,
						// and the popover itself isn't long enough
						// to benefit from in-page anchors. Anything
						// that isn't an external link we swallow.
						openExternalMarkdownLink(href);
					});
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

/** End offset of the identifier the caret sits inside, scanning
 *  forward from `pos` through `[\w$]` characters. Returns `pos`
 *  unchanged when the caret isn't followed by word characters. `$`
 *  is included for the same reason as in the completion source's
 *  `matchBefore` — it's an identifier char in JS. */
function wordEndAfter(doc: Text, pos: number): number {
	const line = doc.lineAt(pos);
	const trailing = /^[\w$]+/.exec(line.text.slice(pos - line.from));
	return trailing ? pos + trailing[0].length : pos;
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
		frontendLog(
			COMPLETION_LOG_SOURCE,
			'debug',
			`invoked (explicit=${context.explicit}) — no file path bound, skipping`,
		);
		return null;
	}
	const languageId = lspLanguageFor(path);
	if (!languageId) {
		frontendLog(
			COMPLETION_LOG_SOURCE,
			'debug',
			`invoked (explicit=${context.explicit}, path=${path}) — no LSP language id for this extension, skipping`,
		);
		return null;
	}
	const line = context.state.doc.lineAt(context.pos);
	const position = {
		line: line.number - 1,
		character: context.pos - line.from,
	};
	frontendLog(
		COMPLETION_LOG_SOURCE,
		'info',
		`invoked (explicit=${context.explicit}, path=${path}, lang=${languageId}, line=${position.line}, char=${position.character}) → calling lsp_completion`,
	);
	let list;
	try {
		list = await ipc.lsp.completion(path, languageId, position);
	} catch (err) {
		frontendLog(COMPLETION_LOG_SOURCE, 'error', `lsp_completion threw: ${formatError(err)}`);
		return null;
	}
	frontendLog(
		COMPLETION_LOG_SOURCE,
		'info',
		`lsp_completion returned ${list.items.length} item${list.items.length === 1 ? '' : 's'} (isIncomplete=${list.isIncomplete})`,
	);
	if (list.items.length === 0) {
		// Returning `null` here lets CM dismiss the popover; the
		// log line above is the only signal the user gets that
		// "Ctrl+Space did fire, the server just had nothing to
		// offer at this position".
		return null;
	}
	// `from` is the start of the word under the caret; CM uses it
	// to replace the prefix on accept. `$` is an identifier char
	// in JS; including it in the class avoids chopping the `$`
	// off a `$foo` completion.
	const word = context.matchBefore(/[\w$]+/);
	const from = word ? word.from : context.pos;
	// Extend the replace range past any word characters that sit
	// *after* the caret so accepting an item mid-identifier
	// rewrites the whole word instead of inserting into it (caret
	// after "Ob" in "ObjectId" must yield "ObjectId", not
	// "ObjectIdjectId").
	const to = wordEndAfter(context.state.doc, context.pos);
	// Build each option without the `undefined` branches so CM6's
	// `exactOptionalPropertyTypes` stays happy — CM wants the keys
	// absent rather than present-but-undefined. Writing each assign
	// explicitly (vs a spread pre-built object) keeps the compiler
	// narrow-tracking what's on the `Completion` type.
	return {
		from,
		to,
		options: list.items.map((item) => {
			const option: Completion = {
				label: item.label,
				apply: (view, _completion, applyFrom, applyTo) => {
					applyLspCompletion(view, item, languageId, applyFrom, applyTo);
				},
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
 * Commit one LSP completion item: dispatch the primary text
 * insertion immediately, then chase the auto-import block via
 * `completionItem/resolve` if the server lazy-resolves it.
 *
 * Two-transaction shape rather than one because resolve has
 * latency (`tsgo` is ~30ms cold, `rust-analyzer` can be 100ms+
 * for a fresh symbol). Forcing the user to wait on resolve
 * before the typed token even appears would make accepts feel
 * laggy. The trade-off is two undo units instead of one — but
 * the import line landing a beat after the identifier is the
 * standard LSP-client UX (VS Code, Helix, Zed all do it this
 * way) and the alternative (everyone waits for the import
 * round-trip) is worse.
 */
function applyLspCompletion(
	view: EditorView,
	item: LspCompletionItem,
	languageId: string,
	applyFrom: number,
	applyTo: number,
): void {
	const primary = primaryEditFor(item, view.state.doc, applyFrom, applyTo);
	view.dispatch({
		changes: { from: primary.from, to: primary.to, insert: primary.insert },
		selection: { anchor: primary.from + primary.insert.length },
	});
	if (item.resolveToken !== null) {
		// Resolve replaces additionalTextEdits — never merge what
		// we already have with what comes back, just trust the
		// resolved item. A non-empty initial list paired with a
		// resolve token is rare (most servers ship one or the
		// other); when it happens, deferring keeps us aligned
		// with VS Code semantics so a server that "fills in"
		// resolves consistently.
		void resolveAndApplyAdditional(view, item.resolveToken, languageId, item.label);
		return;
	}
	if (item.additionalTextEdits.length > 0) {
		applyAdditionalEdits(view, item.additionalTextEdits);
	}
}

/** Where the primary insertion lands. Honours an explicit
 *  `textEdit` range if the server gave one (e.g. completing
 *  `foo.bar` from inside `foo` — the server replaces the whole
 *  dotted span, not just the prefix-matched suffix); otherwise
 *  falls back to "replace the matchBefore-detected word with
 *  `insertText` / `label`". */
function primaryEditFor(
	item: LspCompletionItem,
	doc: Text,
	fallbackFrom: number,
	fallbackTo: number,
): { from: number; to: number; insert: string } {
	if (item.textEdit !== null) {
		const start = offsetFor(doc, item.textEdit.range.start.line, item.textEdit.range.start.character);
		const end = offsetFor(doc, item.textEdit.range.end.line, item.textEdit.range.end.character);
		// The server's range ends at the caret (we advertise
		// `insert_replace_support: false`), so on its own it leaves
		// any trailing word characters behind. `fallbackTo` carries
		// the word-end the completion source computed; take whichever
		// reaches further so a mid-identifier accept rewrites the
		// whole word.
		return { from: start, to: Math.max(end, fallbackTo), insert: item.textEdit.newText };
	}
	return { from: fallbackFrom, to: fallbackTo, insert: item.insertText ?? item.label };
}

/** Apply a list of LSP text edits as one CM transaction. LSP
 *  guarantees edits inside a single document don't overlap, so we
 *  just sort by `from` ascending — CM accepts a sorted array of
 *  changes as a single transaction and updates any in-flight
 *  selection (the caret the primary edit just placed) for the
 *  inserted import line at line 0. */
function applyAdditionalEdits(view: EditorView, edits: readonly LspTextEdit[]): void {
	const doc = view.state.doc;
	const changes = edits
		.map((e) => {
			const from = offsetFor(doc, e.range.start.line, e.range.start.character);
			const to = offsetFor(doc, e.range.end.line, e.range.end.character);
			return { from, to, insert: e.newText };
		})
		.toSorted((a, b) => a.from - b.from);
	if (changes.length === 0) {
		return;
	}
	view.dispatch({ changes });
}

async function resolveAndApplyAdditional(
	view: EditorView,
	token: string,
	languageId: string,
	label: string,
): Promise<void> {
	let resolved: LspCompletionItem;
	try {
		resolved = await ipc.lsp.completionResolve(languageId, token);
	} catch (err) {
		frontendLog(COMPLETION_LOG_SOURCE, 'error', `completionItem/resolve threw for "${label}": ${formatError(err)}`);
		return;
	}
	if (resolved.additionalTextEdits.length === 0) {
		return;
	}
	frontendLog(
		COMPLETION_LOG_SOURCE,
		'info',
		`completionItem/resolve for "${label}" applied ${resolved.additionalTextEdits.length} additional edit${resolved.additionalTextEdits.length === 1 ? '' : 's'}`,
	);
	applyAdditionalEdits(view, resolved.additionalTextEdits);
}

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
