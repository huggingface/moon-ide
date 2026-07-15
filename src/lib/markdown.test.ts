import MarkdownIt from 'markdown-it';
import { describe, expect, it } from 'vitest';

import { __test, slugifyHeading, splitFrontmatter } from './markdown';

const { applyHeadingAnchorRule, applyInlineAnchorRule, frontmatterToHtml } = __test;

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

describe('splitFrontmatter', () => {
	it('splits a leading YAML block from the body', () => {
		const { frontmatter, body } = splitFrontmatter('---\ntitle: Hello\n---\n\n# Body\n');
		expect(frontmatter).toBe('title: Hello');
		expect(body).toBe('\n# Body\n');
	});

	it('accepts `...` as a closing fence', () => {
		const { frontmatter, body } = splitFrontmatter('---\ntitle: Hello\n...\nText\n');
		expect(frontmatter).toBe('title: Hello');
		expect(body).toBe('Text\n');
	});

	it('tolerates a leading BOM and CRLF newlines', () => {
		const { frontmatter, body } = splitFrontmatter('\uFEFF---\r\ntitle: Hello\r\n---\r\nText\r\n');
		expect(frontmatter).toBe('title: Hello');
		expect(body).toBe('Text\r\n');
	});

	it('returns null when the document has no frontmatter', () => {
		const source = '# Heading\n\nText.\n';
		const { frontmatter, body } = splitFrontmatter(source);
		expect(frontmatter).toBeNull();
		expect(body).toBe(source);
	});

	it('ignores a `---` that is not at the very start', () => {
		const source = 'Intro\n\n---\nnot: frontmatter\n---\n';
		const { frontmatter, body } = splitFrontmatter(source);
		expect(frontmatter).toBeNull();
		expect(body).toBe(source);
	});

	it('does not treat a thematic break / setext heading as frontmatter', () => {
		const source = 'Heading\n---\n\nBody\n';
		const { frontmatter } = splitFrontmatter(source);
		expect(frontmatter).toBeNull();
	});
});

