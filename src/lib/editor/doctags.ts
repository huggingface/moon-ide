import type { SyntaxNodeRef } from '@lezer/common';
import { ensureSyntaxTree, syntaxTree } from '@codemirror/language';
import type { EditorState, Extension, Range } from '@codemirror/state';
import { Decoration, EditorView, ViewPlugin, type DecorationSet, type ViewUpdate } from '@codemirror/view';

// Doc-comment tag highlighting: JSDoc `@param` / `{@link Target}` in JS/TS,
// rustdoc `# Section` headings and `[Foo]` intra-doc links in Rust, Go doc
// links (`[Name]`, `[pkg.Name]`) and `# Heading`, and Sphinx-style fields
// (`:param x:`) / roles (`:func:`foo``) in Python docstrings. The grammars
// paint whole comments (or docstrings) with one flat style, so doc tags
// read as a grey blob; this decoration layer adds the emphasis without
// touching the grammars or the highlight style.
//
// The decoration is gated on the *syntax tree*, and every flavor has its
// own "is this a doc comment" rule, because the signal differs:
//
//   - jsdoc / rustdoc — marker bytes (`/**` vs `/*`, `///` vs `//`)
//   - godoc — adjacency: Go marks doc comments purely by position, so a
//     comment counts when its next tree sibling is a declaration
//   - pydoc — docstrings are *string* nodes in the first-statement
//     position of a module / class / function body
//
// That gating is what keeps TS `@decorator` syntax, annotations, emails in
// prose, and bracketed chatter in ordinary comments from lighting up.
//
// Ctrl/Cmd-click *jumps* through doc-link targets with no extra wiring:
// the goto-definition extension already probes any word under the
// modifier (comment or not), and the servers answer — tsgo resolves
// `definition` on the target inside `{@link Widget.spin}` (the method, if
// the click lands on `spin`), rust-analyzer on `[Widget]`. gopls and ty
// are expected to behave the same or return null (no underline, no jump)
// — the gesture degrades to nothing rather than breaking.

export type DoctagKind = 'tag' | 'link' | 'heading';

export type DoctagRange = {
	from: number;
	to: number;
	kind: DoctagKind;
};

// Class per kind; colors resolve via the `--m-syntax-*` CSS variables in
// `src/styles.css` so a theme flip repaints for free.
const MARKS: Record<DoctagKind, Decoration> = {
	tag: Decoration.mark({ class: 'cm-doctag' }),
	link: Decoration.mark({ class: 'cm-doctag-link' }),
	heading: Decoration.mark({ class: 'cm-doctag-heading' }),
};

/** Which doc-comment dialect to scan for. One per language branch. */
export type DoctagFlavor = 'jsdoc' | 'rustdoc' | 'godoc' | 'pydoc';

/**
 * Collect doctag ranges for every doc-comment node intersecting
 * `[from, to]`. Pure over `state` — the ViewPlugin calls it with the
 * visible ranges, tests call it with the whole document.
 */
export function collectDoctagRanges(state: EditorState, from: number, to: number, flavor: DoctagFlavor): DoctagRange[] {
	const ranges: DoctagRange[] = [];
	const tree = ensureSyntaxTree(state, to, 50);
	if (!tree) {
		return ranges;
	}
	tree.iterate({
		from,
		to,
		enter: (node) => {
			switch (flavor) {
				case 'pydoc': {
					if (node.name === 'String' && isPythonDocstring(node.node)) {
						scanLines(state, node.from, node.to, 'pydoc', from, to, ranges);
					}
					return;
				}
				case 'godoc': {
					if (isCommentNode(node.name) && isGodocComment(node.node)) {
						scanLines(state, node.from, node.to, 'godoc', from, to, ranges);
					}
					return;
				}
				case 'rustdoc': {
					if (isCommentNode(node.name) && rustdocMarker(state, node.from, node.to) !== null) {
						scanLines(state, node.from, node.to, 'rustdoc', from, to, ranges);
					}
					return;
				}
				case 'jsdoc': {
					if (!isCommentNode(node.name)) {
						return;
					}
					const marker = jsdocMarker(state, node.from, node.to);
					if (marker === 'block') {
						scanLines(state, node.from, node.to, 'jsdoc', from, to, ranges);
					} else if (marker === 'line') {
						scanLines(state, node.from, node.to, 'jsline', from, to, ranges);
					}
					return;
				}
			}
		},
	});
	// Ranges are pushed per-scan out of document order (inline tags
	// before block tags within a line); the consumer wants them sorted.
	ranges.sort((a, b) => a.from - b.from || a.to - b.to);
	return ranges;
}

