import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { fileURLToPath } from 'node:url';

const TAURI_DEV_HOST = process.env['TAURI_DEV_HOST'];

export default defineConfig(async () => ({
	plugins: [svelte()],

	resolve: {
		alias: {
			$lib: fileURLToPath(new URL('./src/lib', import.meta.url)),
		},
	},

	// Vite expects strict ports / specific host for Tauri's window.
	clearScreen: false,
	server: {
		port: 1420,
		strictPort: true,
		host: TAURI_DEV_HOST ?? false,
		hmr: TAURI_DEV_HOST
			? {
					protocol: 'ws',
					host: TAURI_DEV_HOST,
					port: 1421,
				}
			: undefined,
		watch: {
			// Tauri's Rust files don't need to invalidate frontend modules.
			ignored: ['**/src-tauri/**', '**/target/**', '**/crates/**'],
		},
	},

	envPrefix: ['VITE_', 'TAURI_ENV_*'],

	build: {
		// Tauri 2 ships with modern WebKitGTK / WebView2 / WKWebView. Picking
		// recent targets means oxc/vite don't have to down-level ES2024 features.
		target: process.env['TAURI_ENV_PLATFORM'] === 'windows' ? 'chrome120' : 'safari17',
		// Vite 8 defaults to Oxc-based minification. Disable for tauri debug builds
		// so source mapping back to TS is one-to-one.
		minify: !process.env['TAURI_ENV_DEBUG'],
		sourcemap: !!process.env['TAURI_ENV_DEBUG'],
		// We're packaged inside Tauri and served from the local filesystem;
		// there's no network cost for a larger main chunk. CodeMirror core
		// alone is sizeable and splitting it deeper would be a real refactor
		// for no real benefit. 1.5 MB is a sane ceiling for an IDE bundle.
		chunkSizeWarningLimit: 1500,
	},
}));
