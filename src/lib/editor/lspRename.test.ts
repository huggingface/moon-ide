import { describe, expect, it } from 'vitest';

import { applyEditsToText } from './lspApplyEdits';
import type { LspTextEdit } from '../protocol';

function edit(startLine: number, startChar: number, endLine: number, endChar: number, newText: string): LspTextEdit {
	return {
		range: {
			start: { line: startLine, character: startChar },
			end: { line: endLine, character: endChar },
		},
		newText,
	};
}

describe('applyEditsToText', () => {
	it('replaces a single identifier on one line', () => {
		const text = 'const foo = 1;\n';
		const next = applyEditsToText(text, [edit(0, 6, 0, 9, 'bar')]);
		expect(next).toBe('const bar = 1;\n');
	});

	it('applies multiple edits without shifting later offsets', () => {
		const text = 'foo + foo + foo';
		const edits = [edit(0, 0, 0, 3, 'BAR'), edit(0, 6, 0, 9, 'BAR'), edit(0, 12, 0, 15, 'BAR')];
		expect(applyEditsToText(text, edits)).toBe('BAR + BAR + BAR');
	});

	it('handles edits across multiple lines', () => {
		const text = 'function foo() {\n  return foo;\n}\n';
		const edits = [edit(0, 9, 0, 12, 'qux'), edit(1, 9, 1, 12, 'qux')];
		expect(applyEditsToText(text, edits)).toBe('function qux() {\n  return qux;\n}\n');
	});

	it('accepts edits in any order — the applier sorts descending internally', () => {
		const text = 'aa bb cc';
		const a = edit(0, 0, 0, 2, 'AA');
		const b = edit(0, 3, 0, 5, 'BB');
		const c = edit(0, 6, 0, 8, 'CC');
		expect(applyEditsToText(text, [a, b, c])).toBe('AA BB CC');
		expect(applyEditsToText(text, [c, a, b])).toBe('AA BB CC');
		expect(applyEditsToText(text, [b, c, a])).toBe('AA BB CC');
	});

	it('handles pure insertions (empty range)', () => {
		const text = 'foo';
		const next = applyEditsToText(text, [edit(0, 0, 0, 0, 'bar ')]);
		expect(next).toBe('bar foo');
	});

	it('handles pure deletions (empty newText)', () => {
		const text = 'foo bar baz';
		const next = applyEditsToText(text, [edit(0, 3, 0, 7, '')]);
		expect(next).toBe('foo baz');
	});

	it('clamps a line index past EOF to text length', () => {
		const text = 'one\ntwo\n';
		// `line: 99` is past the doc — applier should treat the
		// range as starting (and ending) at the end of the doc,
		// effectively appending.
		const next = applyEditsToText(text, [edit(99, 0, 99, 0, 'three')]);
		expect(next).toBe('one\ntwo\nthree');
	});
});