function isCommentNode(name: string): boolean {
	return name.toLowerCase().includes('comment');
}

// Marker prefix of a rustdoc line: the `///` / `//!` opener (or a `*`
// continuation inside a `/** */` block doc) plus padding.
const RUSTDOC_PREFIX_RE = /^\s*(?:\/\/(?:\/|!)+|\/\*(?:\*|!)?|\*)[ \t]*/;

// rustdoc resolves *every* bracketed span as an intra-doc link attempt
// (and warns when it can't), so a generous match mirrors the tool's own
// semantics. Space-padded inners (`[ 1, 2 ]` array prose) are skipped.
const RUSTDOC_LINK_RE = /\[[^[\]\n]+\]/g;

function rustdocMarker(state: EditorState, commentFrom: number, commentTo: number): 'doc' | null {
	const start = state.doc.sliceString(commentFrom, Math.min(commentFrom + 3, commentTo));
	if (start === '///' || start === '//!' || start.startsWith('/**') || start.startsWith('/*!')) {
		return 'doc';
	}
	// Ordinary `//` chatter and plain `/* */` banners carry no doc
	// semantics in Rust — the `@words` and `[brackets]` in them are prose.
	return null;
}

function jsdocMarker(state: EditorState, commentFrom: number, commentTo: number): 'block' | 'line' | null {
	const start = state.doc.sliceString(commentFrom, Math.min(commentFrom + 3, commentTo));
	if (start.startsWith('/**')) {
		return 'block';
	}
	// Plain `//` lines can still carry the TS pragmas; the jsline scan
	// allowlists those so `@handle` mentions stay plain.
	if (start.startsWith('//')) {
		return 'line';
	}
	return null;
}

// True when the comment is a doc comment by Go's position rule: a
// comment *block* counts when its last line sits directly before a
// declaration. Go has no doc marker bytes — gopls applies the same
// adjacency rule — so `// chatter` mid-block and trailing `// notes`
// after the last statement stay plain. Each LineComment in a block
// chains to the next via `nextSibling`, so we walk forward through
// comment siblings and check what follows the block.
function isGodocComment(node: SyntaxNodeRef): boolean {
	let last = node.node;
	for (;;) {
		const next = last.nextSibling;
		if (!next) {
			return false;
		}
		if (!isCommentNode(next.name)) {
			return true;
		}
		last = next;
	}
}

// True when the String node is a docstring: the sole expression of an
// ExpressionStatement that is the first such statement of a module,
// class, or function body (PEP 257's position rule). Lezer hands out
// fresh `SyntaxNode` wrappers per `getChild` call, so identity never
// compares equal — offsets are the reliable witness, and a Body's
// first ExpressionStatement is trivially the one with the smallest
// `from`.
function isPythonDocstring(node: SyntaxNodeRef): boolean {
	const parent = node.node.parent;
	if (!parent || parent.name !== 'ExpressionStatement') {
		return false;
	}
	const grandparent = parent.parent;
	if (!grandparent || (grandparent.name !== 'Body' && grandparent.name !== 'Script')) {
		return false;
	}
	return grandparent.getChild('ExpressionStatement')?.from === parent.from;
}

// Walk the lines of one comment / docstring node, clipped at both the
// node boundary and the requested window, and dispatch to the scanner.
function scanLines(
	state: EditorState,
	nodeFrom: number,
	nodeTo: number,
	dialect: 'jsdoc' | 'jsline' | 'rustdoc' | 'godoc' | 'pydoc',
	from: number,
	to: number,
	ranges: DoctagRange[],
): void {
	let pos = Math.max(nodeFrom, from);
	const end = Math.min(nodeTo, to);
	while (pos <= end) {
		const line = state.doc.lineAt(pos);
		const lFrom = Math.max(line.from, nodeFrom);
		const lTo = Math.min(line.to, end);
		const text = line.text.slice(lFrom - line.from, lTo - line.from);
		if (dialect === 'jsdoc') {
			scanJsdoc(text, lFrom, ranges);
		} else if (dialect === 'jsline') {
			scanJsLinePragma(text, lFrom, ranges);
		} else if (dialect === 'rustdoc') {
			scanRustdoc(text, lFrom, ranges);
		} else if (dialect === 'godoc') {
			scanGodoc(text, lFrom, ranges);
		} else {
			scanPydoc(text, lFrom, ranges);
		}
		pos = line.to + 1;
	}
}

