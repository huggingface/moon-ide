import { describe, expect, it } from 'vitest';

import { splitTextLinks } from './textLinks';

function links(text: string): string[] {
	return splitTextLinks(text).flatMap((s) => (s.kind === 'link' ? [s.href] : []));
}

describe('splitTextLinks', () => {
	it('returns plain text untouched', () => {
		expect(splitTextLinks('no links here')).toEqual([{ kind: 'text', text: 'no links here' }]);
		expect(splitTextLinks('')).toEqual([]);
	});

	it('splits around a url and preserves surrounding text', () => {
		expect(splitTextLinks('see https://example.com/a?b=1#c now')).toEqual([
			{ kind: 'text', text: 'see ' },
			{ kind: 'link', text: 'https://example.com/a?b=1#c', href: 'https://example.com/a?b=1#c' },
			{ kind: 'text', text: ' now' },
		]);
	});

	it('round-trips the original text', () => {
		const text = 'a (https://x.com/y). then http://z.io, done\nhttps://q.dev';
		expect(
			splitTextLinks(text)
				.map((s) => s.text)
				.join(''),
		).toBe(text);
	});

	it('trims trailing punctuation and unbalanced brackets', () => {
		expect(links('go to https://x.com/a.')).toEqual(['https://x.com/a']);
		expect(links('(https://x.com/a)')).toEqual(['https://x.com/a']);
		expect(links('[https://x.com/a],')).toEqual(['https://x.com/a']);
	});

	it('keeps balanced parens inside the url', () => {
		expect(links('https://en.wikipedia.org/wiki/Foo_(bar)')).toEqual(['https://en.wikipedia.org/wiki/Foo_(bar)']);
	});

	it('ignores bare schemes and non-http schemes', () => {
		expect(links('https:// and ftp://x.com and javascript:alert(1)')).toEqual([]);
	});
});
