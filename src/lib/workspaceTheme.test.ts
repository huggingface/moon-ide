import { describe, expect, it } from 'vitest';

import { deriveWorkspaceAccent, parseHexColor, rgbToHsl } from './workspaceTheme';

describe('parseHexColor', () => {
	it('parses 6-digit hex with and without the leading #', () => {
		expect(parseHexColor('#7ea3ff')).toEqual({ r: 126, g: 163, b: 255 });
		expect(parseHexColor('7ea3ff')).toEqual({ r: 126, g: 163, b: 255 });
		expect(parseHexColor('#FF0000')).toEqual({ r: 255, g: 0, b: 0 });
	});

	it('expands 3-digit shorthand', () => {
		expect(parseHexColor('#f00')).toEqual({ r: 255, g: 0, b: 0 });
		expect(parseHexColor('0a8')).toEqual({ r: 0, g: 170, b: 136 });
	});

	it('rejects garbage that must fall back to the hash hue', () => {
		expect(parseHexColor('')).toBeNull();
		expect(parseHexColor('#ffff')).toBeNull();
		expect(parseHexColor('#gggggg')).toBeNull();
		expect(parseHexColor('red')).toBeNull();
		expect(parseHexColor('#1234567')).toBeNull();
	});
});

describe('rgbToHsl', () => {
	it('round-trips the primaries', () => {
		expect(rgbToHsl({ r: 255, g: 0, b: 0 })).toEqual({ h: 0, s: 1, l: 0.5 });
		const green = rgbToHsl({ r: 0, g: 255, b: 0 });
		expect(green.h).toBe(120);
		const blue = rgbToHsl({ r: 0, g: 0, b: 255 });
		expect(blue.h).toBe(240);
	});

	it('reports zero hue and saturation for achromatic greys', () => {
		expect(rgbToHsl({ r: 128, g: 128, b: 128 })).toEqual({ h: 0, s: 0, l: 128 / 255 });
		expect(rgbToHsl({ r: 0, g: 0, b: 0 }).s).toBe(0);
		expect(rgbToHsl({ r: 255, g: 255, b: 255 }).s).toBe(0);
	});
});

describe('deriveWorkspaceAccent', () => {
	it('keeps the hue and pins saturation/lightness per palette', () => {
		const dark = deriveWorkspaceAccent('#7ea3ff', 'dark');
		expect(dark.accent).toBe('hsl(223 65% 72%)');
		expect(dark.soft).toBe('hsl(223 65% 72% / 0.22)');
		const light = deriveWorkspaceAccent('#7ea3ff', 'light');
		expect(light.accent).toBe('hsl(223 65% 40%)');
		expect(light.soft).toBe('hsl(223 65% 40% / 0.16)');
	});

	it('rescues achromatic picks with zero saturation rather than a NaN hue', () => {
		expect(deriveWorkspaceAccent('#333333', 'dark').accent).toBe('hsl(0 0% 72%)');
		expect(deriveWorkspaceAccent('not-a-color', 'light').accent).toBe('hsl(0 0% 40%)');
	});
});
