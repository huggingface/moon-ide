import { describe, expect, it } from 'vitest';

import { typescriptLanguage } from '@codemirror/lang-javascript';
import { rustLanguage } from '@codemirror/lang-rust';
import { Text } from '@codemirror/state';

import { enclosingBlocks, enclosingBlocksByIndent, enclosingStack } from './enclosingScope';

// `fromLine` is 1-based and points at the first *visible* line after
// the collapsed region — the same value the view plugin computes
// from the placeholder's position + collapsed line count.
function symbolAfter(src: string, fromLine: number): string | null {
	const stack = enclosingStack(Text.of(src.split('\n')), fromLine);
	return stack[stack.length - 1]?.label ?? null;
}

describe('enclosingStack innermost label', () => {
	it('names the function a hunk sits inside (TS, tabs)', () => {
		const src = ['export function doThing(a: number) {', '\tconst x = 1;', '\treturn x;', '}'].join('\n');
		// Line 3 (`return x;`) is inside the function body.
		expect(symbolAfter(src, 3)).toBe('export function doThing(a: number)');
	});

	it('names the nearest method inside a class body', () => {
		const src = [
			'class Widget {',
			'\trender() {',
			'\t\tconst a = 1;',
			'\t\tconst b = 2;',
			'\t\treturn a + b;',
			'\t}',
			'}',
		].join('\n');
		// Line 5 is inside `render()`, which is the nearest shallower
		// definition-looking line.
		expect(symbolAfter(src, 5)).toBe('render()');
	});

	it('names a Rust fn / impl', () => {
		const src = ['impl Foo {', '\tpub fn bar(&self) -> u32 {', '\t\tlet n = 1;', '\t\tn + 1', '\t}', '}'].join('\n');
		expect(symbolAfter(src, 4)).toBe('pub fn bar(&self) -> u32');
	});

	it('returns null for top-level code with no enclosing scope', () => {
		const src = ['const a = 1;', 'const b = 2;', 'const c = 3;'].join('\n');
		expect(symbolAfter(src, 3)).toBeNull();
	});

	it('does not latch onto control-flow keywords', () => {
		const src = ['function outer() {', '\tif (cond) {', '\t\tdoA();', '\t\tdoB();', '\t\tdoC();', '\t}', '}'].join(
			'\n',
		);
		// Line 5 sits inside the `if`, but `if (...)` must not be
		// reported — we want `outer`.
		expect(symbolAfter(src, 5)).toBe('function outer()');
	});

	it('clamps an over-long signature', () => {
		const sig =
			'export function aVeryLongFunctionNameThatGoesOnAndOnAndOnForQuiteSomeTimePastEightyChars(arg: number) {';
		const src = [sig, '\treturn arg;', '}'].join('\n');
		const out = symbolAfter(src, 2);
		expect(out).not.toBeNull();
		expect(out!.length).toBeLessThanOrEqual(80);
		expect(out!.endsWith('…')).toBe(true);
	});

	it('handles arrow-function property assignments', () => {
		const src = ['const obj = {', '\thandler: (e) => {', '\t\tprocess(e);', '\t\tlog(e);', '\t},', '};'].join('\n');
		expect(symbolAfter(src, 4)).toBe('handler: (e) =>');
	});
});

// The sticky-scroll header consumes the full chain, outermost first.
function stackAfter(src: string, fromLine: number) {
	return enclosingStack(Text.of(src.split('\n')), fromLine);
}

describe('enclosingStack', () => {
	it('returns the full chain outermost-first with line numbers', () => {
		const src = [
			'class Widget {',
			'\trender() {',
			'\t\tconst items = list.map((x) => {',
			'\t\t\treturn x + 1;',
			'\t\t});',
			'\t}',
			'}',
		].join('\n');
		expect(stackAfter(src, 4)).toEqual([
			{ line: 1, label: 'class Widget' },
			{ line: 2, label: 'render()' },
		]);
	});

	it('is empty for top-level code', () => {
		const src = ['const a = 1;', 'const b = 2;'].join('\n');
		expect(stackAfter(src, 2)).toEqual([]);
	});

	it('skips control flow but keeps climbing to outer scopes', () => {
		const src = [
			'impl Foo {',
			'\tpub fn bar(&self) -> u32 {',
			'\t\tif cond {',
			'\t\t\tdo_thing();',
			'\t\t}',
			'\t}',
			'}',
		].join('\n');
		expect(stackAfter(src, 4)).toEqual([
			{ line: 1, label: 'impl Foo' },
			{ line: 2, label: 'pub fn bar(&self) -> u32' },
		]);
	});
});

