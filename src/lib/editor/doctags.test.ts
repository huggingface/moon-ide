import { EditorState } from '@codemirror/state';
import { describe, expect, it } from 'vitest';

import type { DoctagFlavor } from './doctags';
import { __test } from './doctags';

const { collectDoctagRanges } = __test;

// Build a state whose syntax layer is the real grammar under test,
// mirroring `language.test.ts`'s approach: the tree needs to settle
// synchronously for a small document.
async function stateFor(source: string, flavor: DoctagFlavor) {
	switch (flavor) {
		case 'rustdoc': {
			const { rust } = await import('@codemirror/lang-rust');
			return EditorState.create({ doc: source, extensions: [rust()] });
		}
		case 'godoc': {
			const { go } = await import('@codemirror/lang-go');
			return EditorState.create({ doc: source, extensions: [go()] });
		}
		case 'pydoc': {
			const { python } = await import('@codemirror/lang-python');
			return EditorState.create({ doc: source, extensions: [python()] });
		}
		default: {
			const { javascript } = await import('@codemirror/lang-javascript');
			return EditorState.create({ doc: source, extensions: [javascript()] });
		}
	}
}

type Hit = [number, number, string];

async function hits(source: string, flavor: DoctagFlavor = 'jsdoc'): Promise<Hit[]> {
	const state = await stateFor(source, flavor);
	return collectDoctagRanges(state, 0, state.doc.length, flavor).map((r) => [r.from, r.to, r.kind] as Hit);
}

function span(source: string, [from, to]: Hit): string {
	return source.slice(from, to);
}

describe('JSDoc doctags', () => {
	it('decorates block tags in a /** */ comment', async () => {
		const source = '/**\n * @param x the thing\n * @returns nothing\n */';
		const found = await hits(source);
		expect(found).toHaveLength(2);
		expect(span(source, found[0]!)).toBe('@param');
		expect(span(source, found[1]!)).toBe('@returns');
	});

	it('decorates {@link} sigil and target separately', async () => {
		const source = '/** See {@link Foo.bar baz} for more. */';
		const found = await hits(source);
		expect(found).toHaveLength(2);
		expect(span(source, found[0]!)).toBe('{@link');
		expect(found[0]![2]).toBe('tag');
		expect(span(source, found[1]!)).toBe('Foo.bar');
		expect(found[1]![2]).toBe('link');
	});

	it('keeps a non-link inline tag to its sigil', async () => {
		const source = '/** {@code foo} */';
		const found = await hits(source);
		expect(found).toHaveLength(1);
		expect(span(source, found[0]!)).toBe('{@code');
	});

	it('ignores @words in plain /* */ comments and code', async () => {
		const source = '/* @param not a doc */\nconst email = "user@example.com"; // @returns prose';
		expect(await hits(source)).toHaveLength(0);
	});

	it('recognizes the ts pragmas in // line comments only', async () => {
		const source = '// @ts-expect-error broken\nconst x: number = null;';
		const found = await hits(source);
		expect(found).toHaveLength(1);
		expect(span(source, found[0]!)).toBe('@ts-expect-error');
	});

	it('does not decorate inside code that follows the */ closer', async () => {
		const source = '/** doc */ const tagged = f(@param);';
		const found = await hits(source);
		expect(found.every(([, to]) => to <= 9)).toBe(true);
	});

	it('skips emails and keypaths in JSDoc prose', async () => {
		const source = '/**\n * Contact user@example.com.\n * @see {@link https://x} and a@b.c\n */';
		const found = await hits(source);
		expect(found.some(([from, to]) => span(source, [from, to, '']).includes('example.com'))).toBe(false);
	});
});

describe('rustdoc doctags', () => {
	it('decorates section headings across the whole line', async () => {
		const source = '/// # Examples\n///\n/// ```rust\nfn f() {}\n```';
		const found = await hits(source, 'rustdoc');
		expect(found).toHaveLength(1);
		expect(span(source, found[0]!)).toBe('# Examples');
		expect(found[0]![2]).toBe('heading');
	});

	it('decorates [Foo] intra-doc links', async () => {
		const source = '/// See [`Bar`] and [crate::baz] for details.';
		const found = await hits(source, 'rustdoc');
		expect(found.map((h) => span(source, h))).toEqual(['[`Bar`]', '[crate::baz]']);
		expect(found.every((h) => h[2] === 'link')).toBe(true);
	});

	it('ignores @words and [] prose in plain // comments', async () => {
		const source = '// normal @param mention [not a link]\nfn f() {}';
		expect(await hits(source, 'rustdoc')).toHaveLength(0);
	});

	it('handles /** */ rustdoc block comments', async () => {
		const source = '/** # Panics\n *\n * If [x] is bad.\n */';
		const found = await hits(source, 'rustdoc');
		expect(span(source, found[0]!)).toBe('# Panics');
		expect(found.some((h) => span(source, h) === '[x]')).toBe(true);
	});
});

describe('Go doc comments', () => {
	it('decorates doc links in a doc comment before a declaration', async () => {
		const source = '// F does things.\n// See [Bar] and [pkg.Baz].\nfunc F() {}\n';
		const found = await hits(source, 'godoc');
		expect(found.map((h) => span(source, h))).toEqual(['[Bar]', '[pkg.Baz]']);
	});

	it('decorates a single-# heading', async () => {
		const source = '// # Section\n//\n// Doc [X].\nfunc F() {}\n';
		const found = await hits(source, 'godoc');
		expect(found.some((h) => span(source, h) === '# Section' && h[2] === 'heading')).toBe(true);
	});

	it('leaves trailing chatter comments plain', async () => {
		const source = 'func F() {}\n\n// chatter [not a link]\nvar y = 1\n';
		const found = await hits(source, 'godoc');
		// `var` is also a declaration, so the comment before it is a
		// doc comment — but the bracketed inner has a space and fails
		// the identifier-path check, so nothing decorates.
		expect(found).toHaveLength(0);
	});

	it('skips bracketed non-identifier inners', async () => {
		const source = '// Doc with [1, 2] numbers.\nfunc F() {}\n';
		expect(await hits(source, 'godoc')).toHaveLength(0);
	});
});

describe('Python docstrings', () => {
	it('decorates Sphinx fields and roles in a function docstring', async () => {
		const source = 'def f(x):\n    """Does things.\n\n    :param x: the thing\n    :func:`foo` ref\n    """\n';
		const found = await hits(source, 'pydoc');
		const texts = found.map((h) => span(source, h));
		expect(texts).toContain(':param x:');
		expect(texts).toContain(':func:');
		expect(texts).toContain('`foo`');
	});

	it('leaves ordinary strings plain', async () => {
		const source = 'x = "not a docstring :param:"\n';
		expect(await hits(source, 'pydoc')).toHaveLength(0);
	});

	it('leaves non-first statements plain even inside functions', async () => {
		const source = 'def f():\n    """Real doc."""\n    s = ":param: not a doc"\n    return s\n';
		const found = await hits(source, 'pydoc');
		expect(found.every(([, to]) => to <= source.indexOf('Real doc') + 12)).toBe(true);
	});
});
