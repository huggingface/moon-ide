// Which definition (function / class / method / impl ...) encloses a
// piece of code. One home for the two ways we answer that, so the
// sticky-scroll header, the diff collapse bars and their "expand to
// the enclosing block" action agree on what counts as a definition:
//
//   - `enclosingStack`: a text heuristic. Walk upward looking for the
//     nearest definition-looking line at a strictly smaller indent.
//     Works without a parser (StreamLanguage modes, plain text) and
//     without the LSP, which the review surfaces keep silent for
//     files the user only scrolls past. Never wrong on properly
//     indented code, occasionally absent.
//   - `enclosingBlocks`: the Lezer tree. Walk up from a range through
//     the nodes that contain it and keep the definition-like ones.
//     Knows where a block ends, which the indent walk can only guess.

import type { Text } from '@codemirror/state';
import type { SyntaxNode, Tree } from '@lezer/common';

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

/** One enclosing definition: its 1-based line number plus the
 *  compact label `summarise` produced from the signature. */
export interface EnclosingDef {
	line: number;
	label: string;
}

/**
 * Walk upward from `fromLine` (1-based) collecting the full chain of
 * enclosing definitions: at each step, the closest preceding line
 * that looks like a definition and sits at a strictly smaller indent
 * than everything accepted so far. Returns outermost-first (so
 * `class Foo` precedes `method bar()`); empty when the reference
 * line is top-level or nothing convincing is found.
 */
export function enclosingStack(doc: Text, fromLine: number): EnclosingDef[] {
	if (fromLine < 1 || fromLine > doc.lines) {
		return [];
	}
	// Reference indent: the indent of the first non-blank line at or
	// after `fromLine`. The enclosing scopes must be shallower than
	// this. Using the following line (rather than a blank line's own
	// zero indent) matches GitHub: the label describes the code
	// you're about to read.
	let refIndent = -1;
	for (let n = fromLine; n <= doc.lines; n++) {
		const text = doc.line(n).text;
		if (text.trim().length > 0) {
			refIndent = indentOf(text);
			break;
		}
	}
	if (refIndent <= 0) {
		// A top-level reference line has no enclosing scope to name,
		// and a negative ref means the rest of the doc is blank.
		return [];
	}
	const found: EnclosingDef[] = [];
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
			found.push({ line: n, label: summarise(trimmed) });
		}
		// Tighten the bar: every accepted definition — and every
		// shallower line that *isn't* one (e.g. a bare `{`
		// continuation or an attribute) — lowers the indent ceiling,
		// so we never re-accept a deeper line.
		bestIndent = indent;
		if (bestIndent === 0) {
			break;
		}
	}
	return found.toReversed();
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

/** An enclosing definition as a 1-based inclusive line range, plus
 *  the compact label of its first line. */
export interface EnclosingBlock {
	from: number;
	to: number;
	label: string;
}

// Lezer node names that are a definition whatever their first line
// looks like (a typed TS method, `const f = async () => {`, a Rust
// `impl`). Same vocabulary as `DEFINITION` above, spelled the way the
// grammars we ship name their nodes.
const DEFINITION_NODE = new RegExp(
	[
		'^(?:',
		'(?:Function|Method|Constructor|Class|Interface|Enum|Namespace)(?:Declaration|Definition|Decl)',
		'|(?:Function|Class|Closure|Lambda)Expression',
		'|ArrowFunction|FunctionLiteral',
		'|(?:Function|Impl|Struct|Enum|Trait|Mod)Item',
		')$',
	].join(''),
);

// Node kinds that can stand for a definition when their first line
// reads like one (`describe('x', () => {`, a K&R function body). A
// parameter or argument list starting on that same line must not: it
// ends long before the block does.
const BLOCK_LIKE_NODE = /(?:Block|Body|Declaration|Definition|Decl|Statement|Item|Expression)$/;

/**
 * Definition-like syntax nodes containing `from..to`, innermost
 * first. Single-line nodes and the document node are skipped: neither
 * is a block worth expanding to.
 */
export function enclosingBlocks(doc: Text, tree: Tree, from: number, to: number): EnclosingBlock[] {
	const out: EnclosingBlock[] = [];
	let node: SyntaxNode | null = tree.resolveInner(Math.min(from, doc.length), 1);
	while (node !== null && (node.from > from || node.to < to)) {
		node = node.parent;
	}
	for (; node !== null && node.parent !== null; node = node.parent) {
		const first = doc.lineAt(node.from);
		const last = doc.lineAt(node.to);
		if (last.number === first.number) {
			continue;
		}
		const previous = out[out.length - 1];
		if (previous !== undefined && previous.from === first.number && previous.to === last.number) {
			continue;
		}
		const trimmed = first.text.trim();
		if (DEFINITION_NODE.test(node.name) || (BLOCK_LIKE_NODE.test(node.name) && looksLikeDefinition(trimmed))) {
			out.push({ from: first.number, to: last.number, label: summarise(trimmed) });
		}
	}
	return out;
}

// A line that only closes the block opened above it.
const CLOSER = /^(?:[}\])]|end\b)/;

/**
 * The same answer from indentation alone, for documents without a
 * syntax tree: `enclosingStack`'s definitions, innermost first, each
 * running to the last line before the indent drops back to the
 * definition's own (a closing brace at that indent is included).
 */
export function enclosingBlocksByIndent(doc: Text, line: number): EnclosingBlock[] {
	return enclosingStack(doc, line)
		.toReversed()
		.map((def) => ({ from: def.line, to: indentBlockEnd(doc, def.line), label: def.label }));
}

function indentBlockEnd(doc: Text, defLine: number): number {
	const base = indentOf(doc.line(defLine).text);
	let end = defLine;
	for (let n = defLine + 1; n <= doc.lines; n++) {
		const text = doc.line(n).text;
		const trimmed = text.trim();
		if (trimmed.length === 0) {
			continue;
		}
		const indent = indentOf(text);
		if (indent > base) {
			end = n;
			continue;
		}
		return indent === base && CLOSER.test(trimmed) ? n : end;
	}
	return end;
}