describe('enclosingBlocks', () => {
	const ts = [
		'import { x } from "./x";', // 1
		'', // 2
		'export class Widget {', // 3
		'\tprivate count = 0;', // 4
		'', // 5
		'\tasync render(target: HTMLElement): Promise<void> {', // 6
		'\t\tconst items = this.items();', // 7
		'\t\titems.forEach((item) => {', // 8
		'\t\t\ttarget.append(item);', // 9
		'\t\t});', // 10
		'\t\tthis.count += 1;', // 11
		'\t}', // 12
		'}', // 13
		'', // 14
		'const helper = (value: number) => {', // 15
		'\treturn value * 2;', // 16
		'};', // 17
	].join('\n');
	const tsDoc = Text.of(ts.split('\n'));
	const tsTree = typescriptLanguage.parser.parse(ts);
	const blocksAt = (line: number) => {
		const l = tsDoc.line(line);
		return enclosingBlocks(tsDoc, tsTree, l.from, l.to);
	};

	it('finds a typed method the line heuristic cannot, then its class', () => {
		expect(blocksAt(11)).toEqual([
			{ from: 6, to: 12, label: 'async render(target: HTMLElement): Promise<void>' },
			{ from: 3, to: 13, label: 'export class Widget' },
		]);
	});

	it('treats a callback as the innermost block', () => {
		expect(blocksAt(9)[0]).toEqual({ from: 8, to: 10, label: 'items.forEach((item) =>' });
	});

	it('finds an arrow function bound to a const', () => {
		expect(blocksAt(16)).toEqual([{ from: 15, to: 17, label: 'const helper = (value: number) =>' }]);
	});

	it('reports nothing for top-level code, not the document', () => {
		expect(blocksAt(1)).toEqual([]);
	});

	it('only returns blocks that contain the whole range', () => {
		const blocks = enclosingBlocks(tsDoc, tsTree, tsDoc.line(9).from, tsDoc.line(11).to);
		expect(blocks.map((b) => b.from)).toEqual([6, 3]);
	});

	it('walks Rust items', () => {
		const rs = ['impl Foo {', '\tpub fn bar(&self) -> u32 {', '\t\tlet x = 1;', '\t\tx', '\t}', '}'].join('\n');
		const doc = Text.of(rs.split('\n'));
		const line = doc.line(3);
		expect(enclosingBlocks(doc, rustLanguage.parser.parse(rs), line.from, line.to)).toEqual([
			{ from: 2, to: 5, label: 'pub fn bar(&self) -> u32' },
			{ from: 1, to: 6, label: 'impl Foo' },
		]);
	});
});

describe('enclosingBlocksByIndent', () => {
	it('ends a block at its closing brace', () => {
		const src = ['fn a() {', '\tone();', '', '\ttwo();', '}', '', 'fn b() {', '\tthree();', '}'].join('\n');
		expect(enclosingBlocksByIndent(Text.of(src.split('\n')), 4)).toEqual([{ from: 1, to: 5, label: 'fn a()' }]);
	});

	it('ends an indentation-only block before the next sibling', () => {
		const src = ['def a():', '    one()', '    two()', 'def b():', '    three()'].join('\n');
		expect(enclosingBlocksByIndent(Text.of(src.split('\n')), 2)).toEqual([{ from: 1, to: 3, label: 'def a():' }]);
	});

	it('returns the innermost definition first', () => {
		const src = ['class A {', '\tmethod() {', '\t\twork();', '\t}', '}'].join('\n');
		expect(enclosingBlocksByIndent(Text.of(src.split('\n')), 3).map((b) => [b.from, b.to])).toEqual([
			[2, 4],
			[1, 5],
		]);
	});
});
