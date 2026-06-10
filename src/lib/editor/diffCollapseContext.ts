// Annotates `@codemirror/merge`'s `… N unchanged lines` collapse
// placeholders with the enclosing definition (function / class /
// method / etc.) the way GitHub shows the symbol after the `@@`
// hunk header. Lets a reviewer tell "which function is this hunk
// in?" without expanding the fold.
//
// Deliberately heuristic, not LSP-backed: the review tab keeps the
// LSP broker silent for files the user only scrolls past (see
// `ReviewSection.svelte`'s lazy goto-def attach), so resolving the
// enclosing symbol through `documentSymbol` would defeat that and
// add async churn. Instead we walk the document text upward from
// the first line after the fold, looking for the nearest
// definition-looking line at a strictly smaller indent. That's
// accurate on properly-indented code (every language we ship) and
// silently produces no label when it can't find one — never wrong,
// occasionally absent.
//
// Implementation: a `ViewPlugin` that, after each layout, finds the
// package's own `.cm-collapsedLines` widgets (left intact so its
// click-to-expand + sibling-pane sync keep working) and appends a
// muted context span. We don't rebuild the collapse decorations
// ourselves — owning the expand interaction would mean reaching
// into the package's internal `mergeConfig` facet for the sibling
// sync, which is fragile. Post-decorating the existing DOM is the
// lower-risk seam.

import { EditorView, ViewPlugin, type ViewUpdate } from '@codemirror/view';
import type { Text } from '@codemirror/state';

const CONTEXT_CLASS = 'cm-collapseContext';
// Marks a widget we've already annotated this layout pass so a
// no-op `ViewUpdate` (selection move, focus) doesn't re-walk every
// fold.
const STAMP = 'data-moon-collapse-ctx';

// Definition-looking lines, language-agnostic across our stack
// (TS / JS / Rust / Go / Python / Svelte script / CSS-ish blocks).
// Matched against the line's trimmed text. The intent is "a line
// that introduces a named, indentation-defining scope" — we accept
// false negatives (no label) far more readily than false positives
// (a misleading label).
const DEFINITION = new RegExp(
	[
		// `export`/`pub`/`async`/`default`/`static`/`public` … prefixes,
		// then a scope-introducing keyword.
		'^(?:export\\s+)?(?:default\\s+)?(?:pub(?:\\([^)]*\\))?\\s+)?(?:async\\s+)?(?:static\\s+)?(?:public\\s+|private\\s+|protected\\s+)?',
		'(?:function|class|struct|enum|trait|impl|interface|type|fn|func|def|module|mod|namespace|abstract\\s+class)\\b',
	].join(''),
);

