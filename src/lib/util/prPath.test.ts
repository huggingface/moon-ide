import { describe, expect, it } from 'vitest';
import { isPrPath, parsePrPath, prPath, prTabName, splitTabName } from './prPath';

describe('prPath', () => {
	it('round-trips the folder and the number', () => {
		expect(prPath('/code/app', 4099)).toBe('pr:///code/app#4099');
		expect(isPrPath('pr:///code/app#4099')).toBe(true);
		expect(parsePrPath('pr:///code/app#4099')).toEqual({ folder: '/code/app', number: 4099 });
	});

	it('reads the number after the last #', () => {
		expect(parsePrPath(prPath('/code/c#-app', 7))).toEqual({ folder: '/code/c#-app', number: 7 });
	});

	it('rejects other paths and malformed numbers', () => {
		expect(isPrPath('src/pr.ts')).toBe(false);
		expect(parsePrPath('commit:///code/app#4099')).toBeNull();
		expect(parsePrPath('pr://4099')).toBeNull();
		expect(parsePrPath('pr://#4099')).toBeNull();
		expect(parsePrPath('pr:///code/app#')).toBeNull();
		expect(parsePrPath('pr:///code/app#0')).toBeNull();
		expect(parsePrPath('pr:///code/app#12a')).toBeNull();
	});

	it('names the tab after the number and a bounded title', () => {
		expect(prTabName(7, 'Fix the thing')).toBe('#7 Fix the thing');
		expect(prTabName(7, '  ')).toBe('#7');
		const long = prTabName(7, 'x'.repeat(80));
		expect(long).toBe(`#7 ${'x'.repeat(47)}…`);
	});

	it('splits a tab name back into number and title', () => {
		expect(splitTabName('#21155 Prevent two accounts')).toEqual({ ref: '#21155', title: 'Prevent two accounts' });
		expect(splitTabName('#7')).toEqual({ ref: '#7', title: '' });
		expect(splitTabName('README.md')).toBeNull();
		expect(splitTabName('#notes.md')).toBeNull();
	});
});
