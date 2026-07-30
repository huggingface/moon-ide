import { describe, expect, it } from 'vitest';

import { parseToolImages, withoutToolImages } from './components/toolBodyHelpers';

const PNG = 'data:image/png;base64,QUJD';

describe('tool-result image extraction (ADR 0033)', () => {
	it('parses the runner images convention', () => {
		const images = parseToolImages({
			content: '[image file — image/png, attached]',
			images: [{ data_url: PNG, mime: 'image/png' }],
		});
		expect(images).toEqual([{ dataUrl: PNG, mime: 'image/png' }]);
	});

	it('defaults a missing mime and skips unusable entries', () => {
		const images = parseToolImages({
			images: [{ data_url: PNG }, { data_url: '' }, { mime: 'image/png' }, 'not-an-object', null],
		});
		expect(images).toEqual([{ dataUrl: PNG, mime: 'image' }]);
	});

	it('returns nothing for results that carry no images', () => {
		expect(parseToolImages({ content: 'plain' })).toEqual([]);
		expect(parseToolImages({ images: 'not-an-array' })).toEqual([]);
		expect(parseToolImages('a string result')).toEqual([]);
		expect(parseToolImages(null)).toEqual([]);
		// An error envelope must not be mistaken for an image result.
		expect(parseToolImages({ error: 'read_file: binary file' })).toEqual([]);
	});

	it('strips the images key so the JSON fallback stays small', () => {
		const result = { path: 'a.png', content: 'note', images: [{ data_url: PNG, mime: 'image/png' }] };
		expect(withoutToolImages(result)).toEqual({ path: 'a.png', content: 'note' });
		// The original is not mutated — the panel still renders the
		// thumbnails off the same payload.
		expect(result.images).toHaveLength(1);
	});

	it('passes through results with no images key', () => {
		const plain = { content: 'plain' };
		expect(withoutToolImages(plain)).toEqual(plain);
		expect(withoutToolImages('a string')).toBe('a string');
		expect(withoutToolImages(null)).toBe(null);
	});
});
