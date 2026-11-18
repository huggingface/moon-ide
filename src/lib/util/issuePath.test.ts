import { describe, expect, it } from 'vitest';
import { isIssuePath, issuePath, issueTabName, parseIssuePath } from './issuePath';

describe('issuePath', () => {
	it('round-trips the folder and the number', () => {
		expect(issuePath('/code/docs', 77)).toBe('issue:///code/docs#77');
		expect(isIssuePath('issue:///code/docs#77')).toBe(true);
		expect(parseIssuePath('issue:///code/docs#77')).toEqual({ folder: '/code/docs', number: 77 });
	});

	it('rejects other paths and malformed numbers', () => {
		expect(isIssuePath('pr:///code/docs#77')).toBe(false);
		expect(parseIssuePath('pr:///code/docs#77')).toBeNull();
		expect(parseIssuePath('issue://77')).toBeNull();
		expect(parseIssuePath('issue:///code/docs#0')).toBeNull();
		expect(parseIssuePath('issue:///code/docs#7x')).toBeNull();
	});

	it('names the tab like a pull request', () => {
		expect(issueTabName(77, 'Docs build fails')).toBe('#77 Docs build fails');
	});
});
