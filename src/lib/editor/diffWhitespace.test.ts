import { describe, expect, it } from 'vitest';

import { EditorState } from '@codemirror/state';
import { Chunk, diff as rawDiff } from '@codemirror/merge';

import { changesEqualModuloWhitespace, diffConfigIgnoringWhitespace } from './diffWhitespace';

function chunkLines(a: string, b: string, ignore: boolean): { a: string[]; b: string[] } {
	const docA = EditorState.create({ doc: a }).doc;
	const docB = EditorState.create({ doc: b }).doc;
	// MergeView defaults `scanLimit` to 500; mirror that so the test
	// exercises the same code path as production.
	const base = { scanLimit: 500 };
	const conf = ignore ? { ...base, ...diffConfigIgnoringWhitespace() } : { ...base, override: rawDiff };
	const chunks = Chunk.build(docA, docB, conf);
	return {
		a: chunks.map((c) => docA.sliceString(c.fromA, c.endA)),
		b: chunks.map((c) => docB.sliceString(c.fromB, c.endB)),
	};
}

describe('changesEqualModuloWhitespace', () => {
	it('drops a pure reindent (tab width shift)', () => {
		const a = 'if (ok) {\n\tfoo();\n}';
		const b = 'if (ok) {\n\t\tfoo();\n}';
		expect(changesEqualModuloWhitespace(a, b, rawDiff(a, b))).toHaveLength(0);
	});

	it('drops space-to-tab conversions', () => {
		const a = 'function f() {\n    return 1;\n}';
		const b = 'function f() {\n\treturn 1;\n}';
		expect(changesEqualModuloWhitespace(a, b, rawDiff(a, b))).toHaveLength(0);
	});

	it('drops trailing-whitespace-only churn', () => {
		const a = 'const x = 1;  \nconst y = 2;';
		const b = 'const x = 1;\nconst y = 2;';
		expect(changesEqualModuloWhitespace(a, b, rawDiff(a, b))).toHaveLength(0);
	});

	it('keeps changes that alter non-whitespace bytes', () => {
		const a = 'if (ok) {\n\tfoo();\n}';
		const b = 'if (ok) {\n\t\tbar();\n}';
		expect(changesEqualModuloWhitespace(a, b, rawDiff(a, b)).length).toBeGreaterThan(0);
	});

	it('hides whitespace changes inside strings too (documented -w trade-off)', () => {
		// `git diff -w` makes no lexical distinction: a line whose
		// only delta is whitespace inside a string literal is
		// hidden. The filter matches that on purpose — a token-
		// aware comparison would need a grammar per language.
		const a = 'const s = "a  b";';
		const b = 'const s = "a b";';
		expect(changesEqualModuloWhitespace(a, b, rawDiff(a, b))).toHaveLength(0);
	});

	it('keeps pure additions and deletions', () => {
		const a = 'one\ntwo';
		const b = 'one\ninserted\ntwo';
		expect(changesEqualModuloWhitespace(a, b, rawDiff(a, b)).length).toBeGreaterThan(0);
		const c = 'one\ntwo';
		const d = 'one';
		expect(changesEqualModuloWhitespace(c, d, rawDiff(c, d)).length).toBeGreaterThan(0);
	});
});

describe('diffConfigIgnoringWhitespace via Chunk.build', () => {
	it('produces no chunks for a file-wide reindent', () => {
		const a = 'export async function f() {\n\tconst x = 1;\n\tif (x) {\n\t\treturn;\n\t}\n}';
		const b = 'export async function f() {\n\t\tconst x = 1;\n\t\tif (x) {\n\t\t\treturn;\n\t\t}\n}';
		expect(chunkLines(a, b, false).a.length).toBeGreaterThan(0);
		expect(chunkLines(a, b, true).a).toHaveLength(0);
	});

	it('still chunks the real edit inside a reindented block', () => {
		const a = 'if (ok) {\n\tfoo();\n\tbaz();\n}';
		const b = 'if (ok) {\n\t\tfoo();\n\t\tbar();\n}';
		const { b: bLines } = chunkLines(a, b, true);
		expect(bLines).toHaveLength(1);
		expect(bLines[0]).toContain('bar();');
		expect(bLines[0]).not.toContain('foo();');
	});
});
