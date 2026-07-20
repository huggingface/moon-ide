// Generate the companion PWA's icons — a crescent moon on the app's
// dark background — as PNGs, with zero native dependencies (pure
// pixel math + Node's zlib for the PNG encode). Re-run after
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

// Palette (matches companion/src/styles.css).
const BG = [0x0d, 0x11, 0x17];
const MOON = [0x2f, 0x81, 0xf7]; // --accent
const MOON_HI = [0xa5, 0xc8, 0xff]; // highlight tint for the rim

/** Alpha-blended crescent-moon coverage for a pixel, supersampled
 * 4x4. `u`,`v` are the pixel's coordinates in icon space scaled to a
 * unit square (0..1); the artwork occupies the circle centred on
 * (0.5, 0.5) with radius `scale/2`. Returns 0..1 coverage of the
 * crescent (disc minus an offset "bite" disc). */
function crescentCoverage(u, v, scale) {
	const cx = 0.5;
	const cy = 0.5;
	const r = 0.34 * scale;
	// The bite disc: up and to the right, slightly smaller.
	const bx = cx + 0.16 * scale;
	const by = cy - 0.14 * scale;
	const br = 0.3 * scale;
	let hit = 0;
	for (let sy = 0; sy < 4; sy++) {
		for (let sx = 0; sx < 4; sx++) {
			const x = u + (sx + 0.5) / 4 - 0.5;
			const y = v + (sy + 0.5) / 4 - 0.5;
			const inMoon = (x - cx) ** 2 + (y - cy) ** 2 <= r * r;
			const inBite = (x - bx) ** 2 + (y - by) ** 2 <= br * br;
			if (inMoon && !inBite) {
				hit++;
			}
		}
	}
	return hit / 16;
}

/** Rounded-square coverage (for the non-maskable launcher icons),
 * same 4x4 supersample. `radius` is the corner radius as a fraction
 * of the icon size. */
function roundedSquareCoverage(u, v, radius) {
	let hit = 0;
	for (let sy = 0; sy < 4; sy++) {
		for (let sx = 0; sx < 4; sx++) {
			const x = u + (sx + 0.5) / 4 - 0.5;
			const y = v + (sy + 0.5) / 4 - 0.5;
			const dx = Math.max(radius - x, x - (1 - radius), 0);
			const dy = Math.max(radius - y, y - (1 - radius), 0);
			if (dx * dx + dy * dy <= radius * radius) {
				hit++;
			}
		}
	}
	return hit / 16;
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
			const u = (xPix + 0.5) / size;
			const v = (yPix + 0.5) / size;
			const shape = opaque ? 1 : roundedSquareCoverage(u, v, 0.22);
			const moon = crescentCoverage(u, v, scale);
			// Vertical tint: the moon is brighter at the top rim.
			const t = Math.max(0, Math.min(1, 1.4 - 2 * v));
			const mr = MOON[0] + (MOON_HI[0] - MOON[0]) * t * 0.35;
			const mg = MOON[1] + (MOON_HI[1] - MOON[1]) * t * 0.35;
			const mb = MOON[2] + (MOON_HI[2] - MOON[2]) * t * 0.35;
			const r = BG[0] + (mr - BG[0]) * moon;
			const g = BG[1] + (mg - BG[1]) * moon;
			const b = BG[2] + (mb - BG[2]) * moon;
			const i = (yPix * size + xPix) * 4;
			px[i] = Math.round(r);
			px[i + 1] = Math.round(g);
			px[i + 2] = Math.round(b);
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
