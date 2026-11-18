import { describe, expect, it } from 'vitest';
import { isIssuePath, issuePath, issueTabName, numberFromIssuePath } from './issuePath';

describe('issuePath', () => {
	it('round-trips the number', () => {
		expect(issuePath(77)).toBe('issue://77');
		expect(isIssuePath('issue://77')).toBe(true);
		expect(numberFromIssuePath('issue://77')).toBe(77);
	});

	it('rejects other paths and malformed numbers', () => {
		expect(isIssuePath('pr://77')).toBe(false);
		expect(numberFromIssuePath('pr://77')).toBeNull();
		expect(numberFromIssuePath('issue://')).toBeNull();
		expect(numberFromIssuePath('issue://0')).toBeNull();
		expect(numberFromIssuePath('issue://7x')).toBeNull();
	});

	it('names the tab like a pull request', () => {
		expect(issueTabName(77, 'Docs build fails')).toBe('#77 Docs build fails');
	});
});
