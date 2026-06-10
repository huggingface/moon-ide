import { describe, expect, it } from 'vitest';

import { Text } from '@codemirror/state';

import { enclosingSymbol } from './diffCollapseContext';

// `fromLine` is 1-based and points at the first *visible* line after
// the collapsed region — the same value the view plugin computes
// from the placeholder's position + collapsed line count.
function symbolAfter(src: string, fromLine: number): string | null {
	return enclosingSymbol(Text.of(src.split('\n')), fromLine);
}

describe('enclosingSymbol', () => {
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