// A method / property signature inside a class or object body:
// `name(args) {`, `name = (args) => {`, `get name() {`. Requires a
// trailing brace / arrow so we don't latch onto every call
// expression. Excludes obvious control-flow keywords.
const SIGNATURE =
	/^(?!(?:if|for|while|switch|catch|else|return|match)\b)[\w$#.<>]+\s*(?:[:=]\s*)?(?:async\s+)?\([^)]*\)\s*(?:=>\s*)?\{?\s*$/;

function looksLikeDefinition(trimmed: string): boolean {
	if (trimmed.length === 0) {
		return false;
	}
	return DEFINITION.test(trimmed) || SIGNATURE.test(trimmed);
}

// Leading-whitespace width, counting a tab as one column (we only
// compare relative depth, so the tab/space width never matters as
// long as the file is internally consistent — which "properly
// formatted" guarantees).
function indentOf(text: string): number {
	let n = 0;
	while (n < text.length && (text[n] === ' ' || text[n] === '\t')) {
		n++;
	}
	return n;
}

/**
 * Walk upward from `fromLine` (1-based, the first visible line
 * *after* the collapsed region) looking for the nearest enclosing
 * definition: the closest preceding line that looks like a
 * definition and sits at a strictly smaller indent than the line
 * the fold gives way to. Returns the trimmed signature, or `null`
 * when nothing convincing is found.
 */
export function enclosingSymbol(doc: Text, fromLine: number): string | null {
	if (fromLine < 1 || fromLine > doc.lines) {
		return null;
	}
	// Reference indent: the indent of the first non-blank line at or
	// after the fold. The enclosing scope must be shallower than
	// this. Using the post-fold line (rather than the fold's own
	// lines) matches GitHub: the label describes the code you're
	// about to read.
	let refIndent = -1;
	for (let n = fromLine; n <= doc.lines; n++) {
		const text = doc.line(n).text;
		if (text.trim().length > 0) {
			refIndent = indentOf(text);
			break;
		}
	}
	if (refIndent <= 0) {
		// Top-level code after the fold has no enclosing scope to
		// name, and a negative ref means the rest of the doc is
		// blank.
		return null;
	}
	let bestIndent = refIndent;
	// Bound the climb so a pathological file can't make this O(doc).
	const limit = Math.max(1, fromLine - 4000);
	for (let n = fromLine - 1; n >= limit; n--) {
		const text = doc.line(n).text;
		const trimmed = text.trim();
		if (trimmed.length === 0) {
			continue;
		}
		const indent = indentOf(text);
		if (indent >= bestIndent) {
			continue;
		}
		if (looksLikeDefinition(trimmed)) {
			return summarise(trimmed);
		}
		// Tighten the bar: once we step out to a shallower indent
		// that *isn't* a definition (e.g. a bare `{` continuation or
		// an attribute), keep climbing but never re-accept a deeper
		// line.
		bestIndent = indent;
		if (bestIndent === 0) {
			break;
		}
	}
	return null;
}

// Trim a signature to a compact label: drop a trailing opening
// brace / colon, collapse interior whitespace, and clamp length.
function summarise(trimmed: string): string {
	let s = trimmed
		.replace(/\s*\{\s*$/, '')
		.replace(/\s+/g, ' ')
		.trim();
	const MAX = 80;
	if (s.length > MAX) {
		s = s.slice(0, MAX - 1) + '…';
	}
	return s;
}

function annotate(view: EditorView): void {
	const widgets = view.dom.querySelectorAll<HTMLElement>('.cm-collapsedLines');
	for (const widget of widgets) {
		if (widget.hasAttribute(STAMP)) {
			continue;
		}
		const pos = view.posAtDOM(widget);
		// The widget replaces the collapsed block; `posAtDOM` lands at
		// its start. The first line after the fold is one past the
		// block's end line — find it via the line at the widget's end.
		const startLine = view.state.doc.lineAt(pos);
		// The fold spans N lines starting at `startLine`; the first
		// visible line after it is `startLine + N`.
		const afterFoldLine = startLine.number + countCollapsedLines(widget);
		const symbol = enclosingSymbol(view.state.doc, afterFoldLine);
		widget.setAttribute(STAMP, '1');
		if (symbol === null) {
			continue;
		}
		const span = document.createElement('span');
		span.className = CONTEXT_CLASS;
		span.textContent = symbol;
		widget.appendChild(span);
	}
}

// The widget's text is `… N unchanged lines`; pull N back out so we
// know how far the fold reaches. Falls back to 0 (so we look at the
// line right after the widget's start) if the phrase changes.
function countCollapsedLines(widget: HTMLElement): number {
	// childNodes[0] is the package's own text node; reading it
	// avoids counting our appended span.
	const first = widget.childNodes[0];
	const text = first?.textContent ?? widget.textContent ?? '';
	const m = text.match(/(\d+)/);
	if (m === null || m[1] === undefined) {
		return 0;
	}
	return Number.parseInt(m[1], 10);
}

/**
 * View plugin that decorates the merge package's unchanged-region
 * collapse placeholders with their enclosing definition. Install on
 * either or both panes of a `MergeView`.
 */
export const diffCollapseContextExtension = ViewPlugin.fromClass(
	class {
		constructor(view: EditorView) {
			// Defer to next frame: the package's collapse widgets are
			// block decorations rendered during this same layout, so
			// their DOM may not be queryable synchronously on
			// construction.
			requestAnimationFrame(() => annotate(view));
		}
		update(update: ViewUpdate) {
			if (update.docChanged || update.viewportChanged || update.geometryChanged) {
				requestAnimationFrame(() => annotate(update.view));
			}
		}
	},
);
