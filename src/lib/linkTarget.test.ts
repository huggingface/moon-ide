import { describe, expect, it } from 'vitest';
import { webHref } from './linkTarget';

describe('webHref', () => {
	it('keeps http and https addresses', () => {
		expect(webHref('https://example.test/a?b=1#c')).toBe('https://example.test/a?b=1#c');
		expect(webHref(' http://localhost:5173/ ')).toBe('http://localhost:5173/');
	});

	it('refuses file links, anchors and other schemes', () => {
		expect(webHref('src/lib/a.ts')).toBeNull();
		expect(webHref('#heading')).toBeNull();
		expect(webHref('mailto:a@b.test')).toBeNull();
		expect(webHref('javascript:alert(1)')).toBeNull();
		expect(webHref('https://')).toBeNull();
		expect(webHref(null)).toBeNull();
	});
});