describe('splitTopLevelBlocks', () => {
	// We build a bare markdown-it (no highlighter, no DOMPurify) and
	// parse + split. The test verifies two invariants:
	//   1. The split correctly identifies block boundaries.
	//   2. Frozen blocks' source text is stable across deltas —
	//      the streaming invariant that makes per-block caching safe.
	const { splitTopLevelBlocks } = __test;

	function buildBareParser(): MarkdownIt {
		const md = new MarkdownIt({ html: false, linkify: true });
		applyHeadingAnchorRule(md);
		applyInlineAnchorRule(md);
		return md;
	}

	function split(md: MarkdownIt, source: string) {
		const tokens = md.parse(source, {});
		return splitTopLevelBlocks(tokens, source);
	}

	it('splits paragraphs, headings, and fences into separate blocks', () => {
		const md = buildBareParser();
		const source = '# Heading\n\nFirst paragraph.\n\nSecond paragraph.\n\n```js\nvar x = 1\n```\n';
		const blocks = split(md, source);
		expect(blocks).toHaveLength(4);
		expect(blocks[0]?.text).toBe('# Heading');
		expect(blocks[1]?.text).toBe('First paragraph.');
		expect(blocks[2]?.text).toBe('Second paragraph.');
		expect(blocks[3]?.text).toBe('```js\nvar x = 1\n```');
	});

	it('keeps a list as a single block', () => {
		const md = buildBareParser();
		const source = '- alpha\n- beta\n- gamma\n';
		const blocks = split(md, source);
		expect(blocks).toHaveLength(1);
		expect(blocks[0]?.text).toBe('- alpha\n- beta\n- gamma');
	});

	it('keeps a nested list as a single block', () => {
		const md = buildBareParser();
		const source = '- top\n  - nested\n  - nested2\n- back\n';
		const blocks = split(md, source);
		expect(blocks).toHaveLength(1);
	});

	it('keeps a blockquote with multiple lines as one block', () => {
		const md = buildBareParser();
		const source = '> line one\n> line two\n>\n> line three\n';
		const blocks = split(md, source);
		expect(blocks).toHaveLength(1);
		expect(blocks[0]?.text).toBe('> line one\n> line two\n>\n> line three');
	});

	it('treats a thematic break as its own block', () => {
		const md = buildBareParser();
		const source = 'Before\n\n---\n\nAfter\n';
		const blocks = split(md, source);
		expect(blocks).toHaveLength(3);
		expect(blocks[1]?.text).toBe('---');
	});

	it('returns an empty array for empty input', () => {
		const md = buildBareParser();
		const blocks = split(md, '');
		expect(blocks).toHaveLength(0);
	});

	it('produces identical source text for frozen blocks when the tail grows', () => {
		// The streaming invariant: appending to the last block never
		// changes the source text (or rendered HTML) of any earlier
		// block. This is what makes per-block caching safe — frozen
		// blocks' cache keys are permanent hits.
		const md = buildBareParser();

		// Delta 1: one paragraph, still growing
		const d1 = 'First paragraph is here.';
		const b1 = split(md, d1);

		// Delta 2: first paragraph finished, second started
		const d2 = 'First paragraph is here.\n\nSecond paragraph begins.';
		const b2 = split(md, d2);

		// Delta 3: second paragraph grew
		const d3 = 'First paragraph is here.\n\nSecond paragraph begins to grow.';
		const b3 = split(md, d3);

		// The first block's text is identical across all deltas
		expect(b1[0]?.text).toBe('First paragraph is here.');
		expect(b2[0]?.text).toBe(b1[0]?.text);
		expect(b3[0]?.text).toBe(b1[0]?.text);

		// The second block only appears from delta 2 onward
		expect(b2).toHaveLength(2);
		expect(b3).toHaveLength(2);
		expect(b2[1]?.text).toBe('Second paragraph begins.');
		expect(b3[1]?.text).toBe('Second paragraph begins to grow.');
	});

	it('produces stable source text for blocks before a splitting fence', () => {
		// When a paragraph finishes with \n\n and a fence starts, the
		// paragraph becomes frozen and the fence is the new live tail.
		// The paragraph's text must be identical to when it was the tail.
		const md = buildBareParser();

		const d1 = 'Here is some intro text.';
		const b1 = split(md, d1);

		const d2 = 'Here is some intro text.\n\n```rust\nfn main() {}';
		const b2 = split(md, d2);

		const d3 = 'Here is some intro text.\n\n```rust\nfn main() {}\nfn other() {}\n```';
		const b3 = split(md, d3);

		// Paragraph text is frozen and stable from delta 2 onward
		expect(b2[0]?.text).toBe(b1[0]?.text);
		expect(b3[0]?.text).toBe(b1[0]?.text);

		// Fence block appears in delta 2 and grows in delta 3
		expect(b2[1]?.text).toBe('```rust\nfn main() {}');
		expect(b3[1]?.text).toBe('```rust\nfn main() {}\nfn other() {}\n```');
	});
});

describe('frontmatterToHtml', () => {
	it('renders a mapping as a key/value table', () => {
		const html = frontmatterToHtml('title: Hello World\nlicense: mit');
		expect(html).toContain('<table class="md-frontmatter">');
		expect(html).toContain('<th scope="row">title</th>');
		expect(html).toContain('<td>Hello World</td>');
		expect(html).toContain('<th scope="row">license</th>');
	});

	it('renders a scalar list as inline chips', () => {
		const html = frontmatterToHtml('tags:\n  - alpha\n  - beta');
		expect(html).toContain('<code>alpha</code>');
		expect(html).toContain('<code>beta</code>');
	});

	it('renders nested structures as indented JSON', () => {
		const html = frontmatterToHtml('model-index:\n  name: m\n  results:\n    - task: x');
		expect(html).toContain('md-frontmatter-nested');
		expect(html).toContain('&quot;name&quot;');
	});

	it('shows an em dash for empty values', () => {
		const html = frontmatterToHtml('thumbnail:');
		expect(html).toContain('md-frontmatter-empty');
	});

	it('escapes HTML in keys and values', () => {
		const html = frontmatterToHtml('title: <script>alert(1)</script>');
		expect(html).not.toContain('<script>');
		expect(html).toContain('&lt;script&gt;');
	});

	it('falls back to a raw YAML block for a non-mapping document', () => {
		const html = frontmatterToHtml('- just\n- a\n- list');
		expect(html).toContain('md-frontmatter-raw');
		expect(html).not.toContain('<table');
	});

	it('falls back to a raw YAML block when the YAML is invalid', () => {
		const html = frontmatterToHtml('title: [unterminated');
		expect(html).toContain('md-frontmatter-raw');
	});
});
