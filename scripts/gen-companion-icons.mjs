// Generate the companion PWA's PNG icons from the artwork. The
// canonical artwork is companion/artwork/icon.svg; this script
// rasterizes that exact geometry (same coordinates, same palette)
// with 4x4 supersampling and a dependency-free PNG encoder, because
// this repo's tooling stays zero-native-dependency. Re-run after
// changing the artwork:
//
//     node scripts/gen-companion-icons.mjs
//
// Outputs into companion/public/:
//   icon-192.png            launcher icon (rounded corners baked in)
//   icon-512.png            launcher icon, large
//   icon-maskable-512.png   full-bleed square for Android maskable
//                           masks (artwork inside the 80% safe zone)
//   apple-touch-icon.png    180px opaque square (iOS bakes its own
//                           corner radius)

import { deflateSync } from 'node:zlib';
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT_DIR = join(dirname(fileURLToPath(import.meta.url)), '..', 'companion', 'public');

// Palette (matches companion/src/styles.css). Everything below
// works in the SVG's 512x512 coordinate space.
const BG_TOP = [0x14, 0x1b, 0x26];
const BG_BOT = [0x0d, 0x11, 0x17];
const MOON_TOP = [0x9b, 0xc0, 0xff];
const MOON_MID = [0x5b, 0x9d, 0xff];
const MOON_BOT = [0x2f, 0x81, 0xf7]; // --accent
const STAR = [0xe6, 0xed, 0xf3];
const SPARK = [0xa5, 0xc8, 0xff];
const RING_COLOR = [0x58, 0xa6, 0xff];

// Crescent: disc minus an offset bite (the SVG path is exactly this
// boolean).
const MOON = { cx: 256, cy: 256, r: 164 };
const BITE = { cx: 350, cy: 162, r: 216 };
// Orbit: ellipse rx=207 ry=88 centred (256, 256), rotated -26 deg,
// 5px stroke at 35% opacity, drawn with a 0.02-on / 8-off dash that
// reads as a soft stipple; we fold the dash duty factor (~40%) into
// the alpha instead of tracking the path parameter.
const RING = { cx: 256, cy: 256, rx: 207, ry: 88, rot: (-26 * Math.PI) / 180, stroke: 5, alpha: 0.35 * 0.4 };
// Spark: 4-point compass star centred (352, 139), arm radius 53,
// waist half-width 12 at the centre.
const SPARK_STAR = { cx: 352, cy: 139, arm: 53, waist: 12 };
const BG_STARS = [
	[437, 88, 2.6, 0.5],
	[466, 201, 2.2, 0.45],
	[392, 48, 1.9, 0.4],
	[58, 332, 2.4, 0.5],
	[85, 429, 2.0, 0.45],
	[30, 247, 1.8, 0.35],
	[475, 330, 1.9, 0.35],
];
// Baked corner radius for the non-maskable launcher icons.
const CORNER_RADIUS = 112;

/** 4x4 sub-sample positions inside a pixel whose top-left corner is
 * (x, y). */
function subSamples(x, y) {
	const pts = [];
	for (let sy = 0; sy < 4; sy++) {
		for (let sx = 0; sx < 4; sx++) {
			pts.push([x + (sx + 0.5) / 4, y + (sy + 0.5) / 4]);
		}
	}
	return pts;
}

function coverage(x, y, inside) {
	let hit = 0;
	for (const [px, py] of subSamples(x, y)) {
		if (inside(px, py)) {
			hit++;
		}
	}
	return hit / 16;
}

/** Crescent coverage: moon disc minus bite disc. */
function crescentCoverage(x, y) {
	return coverage(x, y, (px, py) => {
		const inMoon = (px - MOON.cx) ** 2 + (py - MOON.cy) ** 2 <= MOON.r * MOON.r;
		const inBite = (px - BITE.cx) ** 2 + (py - BITE.cy) ** 2 <= BITE.r * BITE.r;
		return inMoon && !inBite;
	});
}

/** Rotated-ellipse ring coverage: a sample is on the stroke when
 * its distance to the ellipse curve is within half the stroke
 * width. The curve distance is approximated by the radial residual
 * after scaling into the unit circle — fine for a thin stroke. */
function ringCoverage(x, y) {
	const cos = Math.cos(RING.rot);
	const sin = Math.sin(RING.rot);
	const meanR = (RING.rx + RING.ry) / 2;
	return coverage(x, y, (px, py) => {
		const dx = px - RING.cx;
		const dy = py - RING.cy;
		const ex = dx * cos + dy * sin;
		const ey = -dx * sin + dy * cos;
		const d = Math.abs(Math.hypot(ex / RING.rx, ey / RING.ry) - 1) * meanR;
		return d <= RING.stroke / 2;
	});
}

/** 4-point star coverage: the SVG polygon is a rhombus in the
 * (max(|dx|,|dy|), min(|dx|,|dy|)) frame — inside when
 * a + b * (arm/waist - 1) <= arm. */
function sparkCoverage(x, y) {
	const k = SPARK_STAR.arm / SPARK_STAR.waist - 1;
	return coverage(x, y, (px, py) => {
		const a = Math.max(Math.abs(px - SPARK_STAR.cx), Math.abs(py - SPARK_STAR.cy));
		const b = Math.min(Math.abs(px - SPARK_STAR.cx), Math.abs(py - SPARK_STAR.cy));
		return a + b * k <= SPARK_STAR.arm;
	});
}

