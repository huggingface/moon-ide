// Per-workspace chrome tint (ADR 0047). Each workspace carries one
// identity colour — `WorkspaceMeta.color` when the user picked one,
// the deterministic hash-derived hue otherwise (the same value the
// window icon paints). We rotate that colour to fixed
// saturation/lightness per resolved theme so the tint stays readable
// in both palettes, and paint it on `:root` as the `--m-ws-accent*`
// custom properties. Chrome surfaces (folder bars, editor tabs,
// status bar) read the vars; flipping `.light` swaps which var value
// applies with no JS round-trip.

import { defaultWorkspaceColor } from './workspacePicker.svelte';

export type ResolvedTheme = 'dark' | 'light';

export type Rgb = { r: number; g: number; b: number };
export type Hsl = { h: number; s: number; l: number };

/** Parse `#rgb` / `#rrggbb` (case-insensitive). Returns `null` on
 * anything else — a corrupted catalog entry must never produce an
 * invisible tint, so callers fall back to the hash hue. */
export function parseHexColor(input: string): Rgb | null {
	const m = /^#?([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.exec(input.trim());
	const capture = m?.[1];
	if (capture === undefined) {
		return null;
	}
	const hex =
		capture.length === 3
			? capture
					.split('')
					.map((c) => c + c)
					.join('')
			: capture;
	const n = parseInt(hex, 16);
	return { r: (n >> 16) & 0xff, g: (n >> 8) & 0xff, b: n & 0xff };
}

export function rgbToHsl({ r, g, b }: Rgb): Hsl {
	const rn = r / 255;
	const gn = g / 255;
	const bn = b / 255;
	const max = Math.max(rn, gn, bn);
	const min = Math.min(rn, gn, bn);
	const l = (max + min) / 2;
	if (max === min) {
		return { h: 0, s: 0, l };
	}
	const d = max - min;
	const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
	let h: number;
	if (max === rn) {
		h = ((gn - bn) / d + (gn < bn ? 6 : 0)) * 60;
	} else if (max === gn) {
		h = ((bn - rn) / d + 2) * 60;
	} else {
		h = ((rn - gn) / d + 4) * 60;
	}
	return { h, s, l };
}

/** Derive the two `--m-ws-accent*` values for one resolved theme.
 * Only the hue survives from the picked colour: saturation and
 * lightness are pinned per palette so a near-black or neon user
 * colour still lands in the readable band, and the dark/light flip
 * is a pure CSS-var swap. */
export function deriveWorkspaceAccent(color: string, theme: ResolvedTheme): { accent: string; soft: string } {
	const hsl = rgbToHsl(parseHexColor(color) ?? { r: 0, g: 0, b: 0 });
	const s = Math.round(hsl.s * 100) >= 8 ? 65 : 0;
	if (theme === 'light') {
		return { accent: `hsl(${Math.round(hsl.h)} ${s}% 40%)`, soft: `hsl(${Math.round(hsl.h)} ${s}% 40% / 0.16)` };
	}
	return { accent: `hsl(${Math.round(hsl.h)} ${s}% 72%)`, soft: `hsl(${Math.round(hsl.h)} ${s}% 72% / 0.22)` };
}

/** Paint the workspace tint on `:root`. Both palettes are written
 * at once — the stylesheet picks which one is live via the `.light`
 * class, so theme toggles never need a re-run of this function.
 * `workspaceId` is the fallback source when `color` is null or
 * unparseable (mirrors the window icon's behaviour). */
export function applyWorkspaceAccent(workspaceId: string | null, color: string | null): void {
	if (workspaceId === null) {
		return;
	}
	const resolved = color !== null && parseHexColor(color) !== null ? color : defaultWorkspaceColor(workspaceId);
	const dark = deriveWorkspaceAccent(resolved, 'dark');
	const light = deriveWorkspaceAccent(resolved, 'light');
	const style = document.documentElement.style;
	style.setProperty('--m-ws-accent', dark.accent);
	style.setProperty('--m-ws-accent-soft', dark.soft);
	style.setProperty('--m-ws-accent-light', light.accent);
	style.setProperty('--m-ws-accent-soft-light', light.soft);
}
