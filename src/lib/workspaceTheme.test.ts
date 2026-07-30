// @vitest-environment happy-dom
import { describe, expect, it } from 'vitest';

import { applyWorkspaceScheme, deriveWorkspaceAccent, parseHexColor, rgbToHsl } from './workspaceTheme';

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
		expect(light.accent).toBe('hsl(223 60% 38%)');
		expect(light.soft).toBe('hsl(223 60% 38% / 0.16)');
	});

	it('rescues achromatic picks with zero saturation rather than a NaN hue', () => {
		expect(deriveWorkspaceAccent('#333333', 'dark').accent).toBe('hsl(0 65% 72%)');
		expect(deriveWorkspaceAccent('not-a-color', 'light').accent).toBe('hsl(0 60% 38%)');
	});
});

describe('applyWorkspaceScheme', () => {
	it('writes both palettes keyed on the workspace hue', () => {
		applyWorkspaceScheme('dummy', '#ffd700');
		const style = document.documentElement.style;
		// Hue 51 (yellow). Dark surfaces carry a whisper of it; the
		// accent carries the identity at full voice.
		expect(style.getPropertyValue('--m-bg')).toBe('hsl(51 2% 8%)');
		expect(style.getPropertyValue('--m-accent')).toBe('hsl(51 65% 72%)');
		expect(style.getPropertyValue('--m-accent-light')).toBe('hsl(51 60% 38%)');
		// Hue 51 sits close to both warning ramps (dark 27, light
		// 25), so both push toward orange to stay distinguishable.
		expect(style.getPropertyValue('--m-warning')).toBe('hsl(40 80% 70%)');
		expect(style.getPropertyValue('--m-warning-light')).toBe('hsl(38 90% 32%)');
	});

	it('keeps the warning ramp on a far hue', () => {
		applyWorkspaceScheme('dummy', '#7ea3ff');
		const style = document.documentElement.style;
		expect(style.getPropertyValue('--m-warning')).toBe('hsl(27 80% 70%)');
	});

	it('an amber workspace also moves warning off its own hue', () => {
		// Amber (45) is even closer to the 27 ramp than gold.
		applyWorkspaceScheme('dummy', '#f0b000');
		const style = document.documentElement.style;
		expect(style.getPropertyValue('--m-warning')).toBe('hsl(40 80% 70%)');
	});

	it('falls back to the deterministic hash hue on garbage input', () => {
		// `defaultWorkspaceColor('moon-ide')` hashes to some hue H;
		// an unparseable stored colour must produce the same scheme
		// as no colour at all (mirrors the window icon).
		applyWorkspaceScheme('moon-ide', null);
		const fromNull = document.documentElement.style.getPropertyValue('--m-accent');
		applyWorkspaceScheme('moon-ide', 'not-a-color');
		expect(document.documentElement.style.getPropertyValue('--m-accent')).toBe(fromNull);
	});

	it('is a no-op in preboot mode (no workspace bound)', () => {
		document.documentElement.style.removeProperty('--m-accent');
		applyWorkspaceScheme(null, '#ffd700');
		expect(document.documentElement.style.getPropertyValue('--m-accent')).toBe('');
	});
});