/** Rounded-square coverage (non-maskable launcher icons bake their
 * corners), in the 512 artwork space. */
function roundedSquareCoverage(x, y) {
	return coverage(x, y, (px, py) => {
		const dx = Math.max(CORNER_RADIUS - px, px - (512 - CORNER_RADIUS), 0);
		const dy = Math.max(CORNER_RADIUS - py, py - (512 - CORNER_RADIUS), 0);
		return dx * dx + dy * dy <= CORNER_RADIUS * CORNER_RADIUS;
	});
}

/** Two-stop vertical gradient (t in 0..1, 0 = top). */
function gradient(top, bot, t) {
	const k = Math.max(0, Math.min(1, t));
	return [top[0] + (bot[0] - top[0]) * k, top[1] + (bot[1] - top[1]) * k, top[2] + (bot[2] - top[2]) * k];
}

/** Moon gradient: vertical with the SVG's midpoint stop. */
function moonGradient(t) {
	if (t <= 0.55) {
		return gradient(MOON_TOP, MOON_MID, t / 0.55);
	}
	return gradient(MOON_MID, MOON_BOT, (t - 0.55) / 0.45);
}

/** Layer `src` over `dst` with coverage and alpha. */
function blend(dst, src, cov, alpha) {
	const a = cov * alpha;
	return [dst[0] + (src[0] - dst[0]) * a, dst[1] + (src[1] - dst[1]) * a, dst[2] + (src[2] - dst[2]) * a];
}

/** Render one icon into an RGBA buffer.
 * - `opaque`: fill the whole square (maskable / apple-touch);
 *   otherwise bake rounded corners with transparency outside.
 * - `scale`: artwork scale (maskable icons shrink into the 80%
 *   safe zone). */
function render(size, { opaque, scale }) {
	const px = new Uint8Array(size * size * 4);
	for (let yPix = 0; yPix < size; yPix++) {
		for (let xPix = 0; xPix < size; xPix++) {
			// This pixel's top-left corner in the 512 artwork space.
			const ax = 256 + ((xPix + 0.5) / size - 0.5) * (512 / scale);
			const ay = 256 + ((yPix + 0.5) / size - 0.5) * (512 / scale);
			const t = ((yPix + 0.5) / size - 0.5) / scale + 0.5; // gradient position, 0..1 over the artwork

			let c = gradient(BG_TOP, BG_BOT, t);
			for (const [sx, sy, sr, sa] of BG_STARS) {
				c = blend(
					c,
					STAR,
					coverage(ax, ay, (px2, py2) => (px2 - sx) ** 2 + (py2 - sy) ** 2 <= sr * sr),
					sa,
				);
			}
			c = blend(c, RING_COLOR, ringCoverage(ax, ay), RING.alpha);
			const moon = crescentCoverage(ax, ay);
			if (moon > 0) {
				c = blend(c, moonGradient(t), moon, 1);
			}
			c = blend(c, SPARK, sparkCoverage(ax, ay), 1);

			const shape = opaque ? 1 : roundedSquareCoverage(ax, ay);
			const i = (yPix * size + xPix) * 4;
			px[i] = Math.round(c[0]);
			px[i + 1] = Math.round(c[1]);
			px[i + 2] = Math.round(c[2]);
			px[i + 3] = Math.round(shape * 255);
		}
	}
	return px;
}

// --- Minimal PNG encoder (8-bit RGBA, no interlace). ---

function crc32(buf) {
	let c = 0xffffffff;
	for (let i = 0; i < buf.length; i++) {
		c ^= buf[i];
		for (let k = 0; k < 8; k++) {
			c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
		}
	}
	return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
	const out = Buffer.alloc(8 + data.length + 4);
	out.writeUInt32BE(data.length, 0);
	out.write(type, 4, 'ascii');
	data.copy(out, 8);
	out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
	return out;
}

function encodePng(px, size) {
	const ihdr = Buffer.alloc(13);
	ihdr.writeUInt32BE(size, 0);
	ihdr.writeUInt32BE(size, 4);
	ihdr[8] = 8; // bit depth
	ihdr[9] = 6; // color type RGBA
	// Raw scanlines, each prefixed with filter byte 0.
	const raw = Buffer.alloc(size * (size * 4 + 1));
	for (let y = 0; y < size; y++) {
		raw[y * (size * 4 + 1)] = 0;
		Buffer.from(px.buffer, y * size * 4, size * 4).copy(raw, y * (size * 4 + 1) + 1);
	}
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk('IHDR', ihdr),
		chunk('IDAT', deflateSync(raw, { level: 9 })),
		chunk('IEND', Buffer.alloc(0)),
	]);
}

mkdirSync(OUT_DIR, { recursive: true });
const targets = [
	{ name: 'icon-192.png', size: 192, opaque: false, scale: 1 },
	{ name: 'icon-512.png', size: 512, opaque: false, scale: 1 },
	{ name: 'icon-maskable-512.png', size: 512, opaque: true, scale: 0.8 },
	{ name: 'apple-touch-icon.png', size: 180, opaque: true, scale: 1 },
];
for (const { name, size, opaque, scale } of targets) {
	const png = encodePng(render(size, { opaque, scale }), size);
	writeFileSync(join(OUT_DIR, name), png);
	console.log(`${name}  ${size}x${size}  ${png.length} bytes`);
}
