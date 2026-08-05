import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { execSync } from 'node:child_process';

/** Build stamp baked into the bundle (`__BUILD_INFO__`), logged on
 * startup so a phone can verify which build it's actually running —
 * service-worker + browser caches make "did my deploy land?" a real
 * question. Best-effort: an environment without git still builds. */
function buildInfo(): string {
	let sha = 'unknown';
	try {
		sha = execSync('git rev-parse --short HEAD', { cwd: __dirname }).toString().trim();
		const dirty = execSync('git status --porcelain', { cwd: __dirname }).toString().trim().length > 0;
		if (dirty) {
			sha += '-dirty';
		}
	} catch {
		// No git (tarball build): keep 'unknown'.
	}
	return `${sha} ${new Date().toISOString()}`;
}

// The companion PWA is a separate Vite app from the desktop IDE
// (root `vite.config.ts`). It builds to `companion/dist`, which
// `moon-bridge serve --web-root` serves over HTTPS. No Tauri here —
// the transport is WSS to the bridge, not `invoke`.
export default defineConfig({
	root: __dirname,
	plugins: [svelte()],
	define: {
		__BUILD_INFO__: JSON.stringify(buildInfo()),
	},
	build: {
		outDir: 'dist',
		emptyOutDir: true,
		// One small app; a single chunk keeps the bridge's static
		// serving trivial and the cold load fast on a phone. Targeting
		// recent Safari/Chrome is fine — the audience is the team's own
		// phones, not the long tail of old browsers. es2024 is the
		// highest the toolchain (TS 5.9 lib names) maps cleanly onto.
		target: 'es2024',
		// Ship sourcemaps: the PWA is team-internal, so the small size
		// cost is worth real stack traces when the phone hits a bug
		// (the alternative is squinting at a minified single chunk).
		sourcemap: true,
	},
});
