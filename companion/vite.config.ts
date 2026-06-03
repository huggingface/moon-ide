import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The companion PWA is a separate Vite app from the desktop IDE
// (root `vite.config.ts`). It builds to `companion/dist`, which
// `moon-bridge serve --web-root` serves over HTTPS. No Tauri here —
// the transport is WSS to the bridge, not `invoke`.
export default defineConfig({
	root: __dirname,
	plugins: [svelte()],
	build: {
		outDir: 'dist',
		emptyOutDir: true,
		// One small app; a single chunk keeps the bridge's static
		// serving trivial and the cold load fast on a phone. Targeting
		// recent Safari/Chrome is fine — the audience is the team's own
		// phones, not the long tail of old browsers. es2024 is the
		// highest the toolchain (TS 5.9 lib names) maps cleanly onto.
		target: 'es2024',
	},
});
