// Per-workspace color scheme (ADR 0047). Each workspace carries one
// identity colour — `WorkspaceMeta.color` when the user picked one,
// the deterministic hash-derived hue otherwise (the same value the
// window icon paints). From that single hue we generate the full
// surface/text/accent token set for both palettes and write every
// token onto `:root` at hydrate; flipping `.light` then swaps which
// generated set the stylesheet resolves, so a theme flip stays a
// pure CSS change with no JS round-trip.
//
// Contrast is engineered per palette rather than sampled: surfaces
// sit at 1-3% saturation in the workspace hue (dark) or a plain
// hue-only wash (light), text stays neutral, and each accent keeps
// the hue but re-pins saturation/lightness into a band that holds
// ≥4.5:1 against its background. A hue near the warning ramp's amber
// (~27°) pushes warning a few degrees toward orange so identity and
// severity stay distinguishable at a glance.

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

function hslCss(h: number, s: number, l: number, alpha?: number): string {
	const base = `hsl(${Math.round(h)} ${Math.round(s)}% ${Math.round(l)}%`;
	return alpha === undefined ? `${base})` : `${base} / ${alpha})`;
}

/** Angular distance between two hues on the colour wheel, 0–180. */
function hueDistance(a: number, b: number): number {
	const d = Math.abs(((a - b) % 360) + 360) % 360;
	return d > 180 ? 360 - d : d;
}

/** The full token set for one palette. Keys are `--m-*` custom
 * property names so `applyWorkspaceScheme` can write them verbatim. */
export type WorkspaceScheme = Record<string, string>;

function buildDark(h: number): WorkspaceScheme {
	return {
		'--m-bg': hslCss(h, 2, 8),
		'--m-bg-1': hslCss(h, 2, 10),
		'--m-bg-2': hslCss(h, 2.5, 12),
		'--m-bg-3': hslCss(h, 3, 15),
		'--m-bg-overlay': 'rgba(255, 255, 255, 0.03)',
		'--m-border': hslCss(h, 3, 17),
		'--m-border-strong': hslCss(h, 3, 22),
		'--m-fg': hslCss(0, 0, 88),
		'--m-fg-muted': hslCss(0, 0, 62),
		'--m-fg-subtle': hslCss(0, 0, 45),
		'--m-accent': hslCss(h, 65, 72),
		'--m-accent-strong': hslCss(h, 65, 80),
		// Push away from the workspace hue when it sits anywhere
		// near the amber ramp, else "this workspace is yellow" and
		// "this is a warning" become the same colour. The band is
		// wide on purpose: hues <25° apart read as one colour at UI
		// saturation, so gold (51) counts as clashing with 27.
		'--m-warning': hslCss(hueDistance(h, 27) < 30 ? 40 : 27, 80, 70),
		'--m-editor-selection': hslCss(h, 65, 72, 0.18),
		'--m-ws-accent-soft': hslCss(h, 65, 72, 0.22),
	};
}

function buildLight(h: number): WorkspaceScheme {
	return {
		// Saturation stays at zero in the light palette: surfaces
		// differ from each other by only ~2% lightness, and even a
		// few % of a saturated hue reads as visible banding. The
		// workspace colour comes back in the accents, borders, and
		// overlays — which is where the eye looks for identity.
		'--m-bg': hslCss(0, 0, 99),
		'--m-bg-1': hslCss(0, 0, 96.5),
		'--m-bg-2': hslCss(0, 0, 94),
		'--m-bg-3': hslCss(0, 0, 91),
		'--m-bg-overlay': hslCss(h, 65, 40, 0.05),
		'--m-border': hslCss(h, 20, 85),
		'--m-border-strong': hslCss(h, 25, 74),
		'--m-fg': hslCss(0, 0, 10),
		'--m-fg-muted': hslCss(0, 0, 38),
		'--m-fg-subtle': hslCss(0, 0, 55),
		'--m-accent': hslCss(h, 60, 38),
		'--m-accent-strong': hslCss(h, 60, 30),
		'--m-warning': hslCss(hueDistance(h, 25) < 30 ? 38 : 25, 90, 32),
		'--m-editor-selection': hslCss(h, 60, 38, 0.15),
		'--m-ws-accent-soft': hslCss(h, 60, 38, 0.16),
	};
}

/** Derive the two `--m-ws-accent*` values for one resolved theme.
 * Kept exported for tests and for callers that only want the
 * identity accent, not the whole scheme. */
export function deriveWorkspaceAccent(color: string, theme: ResolvedTheme): { accent: string; soft: string } {
	const hsl = rgbToHsl(parseHexColor(color) ?? { r: 0, g: 0, b: 0 });
	const h = Math.round(hsl.h);
	if (theme === 'light') {
		return { accent: hslCss(h, 60, 38), soft: hslCss(h, 60, 38, 0.16) };
	}
	return { accent: hslCss(h, 65, 72), soft: hslCss(h, 65, 72, 0.22) };
}

/** Paint the workspace scheme on `:root`. Both palettes are written
 * at once — the dark set to the plain token names, the light set to
 * their `*-light` twins, and the stylesheet's `:root.light` block
 * re-points the plain names at the twins. `workspaceId` is the
 * fallback source when `color` is null or unparseable (mirrors the
 * window icon's behaviour). */
export function applyWorkspaceScheme(workspaceId: string | null, color: string | null): void {
	if (workspaceId === null) {
		return;
	}
	const rgb = color === null ? null : parseHexColor(color);
	const h = Math.round(rgbToHsl(rgb ?? parseHexColor(defaultWorkspaceColor(workspaceId)) ?? { r: 0, g: 0, b: 0 }).h);
	const style = document.documentElement.style;
	for (const [name, value] of Object.entries(buildDark(h))) {
		style.setProperty(name, value);
	}
	for (const [name, value] of Object.entries(buildLight(h))) {
		style.setProperty(`${name}-light`, value);
	}
}
