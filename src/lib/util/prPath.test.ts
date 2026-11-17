import { describe, expect, it } from 'vitest';
import { isPrPath, numberFromPrPath, prPath, prTabName } from './prPath';

describe('prPath', () => {
	it('round-trips the number', () => {
		expect(prPath(4099)).toBe('pr://4099');
		expect(isPrPath('pr://4099')).toBe(true);
		expect(numberFromPrPath('pr://4099')).toBe(4099);
	});

	it('rejects other paths and malformed numbers', () => {
		expect(isPrPath('src/pr.ts')).toBe(false);
		expect(numberFromPrPath('commit://4099')).toBeNull();
		expect(numberFromPrPath('pr://')).toBeNull();
		expect(numberFromPrPath('pr://0')).toBeNull();
		expect(numberFromPrPath('pr://12a')).toBeNull();
	});

	it('names the tab after the number and a bounded title', () => {
		expect(prTabName(7, 'Fix the thing')).toBe('#7 Fix the thing');
		expect(prTabName(7, '  ')).toBe('#7');
		const long = prTabName(7, 'x'.repeat(80));
		expect(long).toBe(`#7 ${'x'.repeat(47)}…`);
	});
});