// `{@link Target Label}` (and its `linkcode` / `linkplain` siblings).
// The sigil reads as a tag; for the link variants the first
// identifier-ish run after it reads as a link target. Other inline tags
// (`{@code}`, `{@example}`, …) get the sigil only — their payloads
// aren't symbol references.
const INLINE_TAG_RE = /\{@([\w-]+)(?:[ \t]+([\w$.#![][\w$.#![\]-]*))?/g;
const LINK_TAG_NAMES = new Set(['link', 'linkcode', 'linkplain']);

// Word chars after `@` in a block tag. `$` for JS-flavored names, `-`
// for compound ones (`@suppress-next-line`).
const TAG_WORD_RE = /[\w$-]+/y;

function scanJsdoc(text: string, base: number, ranges: DoctagRange[]): void {
	let m: RegExpExecArray | null;
	INLINE_TAG_RE.lastIndex = 0;
	while ((m = INLINE_TAG_RE.exec(text)) !== null) {
		const name = m[1] ?? '';
		const sigilEnd = m.index + 2 + name.length;
		ranges.push({ from: base + m.index, to: base + sigilEnd, kind: 'tag' });
		const target = m[2];
		if (target && LINK_TAG_NAMES.has(name)) {
			const targetEnd = m.index + m[0].length;
			ranges.push({ from: base + targetEnd - target.length, to: base + targetEnd, kind: 'link' });
		}
	}
	// Block tags: any `@word` at a token boundary. Unknown tags light up
	// too — JSDoc dialects are legion, and a stale allowlist would hide
	// real tags the grammar doesn't know.
	let i = 0;
	for (;;) {
		const at = text.indexOf('@', i);
		if (at < 0) {
			return;
		}
		i = at + 1;
		const prev = at > 0 ? (text[at - 1] ?? '') : '';
		// `{@…` belongs to the inline pass above; a word char before `@`
		// makes it an email / keypath, not a tag.
		if (prev === '{' || /[\w$]/.test(prev)) {
			continue;
		}
		TAG_WORD_RE.lastIndex = at + 1;
		const word = TAG_WORD_RE.exec(text);
		if (!word) {
			continue;
		}
		ranges.push({ from: base + at, to: base + at + 1 + word[0].length, kind: 'tag' });
		i = at + 1 + word[0].length;
	}
}

// In a plain `//` line comment everything is prose except the TS
// pragmas — an allowlist keeps `@handle` mentions and the like from
// lighting up.
const TS_PRAGMA_RE = /@ts-(?:check|expect-error|ignore|nocheck)\b/g;

function scanJsLinePragma(text: string, base: number, ranges: DoctagRange[]): void {
	TS_PRAGMA_RE.lastIndex = 0;
	let m: RegExpExecArray | null;
	while ((m = TS_PRAGMA_RE.exec(text)) !== null) {
		const start = base + m.index;
		ranges.push({ from: start, to: start + m[0].length, kind: 'tag' });
	}
}

function scanRustdoc(text: string, base: number, ranges: DoctagRange[]): void {
	const prefix = RUSTDOC_PREFIX_RE.exec(text);
	const restStart = prefix ? prefix[0].length : 0;
	const rest = text.slice(restStart);
	// Section headings (`# Examples`, `## Panics`) — the whole heading
	// run is one span, like a Markdown ATX heading.
	const heading = /^[ \t]*(#+)[ \t]+\S/.exec(rest);
	if (heading) {
		const hashAt = restStart + heading[0].indexOf('#');
		ranges.push({ from: base + hashAt, to: base + text.trimEnd().length, kind: 'heading' });
		return;
	}
	RUSTDOC_LINK_RE.lastIndex = 0;
	let m: RegExpExecArray | null;
	while ((m = RUSTDOC_LINK_RE.exec(text)) !== null) {
		const inner = m[0].slice(1, -1);
		if (inner.trim() === '' || inner !== inner.trim()) {
			continue;
		}
		const start = base + m.index;
		ranges.push({ from: start, to: start + m[0].length, kind: 'link' });
	}
}

// Go doc links (Go 1.19+): `[Name]`, `[pkg.Name]`, or `[full/import/path]`.
// The inner must look like an identifier path — chatter like `[1, 2]` or
// `[not a link]` stays plain. Doc headings use a single `#` (unlike
// Markdown, `##` is not a deeper level).
const GODOC_LINK_RE = /\[[^[()\n]+\]/g;
const GODOC_LINK_INNER_RE = /^[A-Za-z_][\w./]*$/;
const GODOC_PREFIX_RE = /^\s*\/\/[ \t]?/;

function scanGodoc(text: string, base: number, ranges: DoctagRange[]): void {
	const prefix = GODOC_PREFIX_RE.exec(text);
	const restStart = prefix ? prefix[0].length : 0;
	const rest = text.slice(restStart);
	const heading = /^[ \t]*#[ \t]+\S/.exec(rest);
	if (heading) {
		const hashAt = restStart + heading[0].indexOf('#');
		ranges.push({ from: base + hashAt, to: base + text.trimEnd().length, kind: 'heading' });
		return;
	}
	GODOC_LINK_RE.lastIndex = 0;
	let m: RegExpExecArray | null;
	while ((m = GODOC_LINK_RE.exec(text)) !== null) {
		const inner = m[0].slice(1, -1);
		if (inner.trim() === '' || inner !== inner.trim() || !GODOC_LINK_INNER_RE.test(inner)) {
			continue;
		}
		const start = base + m.index;
		ranges.push({ from: start, to: start + m[0].length, kind: 'link' });
	}
}

// Sphinx-style docstring fields (`:param x:`, `:returns:`, `:raises E:`)
// and roles (`` :func:`foo` ``). Fields decorate as one tag span; roles
// split into the `:role:` sigil (tag) and the backtick payload (link).
// The field regex excludes role matches via the backtick lookahead.
const PY_FIELD_RE = /:(?:\w+(?:[ \t]+[\w.\\]+)?):(?!`)/g;
const PY_ROLE_RE = /:(\w+):`([^`\n]+)`/g;

function scanPydoc(text: string, base: number, ranges: DoctagRange[]): void {
	PY_ROLE_RE.lastIndex = 0;
	let m: RegExpExecArray | null;
	const roleSpans: [number, number][] = [];
	while ((m = PY_ROLE_RE.exec(text)) !== null) {
		const sigilStart = base + m.index;
		const sigilEnd = sigilStart + m[1]!.length + 2;
		ranges.push({ from: sigilStart, to: sigilEnd, kind: 'tag' });
		const payloadStart = sigilEnd;
		const payloadEnd = payloadStart + m[2]!.length + 2;
		ranges.push({ from: payloadStart, to: payloadEnd, kind: 'link' });
		roleSpans.push([sigilStart - base, payloadEnd - base]);
	}
	PY_FIELD_RE.lastIndex = 0;
	while ((m = PY_FIELD_RE.exec(text)) !== null) {
		if (roleSpans.some(([s, e]) => m!.index >= s && m!.index < e)) {
			continue;
		}
		const start = base + m.index;
		ranges.push({ from: start, to: start + m[0].length, kind: 'tag' });
	}
}

function buildDecorations(view: EditorView, flavor: DoctagFlavor): DecorationSet {
	const ranges: Range<Decoration>[] = [];
	for (const { from, to } of view.visibleRanges) {
		for (const range of collectDoctagRanges(view.state, from, to, flavor)) {
			ranges.push(MARKS[range.kind].range(range.from, range.to));
		}
	}
	return Decoration.set(ranges, true);
}

export function doctagExtension(options: { flavor: DoctagFlavor }): Extension {
	const flavor = options.flavor;
	return ViewPlugin.fromClass(
		class {
			decorations: DecorationSet;
			// Identity of the tree the current decoration set was built
			// against. The async parse can complete without any doc /
			// viewport change, and comment shapes only settle then —
			// `syntaxTree` returns a *new* tree object per parse
			// generation, so an identity check catches that completion
			// with zero extra work.
			tree: unknown;
			constructor(view: EditorView) {
				this.tree = syntaxTree(view.state);
				this.decorations = buildDecorations(view, flavor);
			}
			update(update: ViewUpdate) {
				const tree = syntaxTree(update.startState);
				if (update.docChanged || update.viewportChanged || tree !== syntaxTree(update.view.state)) {
					this.tree = syntaxTree(update.view.state);
					this.decorations = buildDecorations(update.view, flavor);
				}
			}
		},
		{ decorations: (value) => value.decorations },
	);
}

// Exposed for unit tests so they don't have to spin up an EditorView.
export const __test = { collectDoctagRanges };
