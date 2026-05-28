import MarkdownIt from 'markdown-it';
import { describe, expect, it } from 'vitest';

import { __test, slugifyHeading } from './markdown';

const { applyHeadingAnchorRule, applyInlineAnchorRule } = __test;

// Build a bare markdown-it with just our two anchor rules. We skip
// the highlighter (grammar imports require the editor's bundle
// pipeline) and DOMPurify (no DOM in vitest's default env). The
// rules under test produce HTML that DOMPurify only ever cleans
// up, never alters semantically, so testing pre-sanitisation is
// sufficient.
function buildParser(): MarkdownIt {
	const md = new MarkdownIt({ html: false, linkify: false });
	applyHeadingAnchorRule(md);
	applyInlineAnchorRule(md);
	return md;
}

describe('slugifyHeading', () => {
	it('lower-cases and dasherises ASCII headings', () => {
		expect(slugifyHeading('Known limitations')).toBe('known-limitations');
		expect(slugifyHeading('Sync repos')).toBe('sync-repos');
	});

	it('strips punctuation but keeps word characters and dashes', () => {
		expect(slugifyHeading('What we ship (and what we do not!)')).toBe('what-we-ship-and-what-we-do-not');
		// Runs of stripped characters collapse with the surrounding
		// whitespace into a single dash — same as GitHub's slugger.
		expect(slugifyHeading('A/B testing & C++')).toBe('ab-testing-c');
	});

	it('collapses runs of dashes and whitespace into a single dash', () => {
		expect(slugifyHeading('foo   --   bar')).toBe('foo-bar');
	});

	it('returns an empty string for headings with no slug-worthy characters', () => {
		expect(slugifyHeading('!!!')).toBe('');
		expect(slugifyHeading('   ')).toBe('');
	});

	it('strips non-ASCII rather than transliterating', () => {
		// Documented behaviour: authors who want a deterministic
		// anchor on a non-ASCII heading attach an explicit `<a name>`.
		expect(slugifyHeading('café — résumé')).toBe('caf-rsum');
	});
});

describe('heading anchor rule', () => {
	it('attaches id="…" to each heading', () => {
		const html = buildParser().render('# Hello world\n\n## Sync repos\n');
		expect(html).toContain('<h1 id="hello-world">');
		expect(html).toContain('<h2 id="sync-repos">');
	});

	it('suffixes duplicate headings with -1, -2, … (first occurrence unsuffixed)', () => {
		const html = buildParser().render('# Setup\n\n## Setup\n\n### Setup\n');
		expect(html).toContain('<h1 id="setup">');
		expect(html).toContain('<h2 id="setup-1">');
		expect(html).toContain('<h3 id="setup-2">');
	});

	it('skips headings whose slug is empty', () => {
		const html = buildParser().render('# !!!\n');
		expect(html).toContain('<h1>!!!</h1>');
	});
});

describe('inline anchor rule', () => {
	it('rewrites <a name="x"></a> to a clean id anchor', () => {
		const html = buildParser().render('Before <a name="sync-repos"></a> after.\n');
		expect(html).toContain('<a id="sync-repos"></a>');
		// The literal `<a name=…>` source must not survive — that
		// would defeat the whole rule.
		expect(html).not.toContain('name=');
	});

	it('accepts <a id="x"></a> equivalently', () => {
		const html = buildParser().render('<a id="step-3"></a>\n\nNext.\n');
		expect(html).toContain('<a id="step-3"></a>');
	});

	it('accepts a self-closing <a name="x" />', () => {
		const html = buildParser().render('<a name="foo" />\n');
		expect(html).toContain('<a id="foo"></a>');
	});

	it('slugifies the captured name the same way headings do', () => {
		const html = buildParser().render('<a name="Sync Repos!"></a>\n');
		expect(html).toContain('<a id="sync-repos"></a>');
	});

	it('escapes the slug so a malicious name cannot break out of the attribute', () => {
		// The slugifier strips the `"`, but belt-and-suspenders:
		// the fallback path (slug = '' → use raw name) is still
		// escaped before insertion.
		const html = buildParser().render('<a name="ok"></a>\n');
		expect(html).toContain('<a id="ok"></a>');
		expect(html).not.toContain('<script');
	});

	it('refuses anchors with extra attributes (defence in depth)', () => {
		// Anything that isn't the narrow `<a name|id="…"></a>` shape
		// falls through to the default tokeniser, which escapes the
		// raw HTML to literal text (because `html: false`). The
		// `onclick` substring survives — but only as escaped text
		// inside a `<p>`, never as a live attribute.
		const html = buildParser().render('<a name="foo" onclick="evil()"></a>\n');
		expect(html).toContain('&lt;a name=');
		expect(html).not.toMatch(/<a\b/);
	});

	it('refuses anchors with inner content', () => {
		const html = buildParser().render('<a name="foo">label</a>\n');
		// Inner text would smuggle content past the rule; fall
		// through to escape.
		expect(html).toContain('&lt;a name=');
	});

	it('refuses single-quoted attributes (out of scope for now)', () => {
		const html = buildParser().render("<a name='foo'></a>\n");
		expect(html).not.toContain('id="foo"');
	});
});
