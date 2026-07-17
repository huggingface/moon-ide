import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { fileURLToPath } from 'node:url';

const TAURI_DEV_HOST = process.env['TAURI_DEV_HOST'];

export default defineConfig(async ({ command }) => ({
	plugins: [
		svelte({
			// In dev, inline component styles into the JS module instead
			// of emitting a separate `?svelte&type=style&lang.css` virtual
			// module. Sidesteps a known race in vite-plugin-svelte where
			// the browser parallel-fetches JS and virtual CSS and the CSS
			// request occasionally lands at Vite before the JS transform
			// has been cached, causing "failed to load virtual css module"
			// and unstyled UI on launch under Tauri/WebKitGTK.
			// (sveltejs/vite-plugin-svelte#1032, tauri-apps/tauri#10173.)
			// Build keeps the default — production benefits from external
			// CSS extraction, and the race is dev-server-only.
			emitCss: command !== 'serve',
		}),
		// WebKitGTK persists ~/.local/share/moon-ide/WebKitCache
		// across launches and revalidates lazily. After config changes that
		// alter what modules a Svelte component imports (e.g. flipping
		// `emitCss`), stale cached JS resurrects requests for modules that
		// no longer exist, which Vite logs as "failed to load virtual css
		// module" warnings. `Cache-Control: no-store` tells WebKit not to
		// keep dev responses on disk at all, so each launch gets a fresh
		// view of whatever Vite is currently serving. Vite's defaults send
		// `no-cache`, which still allows on-disk storage subject to
		// revalidation — not what we want for a dev server.
		//
		// We override `setHeader` instead of pre/post middleware because
		// Vite's transformMiddleware stamps `Cache-Control: no-cache` on
		// its own responses *after* route middlewares run, so a plain
		// `res.setHeader('Cache-Control', 'no-store')` would just be
		// overwritten. Intercepting `setHeader` itself rewrites the value
		// at the moment Vite tries to set it.
		{
			name: 'moon-ide:no-store-in-dev',
			apply: 'serve',
			configureServer(server) {
				server.middlewares.use((_req, res, next) => {
					const original = res.setHeader.bind(res);
					res.setHeader = function (name, value) {
						if (typeof name === 'string' && name.toLowerCase() === 'cache-control') {
							return original(name, 'no-store');
						}
						return original(name, value);
					};
					next();
				});
			},
		},
	],

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
		// Always emit source maps, release builds included: a production
		// stack trace like `index-BnGGoVCf.js:2:6013` is undebuggable
		// without them, and the .map files only cost binary size (the
		// webview fetches them lazily when devtools open, never at
		// normal runtime).
		sourcemap: true,
		// We're packaged inside Tauri and served from the local filesystem;
		// there's no network cost for a larger main chunk. CodeMirror core
		// alone is sizeable and splitting it deeper would be a real refactor
		// for no real benefit. 2 MB is a sane ceiling for an IDE bundle.
		chunkSizeWarningLimit: 2000,
	},
}));
