import { describe, expect, it } from 'vitest';

import { browserIdFromPath, browserPath, browserTabName, isBrowserPath } from './browserPath';

describe('browserPath', () => {
	it('round-trips through isBrowserPath', () => {
		expect(isBrowserPath(browserPath(3))).toBe(true);
		expect(isBrowserPath('src/browser.ts')).toBe(false);
		expect(browserIdFromPath(browserPath(3))).toBe(3);
		expect(browserIdFromPath('browser://x')).toBeNull();
		expect(browserIdFromPath('src/a.ts')).toBeNull();
	});

	it('labels tabs by host and non-root path', () => {
		expect(browserTabName('http://localhost:5173/')).toBe('localhost:5173');
		expect(browserTabName('http://web:3000/models?x=1')).toBe('web:3000/models');
		expect(browserTabName('not a url')).toBe('not a url');
	});
});
